//! v0.6.5 TRAINED EYES — GeeTest v3 icon-click (文字点选) solver.
//!
//! Two tiny ONNX models, 100% local CPU inference via the RTEN runtime
//! (same engine as ocrs — no C deps, no API):
//!   - `yolov8s.onnx` (44MB): detects char boxes. Trained with 2
//!     classes: "small" (<35px = the instruction strip, left-to-right
//!     = click order) vs "big" (the field chars on the photo).
//!   - `siamese.onnx` (17MB): similarity net. Compares each strip
//!     glyph against each field glyph and emits the click order.
//!
//! ~0.5s total on CPU. Verified 5/5 in-browser runs (2/2 on the real
//! passport.bilibili.com login, "Verification Succeeded").
//!
//! Models ship under `$GHOSTFOX_HOME/models/geetest_click/` and
//! auto-download on first use. AGPL-3.0, attributed:
//! ravizhan/geetest-v3-click-crack (see tools/geetest-click/README.md).

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use image::imageops::FilterType;
use rten::Model;
use rten_tensor::Layout as _;

const YOLO_URL: &str = "https://github.com/ras-indo/ghostfox/releases/download/v0.7.3/yolov8s.onnx";
const SIAMESE_URL: &str =
    "https://github.com/ras-indo/ghostfox/releases/download/v0.7.3/siamese_float.onnx";
/// The siamese ships dequantized (`siamese_float.onnx`) because the upstream
/// model is dynamic-quantized (DynamicQuantizeLinear/ConvInteger/MatMulInteger)
/// which rten's ONNX importer mishandles. Regenerate with
/// `tools/geetest-click/dequant_siamese.py`.
const CONF: f32 = 0.8;
const IOU_THRESH: f32 = 0.8;
const SIAMESE_THRESHOLD: f32 = 0.1;
const YOLO_IN: u32 = 384;
const SIAMESE_IN: u32 = 105;

fn models_dir() -> PathBuf {
    let home = std::env::var("GHOSTFOX_HOME").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|d| d.join(".ghostfox"))
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".ghostfox".into())
    });
    PathBuf::from(home).join("models").join("geetest_click")
}

async fn download(url: &str, to: &PathBuf) -> Result<()> {
    let tmp = to.with_extension("part");
    let resp = reqwest::get(url)
        .await
        .map_err(|e| anyhow!("model download failed ({url}): {e}"))?;
    // A 404 body must never be written to disk: the `exists()` guard below
    // would cache the corrupt file forever.
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("model download failed ({url}): HTTP {status}"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("model download read failed ({url}): {e}"))?;
    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, to)
        .await
        .with_context(|| format!("moving {} -> {}", tmp.display(), to.display()))?;
    tracing::info!(target: "ghostcloak::mcp", "downloaded geetest model {} ({} KB)", to.display(), bytes.len() / 1024);
    Ok(())
}

async fn ensure_models() -> Result<(PathBuf, PathBuf)> {
    let dir = models_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let yolo = dir.join("yolov8s.onnx");
    let siam = dir.join("siamese_float.onnx");
    if !yolo.exists() {
        download(YOLO_URL, &yolo).await?;
    }
    if !siam.exists() {
        download(SIAMESE_URL, &siam).await?;
    }
    Ok((yolo, siam))
}

struct GtModels {
    yolo: Model,
    siamese: std::sync::Mutex<Model>,
}

static MODELS: OnceLock<GtModels> = OnceLock::new();

async fn models() -> Result<&'static GtModels> {
    if let Some(m) = MODELS.get() {
        return Ok(m);
    }
    let (yolo_path, siam_path) = ensure_models().await?;
    let yolo_bytes = tokio::fs::read(&yolo_path).await?;
    let siam_bytes = tokio::fs::read(&siam_path).await?;
    let m = GtModels {
        yolo: Model::load(yolo_bytes).context("loading yolov8s.onnx")?,
        siamese: std::sync::Mutex::new(Model::load(siam_bytes).context("loading siamese.onnx")?),
    };
    Ok(MODELS.get_or_init(|| m))
}

/// Box = (x, y, w, h, score). All in ORIGINAL image pixel coords.
#[derive(Debug, Clone)]
struct Box {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    score: f32,
}

