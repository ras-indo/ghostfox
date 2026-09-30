//! The engine abstraction: every browser backend (Chromium/CDP, patched
//! Firefox, Servo, ...) implements this one trait.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EngineKind {
    Chromium,
    Firefox,
    Servo,
}

impl EngineKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineKind::Chromium => "chromium",
            EngineKind::Firefox => "firefox",
            EngineKind::Servo => "servo",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchOptions {
    /// Persistent profile directory (empty = ephemeral).
    pub profile_dir: Option<String>,
    /// Proxy URL, e.g. `socks5://user:pass@host:port`.
    pub proxy: Option<String>,
    /// Extra command-line switches passed to the engine binary.
    pub extra_args: Vec<String>,
    /// Run headless.
    pub headless: bool,
    /// Engine binary override; autodetected when absent.
    pub executable: Option<String>,
    /// Pre-generated identity (TOML) to inject at launch. When set, the
    /// engine uses THIS identity instead of generating a fresh one —
    /// guaranteeing the fingerprint the caller recorded (evidence) is the
    /// fingerprint the engine actually runs with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_toml: Option<String>,
}

/// The full a11y snapshot result: elements plus page metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A11ySnapshot {
    pub elements: Vec<A11yElement>,
    /// "logged-in" | "logged-out" | "unknown" — detected from login/user-menu
    /// signals so agents don't act blind on a dead session.
    pub login_state: String,
    pub page_url: String,
    pub page_title: String,
    /// v0.5: "financial" | "medical" | "legal" | "authentication" | null
    /// — agents slow down on sensitive pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub danger_zone: Option<String>,
    /// v0.5: number of elements flagged as containing suspicious content
    /// (prompt injection patterns, hidden text).
    #[serde(default)]
    pub suspicious_elements: usize,
    /// v0.5: number of elements where hidden text was stripped from the
    /// accessible name/value (content sanitization — see element `stripped`).
    #[serde(default)]
    pub stripped_content: usize,
    /// v0.5.3: page is archived (read-only, no new interactions possible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_archived: Option<bool>,
    /// v0.5.3: number of elements that belong to the logged-in user.
    #[serde(default)]
    pub own_elements: usize,
    /// v0.5.3: logged-in username (from own-content signals / user menu).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// v0.5.3: interactive elements below the visible viewport (scroll to reach).
    #[serde(default)]
    pub below_viewport: usize,
    /// v0.5.3: how many viewport-pages down the lowest element is.
    #[serde(default)]
    pub max_scroll_pages: usize,
    /// v0.5.3: visible notifications (toasts, alerts, errors) — the agent
    /// MUST read these after every action (self health).
    #[serde(default)]
    pub notifications: Vec<String>,
    /// v0.5.3: rate limit seconds remaining, parsed from notifications
    /// ("try again in 381 seconds"). Agent waits instead of retrying blind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_seconds: Option<u64>,
}

/// A semantic element from the accessibility walk: what an agent needs to
/// understand and act on a page without knowing any CSS selector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A11yElement {
    /// Stable handle for click_ref / type_ref ("e12").
    pub r#ref: String,
    /// ARIA-ish role: button, link, textbox, heading, combobox...
    pub role: String,
    /// Accessible name (label, aria-label, placeholder or text).
    pub name: String,
    /// Current value for inputs/editors (reads live form state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// v0.5: element contains suspicious content (prompt injection patterns).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspicious: Option<bool>,
    /// v0.5: number of invisible chars stripped from this element's
    /// innerText-derived name/value (content sanitization).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripped: Option<usize>,
    /// v0.5.3: HTML tag name (button, input, a, shreddit-*, facepile-*...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// v0.5.3: expandable element state (aria-expanded / open attr).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    /// v0.5.3: form field is required (required / aria-required).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// v0.5.3: "visible" | "below" (scroll down) | "hidden" (off-screen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    /// v0.5.3: if visibility == "below", how many viewport-pages down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll_pages: Option<f64>,
    /// v0.5.3: element is OUR own content (matches logged-in username).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own: Option<bool>,
    /// v0.5.3: aria-description / title text for extra context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A captured page state, cheap to hand to an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSnapshot {
    pub url: String,
    pub title: Option<String>,
    /// Accessibility-tree derived text, token-friendly.
    pub content: String,
    pub captured_at: chrono::DateTime<chrono::Utc>,
}

