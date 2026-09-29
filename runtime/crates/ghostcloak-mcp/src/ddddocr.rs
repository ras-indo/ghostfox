//! v0.6.7 CAPTCHA OCR — ddddocr port into the runtime.
//!
//! The ddddocr model (CRNN + LSTM, 54MB, trained specifically on
//! captcha text) runs via onnxruntime (`ort`). The rten ONNX importer
//! has no LSTM mapping, so this one model rides on ort instead of
//! rten — everything else stays pure-Rust inference.
//!
//! Input: grayscale, resized to height 64 keeping aspect ratio
//! (LANCZOS), normalized to [0,1], CHW. Output: CTC — argmax per
//! timestep, skip blank (index 0), collapse repeats, map through the
//! 8210-char charset shipped in models/ddddocr/charset.txt.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use image::imageops::FilterType;
use ort::session::Session;

fn models_dir() -> PathBuf {
    let home = std::env::var("GHOSTFOX_HOME").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|d| d.join(".ghostfox"))
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".ghostfox".into())
    });
    PathBuf::from(home).join("models").join("ddddocr")
}

struct DdddOcr {
    session: std::sync::Mutex<Session>,
    charset: Vec<char>,
}

static ENGINE: OnceLock<DdddOcr> = OnceLock::new();

async fn download(url: &str, to: &PathBuf) -> Result<()> {
    let tmp = to.with_extension("part");
    let resp = reqwest::get(url)
        .await
        .map_err(|e| anyhow!("model download failed ({url}): {e}"))?;
    // A 404 body ("Not Found", 9 bytes) must never be written to disk:
    // the `exists()` guard below would then cache the corrupt file forever.
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!("model download failed ({url}): HTTP {status}"));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| anyhow!("read failed: {e}"))?;
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, to).await?;
    Ok(())
}

async fn ensure_files() -> Result<(PathBuf, PathBuf)> {
    let dir = models_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let model_path = dir.join("common.onnx");
    let charset_path = dir.join("charset.txt");
    if !model_path.exists() {
        download(
            "https://github.com/ras-indo/ghostfox/releases/download/v0.7.3/common.onnx",
            &model_path,
        )
        .await?;
    }
    if !charset_path.exists() {
        download(
            "https://github.com/ras-indo/ghostfox/releases/download/v0.7.3/charset.txt",
            &charset_path,
        )
        .await?;
    }
    Ok((model_path, charset_path))
}

async fn engine() -> Result<&'static DdddOcr> {
    if let Some(e) = ENGINE.get() {
        return Ok(e);
    }
    let (model_path, charset_path) = ensure_files().await?;
    let session = Session::builder()
        .context("creating ort session builder")?
        .commit_from_file(&model_path)
        .context("loading ddddocr common.onnx")?;
    let charset_text = std::fs::read_to_string(&charset_path).context("reading charset")?;
    let charset: Vec<char> = charset_text
        .lines()
        .map(|l| l.chars().next().unwrap_or(' '))
        .collect();
    Ok(ENGINE.get_or_init(|| DdddOcr {
        session: std::sync::Mutex::new(session),
        charset,
    }))
}

/// Classify captcha text from PNG bytes.
pub async fn classify_png(png: &[u8]) -> Result<String> {
    let eng = engine().await?;
    let img = image::load_from_memory(png).context("decoding captcha image")?;
    let (ow, oh) = (img.width(), img.height());
    if ow == 0 || oh == 0 {
        return Err(anyhow!("empty image"));
    }
    // height 64, keep aspect ratio (LANCZOS, same as ddddocr)
    let tw = std::cmp::max(1, ow * 64 / oh);
    let gray = img.to_luma8();
    let resized = image::imageops::resize(&gray, tw, 64, FilterType::Lanczos3);
    let (rw, rh) = (resized.width() as usize, resized.height() as usize);
    let mut data = vec![0f32; rw * rh];
    for (i, px) in resized.as_raw().iter().enumerate() {
        data[i] = *px as f32 / 255.0;
    }
    let input = ort::value::Tensor::from_array((vec![1usize, 1, rh, rw], data))
        .context("building input tensor")?;
    let mut session = eng
        .session
        .lock()
        .map_err(|_| anyhow!("ddddocr session lock poisoned"))?;
    let outputs = session
        .run(ort::inputs!["input1" => input])
        .context("running ddddocr model")?;
    // shape: [1, T, C]
    let (shape, flat) = outputs[0]
        .try_extract_tensor::<f32>()
        .context("extracting output tensor")?;
    let dims: Vec<usize> = shape.iter().map(|d| *d as usize).collect();
    // Output may be [1, T, C] (batch-major) or [T, 1, C] (sequence-major);
    // reading batch dim as T decodes a single timestep and yields "".
    let (t, c) = if dims.len() == 3 && dims[0] == 1 && dims[1] != 1 {
        (dims[1], dims[2])
    } else if dims.len() == 3 {
        (dims[0], dims[2])
    } else {
        (
            dims.first().copied().unwrap_or(0),
            dims.last().copied().unwrap_or(0),
        )
    };

    // CTC greedy decode
    let mut prev: Option<usize> = None;
    let mut out = String::new();
    for ti in 0..t {
        let row = &flat[ti * c..(ti + 1) * c];
        let mut best = 0usize;
        let mut best_v = f32::MIN;
        for (i, &v) in row.iter().enumerate() {
            if v > best_v {
                best_v = v;
                best = i;
            }
        }
        if best == 0 {
            prev = None;
            continue;
        }
        if Some(best) != prev {
            if best < eng.charset.len() {
                out.push(eng.charset[best]);
            }
        }
        prev = Some(best);
    }
    if out.is_empty() {
        return Err(anyhow!(
            "empty CTC decode (dims {:?}, t={}, c={}) — model missed the text",
            dims,
            t,
            c
        ));
    }
    Ok(out)
}