fn iou(a: &Box, b: &Box) -> f32 {
    let ax2 = a.x + a.w;
    let ay2 = a.y + a.h;
    let bx2 = b.x + b.w;
    let by2 = b.y + b.h;
    let ix = (ax2.min(bx2) - a.x.max(b.x)).max(0.0);
    let iy = (ay2.min(by2) - a.y.max(b.y)).max(0.0);
    let inter = ix * iy;
    let union = a.w * a.h + b.w * b.h - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Greedy NMS (same semantics as cv2.dnn.NMSBoxes).
fn nms(mut boxes: Vec<Box>) -> Vec<Box> {
    boxes.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept: Vec<Box> = Vec::new();
    for b in boxes {
        if kept.iter().all(|k| iou(k, &b) < IOU_THRESH) {
            kept.push(b);
        }
    }
    kept
}

/// CHW float32 value from RGB8 pixels, normalized to [0, 1].
fn to_value(rgb: &image::RgbImage) -> rten::Value {
    let (w, h) = rgb.dimensions();
    let pixels = rgb.as_raw();
    let mut chw = vec![0f32; (3 * h * w) as usize];
    for (i, px) in pixels.chunks(3).enumerate() {
        let x = i % w as usize;
        let y = i / w as usize;
        let base = (h * w) as usize;
        chw[0 * base + y * w as usize + x] = px[0] as f32 / 255.0;
        chw[1 * base + y * w as usize + x] = px[1] as f32 / 255.0;
        chw[2 * base + y * w as usize + x] = px[2] as f32 / 255.0;
    }
    rten::Value::from_shape(&[1usize, 3, h as usize, w as usize], chw).expect("tensor build")
}

/// Detect char boxes: returns (smalls, bigs) where smalls are the
/// instruction-strip glyphs (keyed by left x, sort = click order) and
/// bigs are the field chars.
fn detect(
    models: &GtModels,
    img: &image::RgbImage,
) -> Result<(Vec<(i32, image::RgbImage)>, Vec<Box>)> {
    let (ow, oh) = img.dimensions();
    let resized = image::imageops::resize(img, YOLO_IN, YOLO_IN, FilterType::Triangle);
    let input = to_value(&resized);
    let in_id = models.yolo.input_ids()[0];
    let out_id = models.yolo.output_ids()[0];
    let outputs = models
        .yolo
        .run(vec![(in_id, input.into())], &[out_id], None)
        .context("yolo inference")?;
    let out = outputs[0].as_view();
    let rten::ValueView::FloatTensor(tv) = &out else {
        return Err(anyhow!("yolo output is not a float tensor"));
    };
    let n = tv.size(2);
    let data = tv.data().context("yolo output data")?;

    let xf = ow as f32 / YOLO_IN as f32;
    let yf = oh as f32 / YOLO_IN as f32;

    let mut boxes: Vec<Box> = Vec::new();
    for i in 0..n {
        let x = data[i];
        let y = data[n + i];
        let w = data[2 * n + i];
        let h = data[3 * n + i];
        let c0 = data[4 * n + i];
        let c1 = data[5 * n + i];
        let score = c0.max(c1);
        if score < CONF {
            continue;
        }
        boxes.push(Box {
            x: (x - w / 2.0) * xf,
            y: (y - h / 2.0) * yf,
            w: w * xf,
            h: h * yf,
            score,
        });
    }
    let boxes = nms(boxes);

    let mut smalls: Vec<(i32, image::RgbImage)> = Vec::new();
    let mut bigs: Vec<Box> = Vec::new();
    for b in boxes {
        let crop = image::imageops::crop_imm(
            img,
            b.x.max(0.0) as u32,
            b.y.max(0.0) as u32,
            b.w.min(ow as f32 - b.x.max(0.0)) as u32,
            b.h.min(oh as f32 - b.y.max(0.0)) as u32,
        )
        .to_image();
        if crop.width() < 35 && crop.height() < 35 {
            smalls.push((b.x as i32, crop));
        } else {
            bigs.push(b);
        }
    }
    smalls.sort_by_key(|(x, _)| *x);
    Ok((smalls, bigs))
}

fn siamese_prep(crop: &image::RgbImage) -> rten::Value {
    let resized = image::imageops::resize(crop, SIAMESE_IN, SIAMESE_IN, FilterType::Triangle);
    to_value(&resized)
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// Solve a challenge image (the raw composite: field + strip at the
/// bottom). Returns field-char box top-lefts `[[x, y], ...]` in the
/// image's pixel coordinates, IN CLICK ORDER.
pub async fn solve_image(img_bytes: &[u8]) -> Result<Vec<[i32; 2]>> {
    let models = models().await?;
    let img = image::load_from_memory(img_bytes).context("decoding challenge image")?;
    let rgb = img.to_rgb8();
    let (small_glyphs, big_boxes) = detect(models, &rgb)?;

    if small_glyphs.is_empty() || big_boxes.is_empty() {
        return Err(anyhow!(
            "detection found {} strip glyphs / {} field chars (need >= 1 each)",
            small_glyphs.len(),
            big_boxes.len()
        ));
    }

    let mut result: Vec<[i32; 2]> = Vec::new();
    for (_x, glyph) in &small_glyphs {
        let d1 = siamese_prep(glyph);
        for b in &big_boxes {
            if result.contains(&[b.x as i32, b.y as i32]) {
                continue;
            }
            let crop = image::imageops::crop_imm(
                &rgb,
                b.x.max(0.0) as u32,
                b.y.max(0.0) as u32,
                b.w.min(rgb.width() as f32 - b.x.max(0.0)) as u32,
                b.h.min(rgb.height() as f32 - b.y.max(0.0)) as u32,
            )
            .to_image();
            let d2 = siamese_prep(&crop);
            let mut session = models
                .siamese
                .lock()
                .map_err(|_| anyhow!("siamese session poisoned"))?;
            let in1 = session.node_id("input").context("siamese input node")?;
            let in2 = session.node_id("input.53").context("siamese input2 node")?;
            let out_id = session.output_ids()[0];
            let out = session
                .run(
                    vec![(in1, d1.as_view().into()), (in2, d2.into())],
                    &[out_id],
                    None,
                )
                .context("siamese inference")?;
            drop(session);
            let v = out[0].as_view();
            let sim = match &v {
                rten::ValueView::FloatTensor(tv) => {
                    tv.data().and_then(|d| d.first().copied()).unwrap_or(0.0)
                }
                _ => 0.0,
            };
            if sigmoid(sim) >= SIAMESE_THRESHOLD {
                result.push([b.x as i32, b.y as i32]);
                break;
            }
        }
    }
    Ok(result)
}

/// Public similarity helper: two PNG crops -> siamese sigmoid score
/// (1.0 = identical glyph family). Used by the hCaptcha solver for
/// odd-one-out comparisons: the icon with the LOWEST average
/// similarity to the others is "the different one".
pub async fn siamese_similarity(png1: &[u8], png2: &[u8]) -> Result<f32> {
    let m = models().await?;
    let img1 = image::load_from_memory(png1).context("sim decode 1")?;
    let img2 = image::load_from_memory(png2).context("sim decode 2")?;
    let d1 = siamese_prep(&img1.to_rgb8());
    let d2 = siamese_prep(&img2.to_rgb8());
    let mut session = m
        .siamese
        .lock()
        .map_err(|_| anyhow!("siamese session poisoned"))?;
    let in1 = session.node_id("input").context("siamese input node")?;
    let in2 = session.node_id("input.53").context("siamese input2 node")?;
    let out_id = session.output_ids()[0];
    let out = session
        .run(vec![(in1, d1.into()), (in2, d2.into())], &[out_id], None)
        .context("siamese inference")?;
    drop(session);
    let v = out[0].as_view();
    let raw = match &v {
        rten::ValueView::FloatTensor(tv) => {
            tv.data().and_then(|d| d.first().copied()).unwrap_or(0.0)
        }
        _ => 0.0,
    };
    Ok(1.0 / (1.0 + (-raw).exp()))
}

/// Binary closing: dilate then erode with an s x s square kernel.
fn binary_closing(mask: &mut [bool], w: usize, h: usize, s: usize) {
    let mut tmp = mask.to_vec();
    let r = (s / 2) as isize;
    // dilate
    for y in 0..h as isize {
        for x in 0..w as isize {
            if !mask[(y * w as isize + x) as usize] {
                for dy in -r..=r {
                    for dx in -r..=r {
                        let nx = x + dx;
                        let ny = y + dy;
                        if nx >= 0
                            && ny >= 0
                            && nx < w as isize
                            && ny < h as isize
                            && mask[(ny * w as isize + nx) as usize]
                        {
                            tmp[(y * w as isize + x) as usize] = true;
                        }
                    }
                }
            }
        }
    }
    let dil = tmp;
    tmp = vec![false; w * h];
    // erode
    for y in 0..h as isize {
        for x in 0..w as isize {
            let mut all = dil[(y * w as isize + x) as usize];
            for dy in -r..=r {
                for dx in -r..=r {
                    let nx = x + dx;
                    let ny = y + dy;
                    if nx >= 0 && ny >= 0 && nx < w as isize && ny < h as isize {
                        all &= dil[(ny * w as isize + nx) as usize];
                    } else {
                        all = false;
                    }
                }
            }
            tmp[(y * w as isize + x) as usize] = all;
        }
    }
    mask.copy_from_slice(&tmp);
}

/// Largest 4-connected component bounding box.
fn largest_blob(mask: &[bool], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let mut seen = vec![false; w * h];
    let mut best: Option<(usize, (usize, usize, usize, usize))> = None;
    for start in 0..(w * h) {
        if !mask[start] || seen[start] {
            continue;
        }
        let mut stack = vec![start];
        seen[start] = true;
        let (mut x0, mut y0, mut x1, mut y1, mut area) = (w, h, 0usize, 0usize, 0usize);
        while let Some(p) = stack.pop() {
            let x = p % w;
            let y = p / w;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
            area += 1;
            if x > 0 && mask[p - 1] && !seen[p - 1] {
                seen[p - 1] = true;
                stack.push(p - 1);
            }
            if x + 1 < w && mask[p + 1] && !seen[p + 1] {
                seen[p + 1] = true;
                stack.push(p + 1);
            }
            if p >= w && mask[p - w] && !seen[p - w] {
                seen[p - w] = true;
                stack.push(p - w);
            }
            if p + w < w * h && mask[p + w] && !seen[p + w] {
                seen[p + w] = true;
                stack.push(p + w);
            }
        }
        if best.as_ref().map_or(true, |(ba, _)| area > *ba) {
            best = Some((area, (x0, y0, x1, y1)));
        }
    }
    best.map(|(_, b)| b)
}

/// GeeTest v3 slide: find the drag distance.
/// bg = canvas bg (hole), sl = slice (piece), full = complete reference.
pub fn slide_gap(
    bg: &image::GrayImage,
    sl: &image::RgbaImage,
    full: &image::GrayImage,
) -> Result<usize> {
    let (w, h) = bg.dimensions();
    let (fw, fh) = full.dimensions();
    if (w, h) != (fw, fh) || w == 0 || h == 0 {
        return Err(anyhow!("canvas size mismatch"));
    }
    let mut diff_mask = vec![false; (w * h) as usize];
    let mut max_diff = 0f32;
    for i in 0..(w * h) as usize {
        let d = (bg.as_raw()[i] as f32 - full.as_raw()[i] as f32).abs();
        if d > max_diff {
            max_diff = d;
        }
        if d > 40.0 {
            diff_mask[i] = true;
        }
    }
    if max_diff < 10.0 {
        return Err(anyhow!(
            "bg and full canvas are identical — no hole to find"
        ));
    }
    binary_closing(&mut diff_mask, w as usize, h as usize, 5);
    let (hx0, _hy0, _hx1, _hy1) =
        largest_blob(&diff_mask, w as usize, h as usize).context("no diff blob found")?;
    // piece solid left edge: alpha > 128
    let (pw, ph) = sl.dimensions();
    let mut px0 = pw as usize;
    for y in 0..ph {
        for x in 0..pw {
            if sl.get_pixel(x, y).0[3] > 128 {
                px0 = px0.min(x as usize);
            }
        }
    }
    if px0 == pw as usize {
        return Err(anyhow!("slice has no solid pixels"));
    }
    Ok(hx0.saturating_sub(px0))
}

/// Resolve one axis of a CSS `background-size` token into a scale factor
/// (returned value * natural size = rendered size). `None` for tokens this
/// axis cannot interpret — the caller falls back to "fill the element".
fn bg_size_scale(tok: &str, elem: f64, natural: f64) -> Option<f64> {
    if natural <= 0.0 {
        return None;
    }
    let t = tok.trim();
    if let Some(v) = t.strip_suffix("px") {
        let px: f64 = v.trim().parse().ok()?;
        return Some(px / natural);
    }
    if let Some(v) = t.strip_suffix('%') {
        let pct: f64 = v.trim().parse().ok()?;
        if elem <= 0.0 {
            return None;
        }
        return Some(pct / 100.0 * elem / natural);
    }
    if t == "auto" {
        return Some(1.0);
    }
    None
}

/// Resolve one token of a CSS `background-position` into the scaled image's
/// origin offset relative to the element's origin (may be negative — that is
/// how a sprite sheet crops). Unknown tokens read as 0.
fn bg_pos_offset(tok: &str, elem: f64, scaled: f64) -> f64 {
    let t = tok.trim();
    if let Some(v) = t.strip_suffix("px") {
        return v.trim().parse::<f64>().unwrap_or(0.0);
    }
    if let Some(v) = t.strip_suffix('%') {
        let pct: f64 = v.trim().parse().unwrap_or(0.0);
        return pct / 100.0 * (elem - scaled);
    }
    match t {
        "left" | "top" => 0.0,
        "right" | "bottom" => elem - scaled,
        "center" => (elem - scaled) / 2.0,
        _ => 0.0,
    }
}

/// Render one CSS background layer — an image plus its element's rect and
/// computed `background-size`/`background-position` — into an ELEMENT-SIZED
/// RGBA image. GeeTest v4's adaptive mode draws the challenge with divs
/// (`.geetest_bg`, `.geetest_slice_bg`, `.geetest_fullbg`) that have no
/// canvas to read; this reproduces exactly what the element shows, in CSS
/// px, so downstream pixel math and the drag distance stay in CSS px like
/// the canvas path's `toDataURL` canvases.
pub fn render_bg_layer(
    img: &image::DynamicImage,
    elem_w: f64,
    elem_h: f64,
    bg_size: &str,
    bg_pos: &str,
) -> Result<image::RgbaImage> {
    let (iw, ih) = (img.width() as f64, img.height() as f64);
    if iw < 1.0 || ih < 1.0 || elem_w < 1.0 || elem_h < 1.0 {
        return Err(anyhow!(
            "degenerate background layer: {iw}x{ih} image in {elem_w}x{elem_h} element"
        ));
    }
    let mut toks = bg_size.split_whitespace().filter(|t| !t.is_empty());
    let first = toks.next().unwrap_or("auto");
    let second = toks.next();
    let (sx, sy) = if first == "cover" || first == "contain" {
        let s = if first == "cover" {
            (elem_w / iw).max(elem_h / ih)
        } else {
            (elem_w / iw).min(elem_h / ih)
        };
        (s, s)
    } else {
        let sx = bg_size_scale(first, elem_w, iw).unwrap_or(elem_w / iw);
        let sy = second
            .and_then(|t| bg_size_scale(t, elem_h, ih))
            .unwrap_or(sx);
        (sx, sy)
    };
    let (sw, sh) = (
        ((iw * sx).round() as u32).max(1),
        ((ih * sy).round() as u32).max(1),
    );
    let rgba = img.to_rgba8();
    let scaled = if sw == rgba.width() && sh == rgba.height() {
        rgba
    } else {
        image::imageops::resize(&rgba, sw, sh, FilterType::Triangle)
    };
    // background-position -> origin of the scaled image inside the element.
    let mut ptoks = bg_pos.split_whitespace().filter(|t| !t.is_empty());
    let (ox, oy) = match (ptoks.next(), ptoks.next()) {
        (None, _) => (0.0, 0.0),
        (Some(a), None) => match a {
            "top" => (0.0, 0.0),
            "bottom" => (0.0, elem_h - sh as f64),
            "center" => (
                (elem_w - sw as f64) / 2.0,
                (elem_h - sh as f64) / 2.0,
            ),
            _ => (bg_pos_offset(a, elem_w, sw as f64), 0.0),
        },
        (Some(a), Some(b)) => (
            bg_pos_offset(a, elem_w, sw as f64),
            bg_pos_offset(b, elem_h, sh as f64),
        ),
    };
    let (ew, eh) = (
        elem_w.round().max(1.0) as u32,
        elem_h.round().max(1.0) as u32,
    );
    let (oxi, oyi) = (ox.round() as i32, oy.round() as i32);
    let mut out = image::RgbaImage::new(ew, eh);
    for y in 0..eh {
        let src_y = y as i32 - oyi;
        if src_y < 0 || src_y >= sh as i32 {
            continue;
        }
        for x in 0..ew {
            let src_x = x as i32 - oxi;
            if src_x < 0 || src_x >= sw as i32 {
                continue;
            }
            out.put_pixel(x, y, *scaled.get_pixel(src_x as u32, src_y as u32));
        }
    }
    Ok(out)
}

/// luma minus its (2r+1)-box local mean, borders clamped. The box mean
/// absorbs the brightness step a hole's shadow lays across the patch — the
/// cue that makes plain-luma correlation drift toward a smooth wrong spot —
/// while the texture that identifies the hole survives untouched.
fn highpass(plane: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let (wr, hr) = (w as isize - 1, h as isize - 1);
    let mut hor = vec![0f32; w * h];
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let mut s = 0f32;
            for k in -(r as isize)..=(r as isize) {
                s += plane[row + (x as isize + k).clamp(0, wr) as usize];
            }
            hor[row + x] = s;
        }
    }
    let n = ((2 * r + 1) * (2 * r + 1)) as f32;
    let mut out = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut s = 0f32;
            for k in -(r as isize)..=(r as isize) {
                s += hor[((y as isize + k).clamp(0, hr) as usize) * w + x];
            }
            out[y * w + x] = plane[y * w + x] - s / n;
        }
    }
    out
}