/// Build the JS expression for a DOM-level click variant (right-click /
/// double-click via dispatched DOM events). isTrusted=false — engines with a
/// real mouse should prefer engine-level dispatch; this is the portable path
/// (default trait impl, form controls, navigation-churn fallback).
pub fn dom_click_variant_js(selector: &str, button: u8, count: u8) -> String {
    let sel = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".into());
    format!(
        "(() => {{ const el = document.querySelector({sel}); if (!el) return 'MISSING'; \
         el.scrollIntoView({{block:'center'}}); \
         const r = el.getBoundingClientRect(); \
         const x = r.x + r.width/2, y = r.y + r.height/2; \
         const mk = (type, b, detail, buttons) => new MouseEvent(type, \
           {{bubbles:true, cancelable:true, composed:true, view:window, \
             clientX:x, clientY:y, button:b, buttons:buttons, detail:detail}}); \
         if ({btn} === 2) {{ \
           el.dispatchEvent(mk('mousedown',2,1,2)); \
           el.dispatchEvent(mk('mouseup',2,1,0)); \
           el.dispatchEvent(mk('contextmenu',2,1,0)); \
         }} else {{ \
           const n = {cnt}; \
           for (let i = 1; i <= n; i++) {{ \
             el.dispatchEvent(mk('mousedown',0,i,1)); \
             el.dispatchEvent(mk('mouseup',0,i,0)); \
             el.dispatchEvent(mk('click',0,i,0)); \
           }} \
           if (n === 2) el.dispatchEvent(mk('dblclick',0,2,0)); \
         }} \
         return 'OK'; }})()",
        sel = sel,
        btn = button,
        cnt = count,
    )
}

