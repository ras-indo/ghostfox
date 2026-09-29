//! LOCAL CAPTCHA solving — no third-party services, no API keys.
//!
//! Image/text captchas are classified on-device with the ddddocr ONNX
//! model (same engine as page_captcha_ocr). Standalone Turnstile /
//! reCAPTCHA / hCaptcha tokens are issued by the vendor server and
//! cannot be produced on-device; solve those interactively on the live
//! page with the page tools (page_hcaptcha = agent-driven image grids).

use base64::Engine as _;

/// Solve an image/text captcha from base64 PNG with the local ddddocr
/// model. Accepts raw base64 or a `data:image/png;base64,...` URL.
pub async fn solve_image(image_b64: &str) -> Result<String, String> {
    let raw = image_b64.trim();
    let raw = raw.rsplit(',').next().unwrap_or(raw);
    let png = base64::engine::general_purpose::STANDARD
        .decode(raw)
        .map_err(|e| format!("invalid base64 image: {e}"))?;
    crate::ddddocr::classify_png(&png)
        .await
        .map_err(|e| format!("local ddddocr classify failed: {e}"))
}

/// Message for vendor-issued token challenges (Turnstile/reCAPTCHA):
/// there is no local way to mint these tokens.
pub const NO_STANDALONE_TOKEN: &str = "token captchas (Turnstile/reCAPTCHA) are issued by the vendor server — no local solver can mint them and no external service is used. Open the challenge on the live page and solve it interactively: checkbox/widget via page_click, image grids via page_hcaptcha (agent-driven, local vision). For plain image captchas pass image_base64 (solved locally with ddddocr).";