/// One exhaustive normalized cross-correlation pass: score every `pw` x `ph`
/// window of `plane` against the compact patch (`pat`, sampled at `idxs`
/// inside the patch) and keep every score — the runner-up check needs them.
/// Returns `(scores, (best_score, best_dx, best_dy))`, dx fastest.
fn ncc_scan(
    plane: &[f32],
    bw: u32,
    bh: u32,
    pat: &[f64],
    idxs: &[usize],
    pw: u32,
    ph: u32,
    mean_p: f64,
    ss_p: f64,
) -> (Vec<f32>, (f32, u32, u32)) {
    let mf = pat.len() as f64;
    let (pw_usz, nx) = (pw as usize, (bw - pw + 1) as usize);
    let mut scores: Vec<f32> = Vec::with_capacity(nx * (bh - ph + 1) as usize);
    let mut best = (-1f32, 0u32, 0u32);
    for dy in 0..=(bh - ph) {
        for dx in 0..=(bw - pw) {
            let (mut sb, mut sbb, mut sab) = (0f64, 0f64, 0f64);
            for (c, &k) in idxs.iter().enumerate() {
                let i = (k % pw_usz) as u32;
                let j = (k / pw_usz) as u32;
                let b = plane[((dy + j) * bw + dx + i) as usize] as f64;
                sb += b;
                sbb += b * b;
                sab += b * pat[c];
            }
            let var_b = sbb - sb * sb / mf;
            let score = if var_b <= 1e-6 {
                -1f32 // featureless window — correlation undefined
            } else {
                // cov = sum (p - mean_p)(b - mean_b) = sab - mean_p * sb
                let cov = sab - sb * mean_p;
                (cov / (ss_p * var_b).sqrt()) as f32
            };
            scores.push(score);
            if score > best.0 {
                best = (score, dx, dy);
            }
        }
    }
    (scores, best)
}