/// A live handle to one page (tab) inside an engine instance.
#[async_trait]
pub trait PageHandle: Send + Sync {
    async fn navigate(&self, url: &str) -> Result<()>;
    /// v0.6.2: The engine-level target id (tab), if this handle wraps one.
    fn target_id(&self) -> Option<String> {
        None
    }
    async fn snapshot(&self) -> Result<PageSnapshot>;
    async fn click(&self, selector: &str) -> Result<()>;
    /// Click variant: `button` 0=left, 2=right; `count` 1 or 2 (2 = double
    /// click). Default implementation dispatches DOM events via JS — portable
    /// across engines but isTrusted=false (anti-bot gates may reject it).
    /// Engines with a real mouse override this with engine-level dispatch.
    async fn click_variant(&self, selector: &str, button: u8, count: u8) -> Result<()> {
        let res = self
            .evaluate(&dom_click_variant_js(selector, button, count))
            .await?;
        match res.as_str() {
            Some("OK") => Ok(()),
            _ => Err(crate::error::GhostError::PageOp(
                "selector not found".into(),
            )),
        }
    }
    async fn type_text(&self, selector: &str, text: &str) -> Result<()>;
    async fn evaluate(&self, expression: &str) -> Result<serde_json::Value>;
    async fn url(&self) -> Result<String>;
    async fn close(&self) -> Result<()>;
    /// Press a named key (Enter, Tab, Escape, arrows...). Engines without
    /// keyboard-event support return an error naming the limitation.
    async fn press_key(&self, key: &str) -> Result<()> {
        let _ = key;
        Err(crate::error::GhostError::PageOp(
            "press_key not supported by this engine".into(),
        ))
    }
    /// Capture the current viewport (full_page=false) or the whole scrollable
    /// document as PNG bytes. Engines without screenshot support return an
    /// error naming the limitation.
    async fn screenshot(&self, full_page: bool) -> Result<Vec<u8>> {
        let _ = full_page;
        Err(crate::error::GhostError::PageOp(
            "screenshot not supported by this engine".into(),
        ))
    }
    /// Semantic snapshot: walk the page (including shadow roots), return
    /// interactive elements with stable refs an agent can act on.
    async fn a11y_snapshot(&self) -> Result<A11ySnapshot> {
        Err(crate::error::GhostError::PageOp(
            "a11y_snapshot not supported by this engine".into(),
        ))
    }
    /// Act on an element by its ref from a11y_snapshot.
    async fn click_ref(&self, r: &str) -> Result<()> {
        let _ = r;
        Err(crate::error::GhostError::PageOp(
            "click_ref not supported by this engine".into(),
        ))
    }
    /// Move the mouse onto the element a ref points at, along a
    /// human-like path (hover). Engines without mouse pathing return
    /// an error naming the limitation.
    async fn mouse_move_to(&self, r: &str) -> Result<()> {
        let _ = r;
        Err(crate::error::GhostError::PageOp(
            "mouse_move_to not supported by this engine".into(),
        ))
    }
    /// Drag the element `from` (ref) onto the element `to` (ref), or by
    /// an offset when `to` is empty, with a human-like movement profile
    /// (bezier arc, ease-in-out velocity, jitter, overshoot+correction).
    async fn drag_ref(&self, from: &str, to: &str, dx: f64, dy: f64) -> Result<()> {
        let _ = (from, to, dx, dy);
        Err(crate::error::GhostError::PageOp(
            "drag_ref not supported by this engine".into(),
        ))
    }
    /// Safety cleanup: release the mouse button at (x, y) after a
    /// cancelled humanized drag (timeout dropped the future between
    /// mousedown and mouseup — a stuck-pressed button wedges the whole
    /// session). Engines with real mouse dispatch override this;
    /// the default is a no-op error so callers treat it as best-effort.
    async fn release_mouse_at(&self, x: f64, y: f64) -> Result<()> {
        let _ = (x, y);
        Err(crate::error::GhostError::PageOp(
            "release_mouse_at not supported by this engine".into(),
        ))
    }
    /// v0.6.3: Register a script that runs at DOCUMENT START on every
    /// navigation — before any page script. The deepest hook layer.
    async fn add_init_script(&self, source: &str) -> Result<()> {
        let _ = source;
        Err(crate::error::GhostError::PageOp(
            "add_init_script not supported by this engine".into(),
        ))
    }

    /// Go back one history entry via the engine's NATIVE protocol call —
    /// never `history.back()` in page JS, which wedges the content process.
    /// Returns whether a previous entry existed.
    async fn go_back(&self) -> Result<bool> {
        Err(crate::error::GhostError::PageOp(
            "go_back not supported by this engine".into(),
        ))
    }

    /// Forward one history entry (native protocol call). Returns whether a
    /// next entry existed.
    async fn go_forward(&self) -> Result<bool> {
        Err(crate::error::GhostError::PageOp(
            "go_forward not supported by this engine".into(),
        ))
    }

    /// Reload the page via the engine's NATIVE protocol call (safe during
    /// load, unlike `location.reload()` through evaluate).
    async fn reload_page(&self) -> Result<()> {
        Err(crate::error::GhostError::PageOp(
            "reload_page not supported by this engine".into(),
        ))
    }

    /// v0.7 DEBUG CORTEX: buffered console messages (log/warning/error)
    /// captured at the PROTOCOL level (Runtime.consoleAPICalled) — the
    /// page cannot hide or patch it. `clear` drains the buffer.
    async fn console_read(&self, clear: bool) -> Result<Vec<serde_json::Value>> {
        let _ = clear;
        Err(crate::error::GhostError::PageOp(
            "console capture not supported by this engine".into(),
        ))
    }

    /// v0.7 DEBUG CORTEX: buffered uncaught JS exceptions with stack
    /// traces (Runtime.exceptionThrown). `clear` drains the buffer.
    async fn errors_read(&self, clear: bool) -> Result<Vec<serde_json::Value>> {
        let _ = clear;
        Err(crate::error::GhostError::PageOp(
            "error capture not supported by this engine".into(),
        ))
    }

    /// v0.7 DEBUG CORTEX: structured network entries
    /// [{requestId, url, method, status}] — request metadata + response
    /// status, captured passively below the page. `clear` drains.
    async fn net_read(&self, clear: bool) -> Result<Vec<serde_json::Value>> {
        let _ = clear;
        Err(crate::error::GhostError::PageOp(
            "net capture not supported by this engine".into(),
        ))
    }

    /// v0.6.3: PROTOCOL-LEVEL network capture — start collecting every
    /// HTTP response for this page, BELOW the page (invisible to page JS).
    async fn net_capture_start(&self) -> Result<()> {
        Err(crate::error::GhostError::PageOp(
            "net_capture not supported by this engine".into(),
        ))
    }
    /// List captured responses (url, requestId) since capture start.
    async fn net_capture_list(&self) -> Result<Vec<(String, String)>> {
        Err(crate::error::GhostError::PageOp(
            "net_capture not supported by this engine".into(),
        ))
    }
    /// Fetch a captured response body BY PROTOCOL (Network.getResponseBody).
    async fn net_get_body(&self, request_id: &str) -> Result<String> {
        let _ = request_id;
        Err(crate::error::GhostError::PageOp(
            "net_capture not supported by this engine".into(),
        ))
    }

    /// v0.6.2 SUPERMAN GLASSES: render the element a ref points at
    /// (canvas / img / background-image) as a compact luminance grid
    /// the agent READS as digits — a text-model-friendly way to see
    /// shapes without a vision model.
    async fn pixels_ref(&self, r: &str, gw: u32, gh: u32) -> Result<String> {
        let _ = (r, gw, gh);
        Err(crate::error::GhostError::PageOp(
            "pixels_ref not supported by this engine".into(),
        ))
    }
    /// v0.6.3: HIGH-PASS VISION — local-contrast grid of an element's
    /// image. |gray - blur| makes blended content visible (captcha
    /// characters on photos, watermarks). The technique that solved the
    /// captcha family we believed needed a vision model.
    async fn contrast_ref(&self, r: &str, gw: u32, gh: u32, radius: u32) -> Result<String> {
        let _ = (r, gw, gh, radius);
        Err(crate::error::GhostError::PageOp(
            "contrast_ref not supported by this engine".into(),
        ))
    }
    /// v0.6.3: REAL template matching — multi-scale NCC of a needle
    /// (element image, optional sub-rect crop) against a haystack image,
    /// computed in-page at full grayscale resolution. Returns top match
    /// positions (needle centers) in haystack pixels.
    #[allow(clippy::too_many_arguments)]
    async fn match_image_ref(
        &self,
        needle_ref: &str,
        nx: i64,
        ny: i64,
        nw: i64,
        nh: i64,
        hay_ref: &str,
        hx: i64,
        hy: i64,
        hw: i64,
        hh: i64,
    ) -> Result<String> {
        let _ = (needle_ref, nx, ny, nw, nh, hay_ref, hx, hy, hw, hh);
        Err(crate::error::GhostError::PageOp(
            "match_image_ref not supported by this engine".into(),
        ))
    }
    /// Read the FULL value of the element a ref points at (no truncation).
    async fn read_ref_full(&self, r: &str) -> Result<String> {
        let _ = r;
        Err(crate::error::GhostError::PageOp(
            "read_ref_full not supported by this engine".into(),
        ))
    }
    /// Wait until a CSS selector becomes visible (or timeout). Returns true if visible.
    async fn wait_for(&self, selector: &str, timeout_ms: u64) -> Result<bool> {
        let _ = (selector, timeout_ms);
        Err(crate::error::GhostError::PageOp(
            "wait_for not supported by this engine".into(),
        ))
    }
    /// Type text into the element a ref points at (inputs, editors).
    async fn type_ref(&self, r: &str, text: &str) -> Result<()> {
        let _ = (r, text);
        Err(crate::error::GhostError::PageOp(
            "type_ref not supported by this engine".into(),
        ))
    }
    /// Dispatch a native wheel (scroll) event at viewport coords (dx/dy px).
    async fn dispatch_wheel(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        let _ = (x, y, dx, dy);
        Err(crate::error::GhostError::PageOp(
            "dispatch_wheel not supported by this engine".into(),
        ))
    }
    /// Browser-level cookie jar: "get" (ALL cookies incl. httpOnly), "set"
    /// (array of CookieOptions), "clear". Runs on the root session.
    async fn browser_cookies(
        &self,
        action: &str,
        cookies: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let _ = cookies;
        Err(crate::error::GhostError::PageOp(format!(
            "browser_cookies({action}) not supported by this engine"
        )))
    }
    /// Grant page permissions (geolocation, camera, ...) for an origin.
    async fn grant_permissions(&self, origin: &str, permissions: &[String]) -> Result<()> {
        let _ = (origin, permissions);
        Err(crate::error::GhostError::PageOp(
            "grant_permissions not supported by this engine".into(),
        ))
    }
    /// Reset all granted permissions for the browser.
    async fn reset_permissions(&self) -> Result<()> {
        Err(crate::error::GhostError::PageOp(
            "reset_permissions not supported by this engine".into(),
        ))
    }
    /// Configure file downloads: "saveToDisk" | "cancel" into `dir`.
    async fn set_download_options(&self, behavior: &str, dir: &str) -> Result<()> {
        let _ = (behavior, dir);
        Err(crate::error::GhostError::PageOp(
            "set_download_options not supported by this engine".into(),
        ))
    }
    /// Arm the native JS dialog handler (alert/confirm/prompt/beforeunload):
    /// auto-responds per policy so dialogs never wedge the page, and logs
    /// every dialog that opened.
    async fn dialog_setup(&self, accept: bool, prompt_text: Option<String>) -> Result<()> {
        let _ = (accept, prompt_text);
        Err(crate::error::GhostError::PageOp(
            "dialog_setup not supported by this engine".into(),
        ))
    }
    /// Read (and optionally clear) the buffered dialog log for this page.
    async fn dialog_log(&self, clear: bool) -> Result<serde_json::Value> {
        let _ = clear;
        Err(crate::error::GhostError::PageOp(
            "dialog_log not supported by this engine".into(),
        ))
    }
    /// Apply browser emulation ops — a JSON map of the keys this engine
    /// understands (color_scheme, media, reduced_motion, forced_colors,
    /// contrast, viewport, online, geolocation, user_agent, timezone,
    /// locale, platform, headers, http_auth). Keys absent from the map are
    /// left untouched. Returns `{applied:[...], errors:[...]}` — partial
    /// success is reported per key instead of aborting the whole batch.
    async fn emulate(&self, _ops: &serde_json::Value) -> Result<serde_json::Value> {
        Err(crate::error::GhostError::PageOp(
            "emulate not supported by this engine".into(),
        ))
    }
}

/// A running engine process managing pages.
#[async_trait]
pub trait Engine: Send + Sync {
    fn kind(&self) -> EngineKind;
    async fn new_page(
        &self,
        opts: &HashMap<String, serde_json::Value>,
    ) -> Result<Arc<dyn PageHandle>>;
    async fn pages(&self) -> Result<Vec<String>>;
    async fn shutdown(&self) -> Result<()>;
    /// v0.6.2: All browser targets (tabs AND site-opened popups) with
    /// URLs where known. Engines without popup vision return their
    /// known page ids with unknown URLs.
    async fn list_targets(&self) -> Result<Vec<(String, Option<String>)>> {
        Ok(self
            .pages()
            .await?
            .into_iter()
            .map(|id| (id, None))
            .collect())
    }
    /// v0.6.2: Attach to an existing target (a popup the SITE opened —
    /// OAuth windows, payment flows). Returns a live page handle.
    async fn attach_target(&self, target_id: &str) -> Result<Arc<dyn PageHandle>> {
        let _ = target_id;
        Err(crate::error::GhostError::PageOp(
            "attach_target not supported by this engine".into(),
        ))
    }
}