/// GeeTest v4 (div layers) WITHOUT a fullbg reference: find the hole by
/// template-matching the piece into the bg. The piece is a crop of the
/// original scene and the hole only shadows it, so brightness-invariant
/// normalized cross-correlation (the same statistic as OpenCV's
/// `TM_CCOEFF_NORMED`, what ddddocr runs on this exact pair) peaks at the
/// hole. Only the piece's solid pixels (alpha > 128) are correlated — the
/// transparent corners of a puzzle piece are padding, not content.
///
/// The hole's shadow is a brightness STEP across the patch, and on a
/// low-texture bg that step can tilt plain-luma correlation toward a
/// smooth wrong spot, so a second shadow-blind pass (high-pass luma) has to
/// land on the same column before the answer is trusted; when the two
/// disagree the result is refused instead of dragged. An all-opaque slice
/// (no alpha) is correlated whole — it works when the slice is a clean
/// crop, and the corroboration turns the ambiguous cases into a refusal.
///
/// Returns `hole_left - piece_left`, the same convention as [`slide_gap`],
/// in background pixels. Errors when nothing clears the confidence floor:
/// a blind drag wastes the challenge (and the host's rate-limit budget),
/// an honest failure does not.
pub fn slide_gap_no_full(bg: &image::GrayImage, slice: &image::RgbaImage) -> Result<usize> {
    let (bw, bh) = bg.dimensions();
    let (sw, sh) = slice.dimensions();
    if bw == 0 || bh == 0 || sw == 0 || sh == 0 {
        return Err(anyhow!("empty background or slice image"));
    }
    // Solid bbox of the piece. An all-opaque slice (no alpha channel) is
    // its own bbox with x0 = 0, which reproduces slide_gap's convention.
    let (mut x0, mut y0, mut x1, mut y1) = (sw, sh, 0u32, 0u32);
    for y in 0..sh {
        for x in 0..sw {
            if slice.get_pixel(x, y).0[3] > 128 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 {
        return Err(anyhow!("slice has no solid pixels"));
    }
    let (pw, ph) = (x1 - x0 + 1, y1 - y0 + 1);
    if pw > bw || ph > bh {
        return Err(anyhow!(
            "slice ({pw}x{ph}) does not fit inside the background ({bw}x{bh})"
        ));
    }
    // Same luma conversion the bg went through, so both sides correlate in
    // one color space; the bg also becomes an f32 plane both scans read.
    let sl_luma = image::DynamicImage::ImageRgba8(slice.clone()).to_luma8();
    let bg_plane: Vec<f32> = bg.as_raw().iter().map(|&v| v as f32).collect();
    let mut idxs: Vec<usize> = Vec::new();
    let mut pat: Vec<f64> = Vec::new();
    for j in 0..ph {
        for i in 0..pw {
            if slice.get_pixel(x0 + i, y0 + j).0[3] > 128 {
                idxs.push((j * pw + i) as usize);
                pat.push(sl_luma.get_pixel(x0 + i, y0 + j)[0] as f64);
            }
        }
    }
    let m = pat.len();
    if m < 64 {
        return Err(anyhow!("slice has too few solid pixels ({m})"));
    }
    let mf = m as f64;
    let mean_p = pat.iter().sum::<f64>() / mf;
    let mut ss_p = 0f64;
    for &p in &pat {
        let d = p - mean_p;
        ss_p += d * d;
    }
    if ss_p < 1e-6 {
        return Err(anyhow!("slice patch is flat — nothing to correlate"));
    }
    let (scores, best) = ncc_scan(&bg_plane, bw, bh, &pat, &idxs, pw, ph, mean_p, ss_p);
    const NCC_FLOOR: f32 = 0.5;
    const PEAK_ZONE: u32 = 8;
    const PEAK_MARGIN: f32 = 0.05;
    if best.0 < NCC_FLOOR {
        return Err(anyhow!(
            "no reliable slice match (best ncc {:.2} < 0.50) — refusing to drag blind",
            best.0
        ));
    }
    let nx = (bw - pw + 1) as usize;
    let mut second = -1f32;
    for (n, &sc) in scores.iter().enumerate() {
        let dx = (n % nx) as u32;
        let dy = (n / nx) as u32;
        if dx.abs_diff(best.1) <= PEAK_ZONE && dy.abs_diff(best.2) <= PEAK_ZONE {
            continue;
        }
        if sc > second {
            second = sc;
        }
    }
    if best.0 - second < PEAK_MARGIN {
        return Err(anyhow!(
            "ambiguous slice match (best ncc {:.2} vs {:.2} elsewhere) — refusing to drag blind",
            best.0,
            second
        ));
    }
    // Corroboration. The hole's shadow is a piecewise brightness step across
    // the patch, which can tilt plain-luma correlation toward a smooth wrong
    // spot; the high-pass pass sees texture only, so both must land on the
    // same column. They disagree -> the answer is a guess, and a guess
    // spends the challenge.
    const HP_RADIUS: usize = 2;
    const HP_NCC_FLOOR: f32 = 0.15;
    const HP_COLUMN_TOL: u32 = 3;
    // Transparent pixels are padding: fill them with the patch mean so they
    // neither drag the local mean around nor enter the correlation.
    let mut pat_plane = vec![mean_p as f32; (pw * ph) as usize];
    for (c, &k) in idxs.iter().enumerate() {
        pat_plane[k] = pat[c] as f32;
    }
    let hp_plane = highpass(&pat_plane, pw as usize, ph as usize, HP_RADIUS);
    let mut hp_pat: Vec<f64> = Vec::with_capacity(m);
    for &k in &idxs {
        hp_pat.push(hp_plane[k] as f64);
    }
    let mean_hp = hp_pat.iter().sum::<f64>() / mf;
    let mut ss_hp = 0f64;
    for &p in &hp_pat {
        let d = p - mean_hp;
        ss_hp += d * d;
    }
    // A near-smooth piece has no texture for the check to judge by.
    if ss_hp >= 1e-6 {
        let hp_bg = highpass(&bg_plane, bw as usize, bh as usize, HP_RADIUS);
        let (_, hp_best) = ncc_scan(&hp_bg, bw, bh, &hp_pat, &idxs, pw, ph, mean_hp, ss_hp);
        if hp_best.0 >= HP_NCC_FLOOR && hp_best.1.abs_diff(best.1) > HP_COLUMN_TOL {
            return Err(anyhow!(
                "inconsistent slice match (luma x={} vs high-pass x={}) — \
                 refusing to drag blind",
                best.1,
                hp_best.1
            ));
        }
    }
    Ok(best.1.saturating_sub(x0) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn debug_solve_known_image() {
        // Debug-only test: requires a local fixture image that exists only
        // on developer machines. Skip when the fixture is absent (CI).
        let path = "/tmp/opencode/ga.png";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping debug_solve_known_image: {path} not present");
            return;
        }
        let bytes = std::fs::read(path).expect("read test image");
        let boxes = solve_image(&bytes).await.expect("solve");
        println!("RUST BOXES: {:?}", boxes);
        println!("EXPECTED:   [[55, 36], [205, 12], [161, 86], [147, 255]]");
    }
}

#[cfg(test)]
mod tests2 {
    use super::*;

    #[tokio::test]
    async fn debug_matrix() {
        // Debug-only test: requires a local fixture image that exists only
        // on developer machines. Skip when the fixture is absent (CI).
        let path = "/tmp/opencode/ga.png";
        if !std::path::Path::new(path).exists() {
            eprintln!("skipping debug_matrix: {path} not present");
            return;
        }
        let bytes = std::fs::read(path).expect("read test image");
        let m = models().await.expect("models");
        let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
        let (smalls, bigs) = detect(m, &img).expect("detect");
        println!(
            "RUST smalls x: {:?}",
            smalls
                .iter()
                .map(|(x, c)| (x, c.width(), c.height()))
                .collect::<Vec<_>>()
        );
        println!(
            "RUST bigs: {:?}",
            bigs.iter()
                .map(|b| (b.x, b.y, b.w, b.h))
                .collect::<Vec<_>>()
        );
        for (x, g) in &smalls {
            let d1 = siamese_prep(g);
            let mut row = Vec::new();
            for b in &bigs {
                let crop = image::imageops::crop_imm(
                    &img,
                    b.x.max(0.0) as u32,
                    b.y.max(0.0) as u32,
                    b.w.min(img.width() as f32 - b.x.max(0.0)) as u32,
                    b.h.min(img.height() as f32 - b.y.max(0.0)) as u32,
                )
                .to_image();
                let d2 = siamese_prep(&crop);
                let mut session = m.siamese.lock().unwrap();
                let in1 = session.node_id("input").unwrap();
                let in2 = session.node_id("input.53").unwrap();
                let out_id = session.output_ids()[0];
                let out = session
                    .run(
                        vec![(in1, d1.as_view().into()), (in2, d2.into())],
                        &[out_id],
                        None,
                    )
                    .unwrap();
                drop(session);
                let v = out[0].as_view();
                let sim = match &v {
                    rten::ValueView::FloatTensor(tv) => {
                        tv.data().and_then(|d| d.first().copied()).unwrap_or(0.0)
                    }
                    _ => 0.0,
                };
                row.push(format!("{:.3}", sigmoid(sim)));
            }
            println!("glyph@{}: {:?}", x, row);
        }
    }
}
