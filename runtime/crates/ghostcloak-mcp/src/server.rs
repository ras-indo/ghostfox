//! MCP server implementation: tools mapped 1:1 onto core operations.
//!
//! Every tool is narrow and typed — no god-tools, no "smart" behavior an
//! attacker could steer through page content.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use rmcp::handler::server::tool::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use ghostcloak_core::engine::EngineKind;
use ghostcloak_core::session::Session;

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionCreateParams {
    /// Restrict identity platform: "windows" | "macos" | "linux" | "android" (optional).
    #[serde(default)]
    platform: Option<String>,
    /// Reuse a persistent profile directory (optional).
    #[serde(default)]
    profile_dir: Option<String>,
    /// Proxy URL, e.g. socks5://user:pass@host:port (optional).
    #[serde(default)]
    proxy: Option<String>,
    /// Run with a visible browser window instead of headless (optional) —
    /// for humans who want to watch the agent work.
    #[serde(default)]
    headful: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionEvidenceParams {
    session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionCloseParams {
    session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RefParams {
    session_id: String,
    page_id: String,
    /// Element ref from page_a11y (e.g. "e12").
    r#ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DragParams {
    session_id: String,
    page_id: String,
    /// Ref of the element to grab (from page_a11y).
    from_ref: String,
    /// Ref of the drop target. Omit to drag by offset instead (sliders).
    to_ref: Option<String>,
    /// Horizontal pixels to drag when to_ref is omitted (positive = right).
    offset_x: Option<f64>,
    /// Vertical pixels to drag when to_ref is omitted (positive = down).
    offset_y: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PixelsParams {
    session_id: String,
    page_id: String,
    /// Ref of the element to render (canvas / img / background-image).
    r#ref: String,
    /// Grid width in cells (default 32).
    grid_w: Option<u32>,
    /// Grid height in cells (default 21).
    grid_h: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ContrastParams {
    session_id: String,
    page_id: String,
    /// Ref of the element to analyze (canvas / img / background-image).
    r#ref: String,
    /// Grid width in cells (default 100).
    grid_w: Option<u32>,
    /// Grid height in cells (default 44).
    grid_h: Option<u32>,
    /// Gaussian blur radius in px (default 4).
    blur_radius: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InitScriptParams {
    session_id: String,
    page_id: String,
    /// JavaScript source to run at document start on every navigation.
    source: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NetParams {
    session_id: String,
    page_id: String,
    /// Optional URL substring filter (e.g. "geetest").
    filter: Option<String>,
    /// Clear the capture buffer after reading (default false). The buffer
    /// caps at 1000 entries — clear it between scenarios so old traffic
    /// doesn't drown the requests you care about.
    #[serde(default)]
    clear: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NetBodyParams {
    session_id: String,
    page_id: String,
    /// requestId from page_network_read.
    request_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MatchImageParams {
    session_id: String,
    page_id: String,
    /// Ref of the NEEDLE element (canvas / img / background-image).
    needle_ref: String,
    /// Optional crop of the needle image (x, y, w, h in needle-image px).
    /// Omit to use the whole image. Use this to match a sub-region
    /// (e.g. an instruction glyph band inside the same image).
    needle_rect: Option<Vec<i64>>,
    /// Ref of the HAYSTACK element to search in.
    hay_ref: String,
    /// Optional search-region crop of the haystack (x, y, w, h in
    /// haystack-image px). Omit for the full image. USE THIS to
    /// exclude the needle's own area (self-match guard).
    hay_rect: Option<Vec<i64>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OcrParams {
    session_id: String,
    page_id: String,
    /// Ref of the element to OCR. Omit for the whole viewport.
    r#ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct VisionParams {
    session_id: String,
    page_id: String,
    /// Ref of the element to analyze. Omit for the whole viewport.
    r#ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GeetestClickParams {
    session_id: String,
    page_id: String,
    /// Ref of the .geetest_item_wrap element (carries the challenge background image).
    r#ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GeetestSlideParams {
    session_id: String,
    page_id: String,
    /// Optional ref of the slider handle (.geetest_slider_button). If omitted
    /// the tool locates it by selector and registers its own ref.
    slider_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CaptchaOcrParams {
    session_id: String,
    page_id: String,
    /// Ref of the captcha <img> element. Omit for the whole viewport.
    r#ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct Tile {
    x: f64,
    y: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DragXY {
    from: Tile,
    to: Tile,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HcaptchaParams {
    session_id: String,
    page_id: String,
    /// Optional ref of the hCaptcha anchor iframe / checkbox. If omitted
    /// the tool auto-locates the anchor iframe by src pattern.
    checkbox_ref: Option<String>,
    /// APPLY phase: viewport-absolute pixel coordinates to click this
    /// round, as read from the need_answer screenshot. Omit for the
    /// PREPARE phase (opens the challenge and returns a screenshot).
    tiles: Option<Vec<Tile>>,
    /// Optional drag puzzle: from/to in viewport pixels (same screenshot
    /// coordinate space as tiles).
    drag: Option<DragXY>,
    /// Optional verify/submit button coordinate (viewport pixels).
    verify: Option<Tile>,
    /// Max challenge rounds (hCaptcha chains 2-4). Default 6.
    max_rounds: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConsoleParams {
    session_id: String,
    page_id: String,
    /// Drain the buffer after reading (default: keep entries).
    clear: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ErrorsParams {
    session_id: String,
    page_id: String,
    /// Drain the buffer after reading (default: keep entries).
    clear: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RotateParams {
    session_id: String,
    page_id: String,
    /// Ref of the rotate-right button (15deg per click in the standard demos).
    rot_right_ref: String,
    /// Ref of the rotate-left button.
    rot_left_ref: String,
    /// Ref of the check/verify button.
    check_ref: String,
    /// Ref of the reset button.
    reset_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PagesParams {
    session_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageCommentParams {
    session_id: String,
    page_id: String,
    /// The comment text to submit.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionMeParams {
    session_id: String,
    #[serde(default)]
    page_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConfirmActionParams {
    session_id: String,
    page_id: String,
    /// What the agent is about to do (for the evidence log).
    action: String,
    /// The ref of the element that will be clicked/activated.
    r#ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DismissModalParams {
    session_id: String,
    page_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadRefParams {
    session_id: String,
    page_id: String,
    /// Element ref from page_a11y (e.g. "e12").
    r#ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitForParams {
    session_id: String,
    page_id: String,
    /// CSS selector to wait for.
    selector: String,
    /// Timeout in milliseconds (default 10000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UploadParams {
    session_id: String,
    page_id: String,
    /// CSS selector for the file input (input[type=file]).
    selector: String,
    /// Absolute path to the file to upload.
    file_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TypeRefParams {
    session_id: String,
    page_id: String,
    /// Element ref from page_a11y (e.g. "e12").
    r#ref: String,
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageEvalParams {
    session_id: String,
    page_id: String,
    /// JavaScript expression; the JSON-ified result is returned.
    expression: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageScreenshotParams {
    session_id: String,
    page_id: String,
    /// Capture the whole scrollable document instead of the viewport (optional).
    #[serde(default)]
    full_page: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CaptchaSolveParams {
    session_id: String,
    /// Turnstile/hcaptcha-style: the site's sitekey.
    #[serde(default)]
    sitekey: Option<String>,
    /// Turnstile/hcaptcha-style: the page URL the challenge lives on.
    #[serde(default)]
    pageurl: Option<String>,
    /// Image captcha: the challenge image as base64 PNG.
    #[serde(default)]
    image_base64: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageOpenParams {
    session_id: String,
    url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageRefParams {
    session_id: String,
    page_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageClickParams {
    session_id: String,
    page_id: String,
    /// CSS selector.
    selector: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageTypeParams {
    session_id: String,
    page_id: String,
    /// CSS selector.
    selector: String,
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageFillParams {
    session_id: String,
    page_id: String,
    /// CSS selector.
    selector: String,
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PagePressParams {
    session_id: String,
    page_id: String,
    /// Key name: Enter, Tab, Escape, Backspace, Delete, ArrowUp/Down/Left/Right, Home, End, PageUp, PageDown.
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IdentityAuditParams {
    identity_toml: String,
}

// ===== SEARCH/QUERY/DROPDOWN/SCROLL-TEXT (added 2026-09-30, from
//      browser-use search_page/find_elements/dropdown_options/scroll_to_text) =====

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchParams {
    session_id: String,
    page_id: String,
    /// Text (or regex) to find inside the rendered DOM.
    pattern: String,
    /// Treat pattern as a regular expression (default false = literal).
    #[serde(default)]
    regex: Option<bool>,
    /// Case-sensitive match (default false = insensitive).
    #[serde(default)]
    case_sensitive: Option<bool>,
    /// Restrict the search to elements under this CSS selector.
    #[serde(default)]
    css_scope: Option<String>,
    /// Characters of surrounding context per hit (default 150).
    #[serde(default)]
    context_chars: Option<usize>,
    /// Max hits returned (default 25).
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QueryParams {
    session_id: String,
    page_id: String,
    /// CSS selector (querySelectorAll syntax).
    selector: String,
    /// Extra attributes to collect per element (e.g. ["href", "src"]).
    #[serde(default)]
    attributes: Option<Vec<String>>,
    /// Max elements returned (default 50).
    #[serde(default)]
    max_results: Option<usize>,
    /// Include trimmed inner text per element (default true).
    #[serde(default)]
    include_text: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DropdownParams {
    session_id: String,
    page_id: String,
    /// CSS selector of the <select> or the ARIA menu container.
    selector: String,
    /// "list" (default) = enumerate options, "select" = pick one.
    #[serde(default)]
    action: Option<String>,
    /// Option label to match exactly (text.trim() equality).
    #[serde(default)]
    text: Option<String>,
    /// Option value to match exactly (fallback when text has no match).
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScrollTextParams {
    session_id: String,
    page_id: String,
    /// Text that must become visible in the viewport.
    text: String,
    /// "down" (default) = first occurrence from top, "up" = last occurrence.
    #[serde(default)]
    direction: Option<String>,
}

// ===== BATCH TOOLS (added 2026-09-29: extraction/scraping/debug layer) =====

#[derive(Debug, Deserialize, JsonSchema)]
struct ExtractParams {
    session_id: String,
    page_id: String,
    /// CSS selector targeting specific elements (tables, lists, anything).
    /// Omit for auto-detection of every table + list on the page.
    #[serde(default)]
    selector: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MarkdownParams {
    session_id: String,
    page_id: String,
    /// CSS selector of the subtree to convert (e.g. "article"). Omit to
    /// auto-pick article > main > body.
    #[serde(default)]
    selector: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BatchParams {
    session_id: String,
    page_id: String,
    /// Ordered actions, e.g. [{"action":"click","selector":"#go"},
    /// {"action":"type","selector":"input","text":"hi"},
    /// {"action":"eval","expression":"document.title"}].
    /// Actions: click | click_ref | type | fill | press | eval | wait |
    /// move_to | back | forward | reload.
    actions: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IdleParams {
    session_id: String,
    page_id: String,
    /// Max total wait in ms (default 15000).
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Quiet period with zero new responses, in ms (default 800).
    #[serde(default)]
    idle_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TokensParams {
    session_id: String,
    page_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AntiBotParams {
    session_id: String,
    page_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StorageParams {
    session_id: String,
    page_id: String,
    /// get (default) | set | clear | keys
    #[serde(default)]
    op: Option<String>,
    /// local (localStorage, default) | session (sessionStorage) | cookie
    #[serde(default)]
    area: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HttpParams {
    session_id: String,
    page_id: String,
    /// URL to fetch FROM THE PAGE CONTEXT (cookies + page origin apply;
    /// cross-origin still subject to CORS, same-origin/API pivots always work).
    url: String,
    /// GET (default) | POST | PUT | PATCH | DELETE
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HarParams {
    session_id: String,
    page_id: String,
}

/// ### BATCH TOOLS 2026-09-29 (native Juggler capabilities)
#[derive(Debug, Deserialize, JsonSchema)]
struct ScrollParams {
    session_id: String,
    page_id: String,
    /// Scroll WITHIN this element (it is scrolled into view first, then the
    /// wheel fires at its center — nested containers scroll correctly).
    #[serde(default)]
    selector: Option<String>,
    /// Viewport x for the wheel (default: element or viewport center).
    #[serde(default)]
    x: Option<f64>,
    /// Viewport y for the wheel (default: element or viewport center).
    #[serde(default)]
    y: Option<f64>,
    /// Horizontal wheel delta in px (positive = right).
    #[serde(default)]
    dx: Option<f64>,
    /// Vertical wheel delta in px (positive = down, negative = up).
    #[serde(default)]
    dy: Option<f64>,
    /// Split into N wheel events for a natural feel (default 8).
    #[serde(default)]
    steps: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CookiesParams {
    session_id: String,
    page_id: String,
    /// get | set | clear
    action: String,
    /// get: filter by cookie name (exact).
    #[serde(default)]
    name: Option<String>,
    /// get: filter by domain substring.
    #[serde(default)]
    domain: Option<String>,
    /// set: cookies to write — [{name, value, url|domain, path?, secure?,
    /// httpOnly?, sameSite? (Strict|Lax|None), expires? (unix seconds)}]
    #[serde(default)]
    cookies: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PermissionsParams {
    session_id: String,
    page_id: String,
    /// grant | reset
    action: String,
    /// grant: origin the permission applies to, e.g. "https://example.com"
    #[serde(default)]
    origin: Option<String>,
    /// grant: ["geolocation","camera","microphone","notifications",
    /// "clipboard-read","clipboard-write","persistent-storage"]
    #[serde(default)]
    permissions: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DownloadParams {
    session_id: String,
    page_id: String,
    /// Click this element to start the download (e.g. "a#export").
    #[serde(default)]
    selector: Option<String>,
    /// ...or navigate to a file URL that triggers the download.
    #[serde(default)]
    url: Option<String>,
    /// Destination dir (default /root/tmp/ghostfox-dl, created if missing).
    #[serde(default)]
    dir: Option<String>,
    /// Max wait for a new file to appear (default 20000).
    #[serde(default)]
    wait_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DialogParams {
    session_id: String,
    page_id: String,
    /// arm (set auto-responder policy) | list (read the dialog log)
    #[serde(default)]
    action: Option<String>,
    /// arm: accept dialogs (default true). false = dismiss them.
    #[serde(default)]
    accept: Option<bool>,
    /// arm: text to answer prompt() dialogs with (default "").
    #[serde(default)]
    prompt_text: Option<String>,
    /// list: drain the log after reading (default false = keep).
    #[serde(default)]
    clear: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GeolocationSpec {
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    accuracy: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HttpAuthSpec {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ViewportSpec {
    width: u64,
    height: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EmulateParams {
    session_id: String,
    page_id: String,
    /// "dark" | "light" | "none" (reset) → prefers-color-scheme.
    #[serde(default)]
    color_scheme: Option<String>,
    /// true=online, false=offline → navigator.onLine & network.
    #[serde(default)]
    online: Option<bool>,
    /// {latitude, longitude, accuracy?} — omit to leave untouched.
    #[serde(default)]
    geolocation: Option<GeolocationSpec>,
    /// Override navigator.userAgent.
    #[serde(default)]
    user_agent: Option<String>,
    /// IANA timezone (e.g. "America/New_York").
    #[serde(default)]
    timezone: Option<String>,
    /// BCP-47 language (e.g. "id-ID").
    #[serde(default)]
    locale: Option<String>,
    /// navigator.platform value.
    #[serde(default)]
    platform: Option<String>,
    /// Extra headers sent with EVERY request: {"name": "value"}.
    #[serde(default)]
    headers: Option<serde_json::Value>,
    /// HTTP Basic auth {username, password} for all requests.
    #[serde(default)]
    http_auth: Option<HttpAuthSpec>,
    /// {width, height} window viewport.
    #[serde(default)]
    viewport: Option<ViewportSpec>,
    /// "print" | "screen" | "none" (reset) → matchMedia.
    #[serde(default)]
    media: Option<String>,
    /// "reduce" | "none" → prefers-reduced-motion.
    #[serde(default)]
    reduced_motion: Option<String>,
    /// "active" | "none" → prefers-forced-colors.
    #[serde(default)]
    forced_colors: Option<String>,
    /// "less" | "more" | "custom" | "none" → prefers-contrast.
    #[serde(default)]
    contrast: Option<String>,
}

#[derive(Clone, Default)]
pub struct GhostcloakServer {
    state: Arc<tokio::sync::RwLock<ServerState>>,
    recorder: crate::recording::Recorder,
}

#[derive(Default)]
pub(crate) struct ServerState {
    pub(crate) sessions: HashMap<String, Arc<Session>>,
}

impl GhostcloakServer {
    pub fn new() -> Self {
        Self::default()
    }

    async fn session(&self, id: &str) -> Result<Arc<Session>, String> {
        self.state
            .read()
            .await
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| format!("session `{id}` not found"))
    }
}

/// Block until `document.body` exists (max ~15s) so callers never race a
/// blank document: page_open/page_back/page_reload used to return before
/// the DOM was parseable, failing every follow-up call.
async fn wait_for_dom(page: &Arc<dyn ghostcloak_core::engine::PageHandle>) {
    for _ in 0..60 {
        match page.evaluate("!!document.body").await {
            Ok(v) if v.as_bool() == Some(true) => break,
            _ => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
}

/// Read the page URL, tolerating the mid-commit window where a reload or
/// history move still reports `about:blank` (poll up to ~6s).
async fn settled_url(page: &Arc<dyn ghostcloak_core::engine::PageHandle>) -> String {
    for _ in 0..24 {
        match page.url().await {
            Ok(u) if u != "about:blank" && !u.is_empty() => return u,
            Ok(u) => {
                if u == "about:blank" {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                } else {
                    return u;
                }
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
    page.url().await.unwrap_or_default()
}

fn text_result(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![rmcp::model::Content::text(s.into())])
}

/// Escape a Rust string for embedding inside a double-quoted JS string.
fn js_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "")
}

const EXTRACT_JS: &str = r#"(()=>{
  const sel = "@@SEL@@";
  const txt = e => (e.innerText || '').trim();
  const out = { tables: [], lists: [], items: [] };
  const tbl = t => ({
    caption: t.caption ? txt(t.caption) : null,
    headers: [...(t.tHead && t.tHead.rows.length ? t.tHead.rows[0].cells
              : (t.rows.length ? t.rows[0].cells : []))].map(txt),
    rows: [...t.rows].map(r => [...r.cells].map(txt))
  });
  const nodes = sel ? [...document.querySelectorAll(sel)]
                    : [...document.querySelectorAll('table,ul,ol')];
  for (const e of nodes) {
    if (e.tagName === 'TABLE') out.tables.push(tbl(e));
    else if (e.tagName === 'UL' || e.tagName === 'OL')
      out.lists.push({ ordered: e.tagName === 'OL',
        items: [...e.children].filter(c => c.tagName === 'LI').map(txt) });
    else out.items.push({ tag: e.tagName.toLowerCase(), text: txt(e).slice(0, 2000),
        href: e.href || null, src: e.src || null,
        value: ('value' in e) ? String(e.value).slice(0, 2000) : null });
  }
  return { tables: out.tables.slice(0, 50), lists: out.lists.slice(0, 50),
    items: out.items.slice(0, 500),
    counts: { tables: out.tables.length, lists: out.lists.length,
      items: out.items.length } };
})()"#;

const MARKDOWN_JS: &str = r#"(()=>{
  const sel = "@@SEL@@";
  const root = sel ? document.querySelector(sel)
    : (document.querySelector('article') || document.querySelector('main') || document.body);
  if (!root) return { error: 'no root element' };
  const conv = n => {
    if (n.nodeType === 3) return n.textContent.replace(/\s+/g, ' ');
    if (n.nodeType !== 1) return '';
    const t = n.tagName;
    if (/^(SCRIPT|STYLE|NOSCRIPT|TEMPLATE|SVG)$/.test(t)) return '';
    const c = [...n.childNodes].map(conv).join('');
    if (/^H[1-6]$/.test(t)) return '\n\n' + '#'.repeat(+t[1]) + ' ' + c.trim() + '\n';
    if (t === 'P') return '\n\n' + c.trim() + '\n';
    if (t === 'A') return '[' + c.trim() + '](' + (n.href || '') + ')';
    if (t === 'STRONG' || t === 'B') return '**' + c + '**';
    if (t === 'EM' || t === 'I') return '*' + c + '*';
    if (t === 'CODE') return '`' + c + '`';
    if (t === 'PRE') return '\n\n```\n' + n.innerText.replace(/\n$/, '') + '\n```\n';
    if (t === 'BLOCKQUOTE') return '\n\n> ' + c.trim().replace(/\n/g, '\n> ') + '\n';
    if (t === 'BR') return '\n';
    if (t === 'HR') return '\n\n---\n';
    if (t === 'IMG') return '![' + (n.alt || '') + '](' + (n.src || '') + ')';
    if (t === 'LI') return '\n- ' + c.trim();
    if (t === 'OL') return '\n' + [...n.children].map((li, i) =>
      '\n' + (i + 1) + '. ' + li.innerText.trim()).join('') + '\n';
    if (t === 'UL') return '\n' + [...n.children].map(li =>
      '\n- ' + li.innerText.trim()).join('') + '\n';
    if (t === 'TABLE') {
      const rows = [...n.rows].map(r => [...r.cells].map(
        x => x.innerText.trim().replace(/\|/g, '\\|')));
      if (!rows.length) return '';
      const head = rows[0], body = rows.slice(1);
      const line = r => '| ' + r.join(' | ') + ' |';
      return '\n\n' + line(head) + '\n|' + head.map(() => '---').join('|')
        + '|\n' + body.map(line).join('\n') + '\n';
    }
    return c;
  };
  let md = (document.title ? '# ' + document.title + '\n' : '') + conv(root);
  md = md.replace(/[ \t]+\n/g, '\n').replace(/\n{3,}/g, '\n\n').trim();
  return { markdown: md.slice(0, 300000), chars: md.length };
})()"#;

const SEARCH_JS: &str = r#"(()=>{
  const pat = "@@PAT@@";
  const isRe = @@ISRE@@, cs = @@CS@@, scope = "@@SCOPE@@", ctxN = @@CTX@@, max = @@MAX@@;
  let re;
  try {
    const src = isRe ? pat : pat.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    re = new RegExp(src, cs ? "g" : "gi");
  } catch(e){ return {error: "invalid regex: " + e.message}; }
  let roots = [document.body];
  if (scope) {
    try { roots = Array.from(document.querySelectorAll(scope)); } catch(e){ return {error: "invalid css_scope: " + e.message}; }
    if (!roots.length) return {found:0, items:[], note:"css_scope matched 0 elements"};
  }
  const items = [];
  let found = 0;
  outer:
  for (const root of roots) {
    // Sites may split text into many nodes (per-letter animation spans):
    // search the AGGREGATE textContent, then resolve each hit back to an element.
    const parts = [];
    let full = "";
    const w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    let n;
    while ((n = w.nextNode())) {
      const v = n.nodeValue || "";
      parts.push({ node: n, start: full.length, len: v.length });
      full += v;
    }
    re.lastIndex = 0;
    let m;
    while ((m = re.exec(full))) {
      found++;
      if (items.length >= max) break outer;
      let el = null;
      for (const p of parts) {
        if (m.index >= p.start && m.index < p.start + p.len) { el = p.node.parentElement; break; }
      }
      const from = Math.max(0, m.index - Math.floor(ctxN / 2));
      const to = m.index + m[0].length + Math.ceil(ctxN / 2);
      items.push({
        match: m[0],
        context: full.slice(from, to).replace(/\s+/g, " ").trim(),
        tag: el ? el.tagName.toLowerCase() : "",
        id: el && el.id ? el.id : undefined,
        offset: m.index,
      });
      if (!m[0].length) re.lastIndex++;
    }
  }
  return {found, items, truncated: found > items.length};
})()"#;

const QUERY_JS: &str = r#"(()=>{
  const sel = "@@SEL@@", max = @@MAX@@, withText = @@TEXT@@, attrs = @@ATTRS@@;
  let nodes;
  try { nodes = document.querySelectorAll(sel); }
  catch(e) { return { error: "invalid selector: " + e.message }; }
  const total = nodes.length;
  const items = [];
  for (let i = 0; i < Math.min(total, max); i++) {
    const n = nodes[i];
    const it = { index: i, tag: n.tagName.toLowerCase() };
    if (n.id) it.id = n.id;
    if (withText) {
      const t = (n.innerText || n.textContent || "").trim().slice(0, 100);
      if (t) it.text = t;
    }
    if (attrs) {
      const o = {};
      for (const a of attrs) {
        const v = n.getAttribute(a);
        if (v !== null) o[a] = v.slice(0, 300);
      }
      it.attrs = o;
    }
    items.push(it);
  }
  return { total: total, returned: items.length, items: items };
})()"#;

const DROPDOWN_JS: &str = r#"(()=>{
  const sel = "@@SEL@@", action = "@@ACT@@", wantText = @@TXT@@, wantValue = @@VAL@@;
  let el;
  try { el = document.querySelector(sel); }
  catch(e) { return { error: "invalid selector: " + e.message }; }
  if (!el) return { error: "no element matches " + sel };
  if (action === "list") {
    if (el.tagName === "SELECT") {
      const opts = Array.from(el.options).map((o, i) => ({
        index: i, text: o.text.trim(), value: o.value,
        selected: o.selected, disabled: o.disabled
      }));
      return { kind: "select", count: opts.length, multiple: el.multiple,
               value: el.value, options: opts.slice(0, 200) };
    }
    const opts = Array.from(el.querySelectorAll('[role="option"], [role="menuitem"], [role="menuitemradio"]'));
    if (!opts.length) return { error: "element is neither a <select> nor an ARIA menu with role=option children" };
    return { kind: "aria", count: opts.length,
             options: opts.slice(0, 200).map((o, i) => ({
               index: i, text: (o.innerText || "").trim().slice(0, 120),
               selected: o.getAttribute("aria-selected") === "true"
             })) };
  }
  if (action !== "select") return { error: "action must be list|select, got " + action };
  if (el.tagName === "SELECT") {
    const opts = Array.from(el.options);
    let idx = -1;
    if (wantText !== null) idx = opts.findIndex(o => o.text.trim() === wantText);
    if (idx < 0 && wantValue !== null) idx = opts.findIndex(o => o.value === wantValue);
    if (idx < 0) {
      return { error: "no option matches " + (wantText !== null ? "text " + JSON.stringify(wantText) : "value " + JSON.stringify(wantValue)),
               available: opts.slice(0, 50).map(o => o.text.trim()) };
    }
    const o = opts[idx];
    if (o.disabled) return { error: "matched option is disabled" };
    el.selectedIndex = idx;
    el.dispatchEvent(new Event("input", { bubbles: true }));
    el.dispatchEvent(new Event("change", { bubbles: true }));
    return { ok: true, kind: "select", text: o.text.trim(), value: o.value };
  }
  const opts = Array.from(el.querySelectorAll('[role="option"], [role="menuitem"], [role="menuitemradio"]'));
  const m = opts.find(o => (o.innerText || "").trim() === wantText)
         || (wantValue !== null ? opts.find(o => (o.innerText || "").trim() === wantValue) : undefined);
  if (!m) return { error: "no ARIA option matches the requested text/value" };
  m.click();
  return { ok: true, kind: "aria", text: (m.innerText || "").trim() };
})()"#;

const SCROLL_TEXT_JS: &str = r#"(()=>{
  const target = "@@TXT@@", dir = "@@DIR@@";
  const hits = [];
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  let n;
  while ((n = walker.nextNode())) {
    const v = n.nodeValue || "";
    if (v.includes(target) && n.parentElement) hits.push(n.parentElement);
  }
  if (!hits.length) return { found: false, error: "text not present in rendered DOM" };
  const el = dir === "up" ? hits[hits.length - 1] : hits[0];
  el.scrollIntoView({ block: "center", behavior: "instant" });
  const r = el.getBoundingClientRect();
  return { found: true, occurrences: hits.length, tag: el.tagName.toLowerCase(),
           inViewport: r.top >= -2 && r.bottom <= window.innerHeight + 2,
           rect: { x: Math.round(r.left), y: Math.round(r.top),
                   w: Math.round(r.width), h: Math.round(r.height) },
           scrollY: Math.round(window.scrollY) };
})()"#;

const TOKENS_JS: &str = r#"(()=>{
  const found = [];
  const push = (where, raw) => {
    if (typeof raw !== 'string' || raw.length < 16) return;
    const jwt = raw.match(/eyJ[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}\.[A-Za-z0-9_-]{5,}/g);
    if (jwt) for (const t of jwt)
      found.push({ kind: 'jwt', where, preview: t.slice(0, 16) + '…', len: t.length });
    if (/^(sk-|pk_|ghp_|gho_|ghu_|xoxb-|xoxp-|AKIA[0-9A-Z]{12})/.test(raw))
      found.push({ kind: 'api-key-pattern', where, preview: raw.slice(0, 10) + '…', len: raw.length });
    else if (/^[0-9a-f]{40,}$/i.test(raw) && raw.length >= 40)
      found.push({ kind: 'hex-token', where, preview: raw.slice(0, 12) + '…', len: raw.length });
  };
  for (const [area, store] of [['localStorage', localStorage], ['sessionStorage', sessionStorage]]) {
    try { for (let i = 0; i < store.length; i++) {
      const k = store.key(i); push(area + ':' + k, store.getItem(k));
    } } catch (e) {}
  }
  try { document.cookie.split('; ').forEach(p => {
    const eq = p.indexOf('='); if (eq > 0) push('cookie:' + p.slice(0, eq), p.slice(eq + 1));
  }); } catch (e) {}
  document.querySelectorAll('meta[name*="csrf" i],meta[name*="token" i],meta[name*="key" i]')
    .forEach(m => push('meta:' + m.getAttribute('name'), m.content || ''));
  document.querySelectorAll('input[type="hidden"]')
    .forEach(inp => push('input:' + (inp.name || inp.id || '?'), inp.value || ''));
  return { count: found.length, tokens: found.slice(0, 200),
    note: 'values are redacted — previews only, full secrets never leave the page' };
})()"#;

const ANTIBOT_JS: &str = r#"(()=>{
  const f = [];
  const add = (level, msg) => f.push({ level, msg });
  if (navigator.webdriver) add('critical', 'navigator.webdriver = true');
  if (/HeadlessChrome|PhantomJS|Playwright/i.test(navigator.userAgent))
    add('critical', 'headless/automation user-agent');
  let html = '';
  try { html = document.documentElement.outerHTML.slice(0, 2000000); } catch (e) {}
  const widgets = [['cf-chl', 'cloudflare challenge'], ['cf_turnstile', 'cloudflare turnstile'],
    ['turnstile', 'turnstile'], ['hcaptcha', 'hCaptcha'], ['g-recaptcha', 'Google reCAPTCHA'],
    ['geetest', 'GeeTest'], ['altcha', 'ALTCHA'], ['funcaptcha', 'FunCaptcha'],
    ['arkoselabs', 'Arkose'], ['text-captcha', 'text captcha'], ['_7fb9', 'antibot script'],
    ['datadome', 'DataDome'], ['perimeterx', 'PerimeterX'], ['shape-fp', 'Shape'],
    ['kasada', 'Kasada'], ['queue-it', 'queue-it'], ['imperva', 'Imperva/Incapsula'],
    ['recaptcha/api', 'recaptcha script'], ['challenges.cloudflare.com', 'CF challenge iframe']];
  for (const [n, label] of widgets) if (html.includes(n)) add('info', label + ' detected');
  const ifr = [...document.querySelectorAll('iframe')].map(x => x.src || '').join(' ');
  if (/hcaptcha\.com|recaptcha|challenges\.cloudflare\.com|geetest/.test(ifr))
    add('info', 'challenge iframe present');
  try {
    if (!navigator.plugins || navigator.plugins.length === 0)
      add('warn', 'no browser plugins (headless-like)');
  } catch (e) {}
  try {
    if (!navigator.languages || navigator.languages.length === 0)
      add('warn', 'empty navigator.languages');
  } catch (e) {}
  try {
    const c = document.createElement('canvas'); const gl = c.getContext('webgl');
    if (!gl) add('warn', 'WebGL unavailable (fingerprint signal)');
  } catch (e) {}
  let score = 0;
  for (const x of f) score += x.level === 'critical' ? 40 : (x.level === 'warn' ? 15 : 5);
  score = Math.min(100, score);
  return { score, risk: score >= 55 ? 'high' : (score >= 25 ? 'medium' : 'low'),
    findings: f, ua: navigator.userAgent,
    webdriver: !!navigator.webdriver,
    hw_concurrency: navigator.hardwareConcurrency,
    device_memory: navigator.deviceMemory || null,
    screen: screen.width + 'x' + screen.height + '@' + (window.devicePixelRatio || 1),
    timezone: (Intl.DateTimeFormat().resolvedOptions() || {}).timeZone || null,
    platform: navigator.platform, languages: navigator.languages };
})()"#;

#[tool_router]
impl GhostcloakServer {
    #[tool(
        description = "Create a new browsing session: launches the engine with a fresh coherent identity. Returns session_id."
    )]
    async fn session_create(
        &self,
        Parameters(SessionCreateParams {
            platform,
            profile_dir,
            proxy,
            headful,
        }): Parameters<SessionCreateParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        // Strict platform validation with friendly aliases: a typo used to
        // fall through silently and generate a random identity (so "mac"
        // gave you an Android phone).
        let platform = match platform.as_deref().map(|s| s.trim().to_ascii_lowercase()) {
            None => None,
            Some(p) if p.is_empty() => None,
            Some(p) => match p.as_str() {
                "windows" | "win" => Some(ghostcloak_fingerprint::Platform::Windows),
                "macos" | "mac" | "darwin" | "osx" => Some(ghostcloak_fingerprint::Platform::MacOS),
                "linux" => Some(ghostcloak_fingerprint::Platform::Linux),
                "android" | "mobile" => Some(ghostcloak_fingerprint::Platform::Android),
                other => {
                    return Err(rmcp::model::ErrorData::invalid_params(
                        format!(
                            "invalid platform `{other}` — use windows | macos | linux | android"
                        ),
                        None,
                    ))
                }
            },
        };
        let gen_opts = ghostcloak_fingerprint::GenerateOptions {
            platform,
            webrtc: None,
        };
        let identity = ghostcloak_fingerprint::generate(&gen_opts);

        let mut launch = ghostcloak_core::engine::LaunchOptions {
            profile_dir,
            proxy,
            headless: !headful.unwrap_or(false),
            ..Default::default()
        };

        // Camoufox is the primary engine: patched-Firefox spoofing at the
        // C++ level. Identity is injected via CAMOU_CONFIG env at launch,
        // so generate-then-launch isn't a race — launch reads the identity
        // it finds in the (optional) profile dir or generates its own.
        //
        // Write identity.toml only when it doesn't exist yet: overwriting
        // it on every session_create would re-randomize the fingerprint of
        // a persistent profile, leaving cookies from device A behind a
        // fingerprint from device B — an incoherent identity.
        if let Some(dir) = &launch.profile_dir {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir)
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            let identity_file = dir.join("identity.toml");
            if !identity_file.exists() {
                let _ = identity
                    .to_toml()
                    .map(|t| std::fs::write(&identity_file, t));
            }
        }
        // The engine MUST run with the identity we just generated (the one
        // we record as evidence) — pass it explicitly so launch() never
        // re-randomizes into a different fingerprint.
        if launch.identity_toml.is_none() {
            launch.identity_toml = identity.to_toml().ok();
        }
        let engine = ghostcloak_camoufox::launch(&launch)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let session = Session::from_engine(
            ghostcloak_core::util::short_id(),
            identity.label.clone(),
            EngineKind::Firefox,
            engine,
        );
        let id = session.id.clone();
        // Evidence: record the persona this run uses, plus the launch facts.
        {
            let ev = serde_json::json!({
                "label": identity.label,
                "platform": format!("{:?}", identity.platform),
                "identity_hash": identity.fingerprint_hash(),
                "profile_dir": launch.profile_dir,
                "proxy": launch.proxy.is_some(),
                "engine": "ghostfox",
            });
            let _ = self
                .recorder
                .record_identity(&id, &identity.to_toml().unwrap_or_default());
            let _ = self.recorder.record(&id, "session_create", None, ev);
        }
        self.state
            .write()
            .await
            .sessions
            .insert(id.clone(), Arc::new(session));
        // Live view (opt-in via GHOSTFOX_LIVE_VIEW_PORT): starts once, on the
        // first session.
        crate::liveview::start_if_configured(self.state.clone());
        Ok(text_result(id))
    }

    #[tool(
        description = "Close a browsing session: shuts down the engine process, frees its memory (RAM), and deletes its ephemeral profile. ALWAYS call this when done with a session — sessions that are never closed keep consuming hundreds of MB until the server restarts. Returns 'closed'."
    )]
    async fn session_close(
        &self,
        Parameters(SessionCloseParams { session_id }): Parameters<SessionCloseParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        // Remove from the registry FIRST so no new tool call can grab a
        // handle mid-shutdown, then stop the engine (SIGTERM + profile
        // cleanup happens inside shutdown()).
        let session = self
            .state
            .write()
            .await
            .sessions
            .remove(&session_id)
            .ok_or_else(|| {
                rmcp::model::ErrorData::invalid_params(
                    format!("session `{session_id}` not found"),
                    None,
                )
            })?;
        let page_count = session.page_ids().await.len();
        let shutdown_result = session.engine().shutdown().await;
        let _ = self.recorder.record(
            &session_id,
            "session_close",
            None,
            serde_json::json!({ "pages": page_count }),
        );
        shutdown_result.map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        Ok(text_result(format!("closed ({page_count} pages)")))
    }

    #[tool(
        description = "List all active browsing sessions with their identity label and open page count. Use to find sessions that should be closed (session_close) — never leave idle sessions running."
    )]
    async fn session_list(&self) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let state = self.state.read().await;
        let mut out = Vec::new();
        for (id, sess) in state.sessions.iter() {
            out.push(serde_json::json!({
                "session_id": id,
                "identity": sess.identity,
                "pages": sess.page_ids().await.len(),
            }));
        }
        drop(state);
        Ok(text_result(
            serde_json::to_string_pretty(&out).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "Navigate to a URL in an existing session. Returns a page_id (string) that must be passed to all subsequent page tools. Waits for the page to load. If the page has iframes or shadow DOM, use page_a11y instead of guessing CSS selectors. Example: page_open(session_id, 'https://example.com') returns a page_id like 'abc123'."
    )]
    async fn page_open(
        &self,
        Parameters(PageOpenParams { session_id, url }): Parameters<PageOpenParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        // Scheme allowlist: file:// let any authenticated client read local
        // files (e.g. /etc/passwd) straight out of the browser; javascript:
        // URLs execute in the page. Only real web content is allowed.
        let scheme_ok = {
            let u = url.trim().to_ascii_lowercase();
            u.starts_with("http://")
                || u.starts_with("https://")
                || u.starts_with("about:")
                || u.starts_with("data:text/html")
        };
        if !scheme_ok {
            return Err(rmcp::model::ErrorData::invalid_params(
                format!(
                    "URL scheme not allowed: `{url}` — use http:// or https:// (file:// and javascript: are blocked)"
                ),
                None,
            ));
        }
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        // The session's page registry is the id authority; new_page returns
        // the handle and registers it — we surface its id via the registry.
        let page_ids_before = session.page_ids().await;
        let _page = session
            .new_page(Some(&url))
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let page_ids_after = session.page_ids().await;
        let new_id = page_ids_after
            .into_iter()
            .find(|id| !page_ids_before.contains(id))
            .unwrap_or_default();
        // Wait for a usable DOM: returning before document.body exists made
        // every follow-up call (page_a11y, page_eval) fail with
        // "document.body is null" on slower loads.
        if !new_id.is_empty() {
            if let Ok(page) = session.page(&new_id).await {
                for _ in 0..60 {
                    match page.evaluate("!!document.body").await {
                        Ok(v) if v.as_bool() == Some(true) => break,
                        _ => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
                    }
                }
                // Default dialog policy: accept everything — a bare alert()
                // otherwise wedges every subsequent eval on this page.
                // Best-effort: never fail the open over the dialog handler.
                if let Err(e) = page.dialog_setup(true, Some(String::new())).await {
                    tracing::warn!(
                        target: "ghostcloak::dialog",
                        "page_open auto-arm dialog handler failed for {new_id}: {e}"
                    );
                }
            }
        }
        let _ = self.recorder.record(
            &session_id,
            "page_open",
            Some(&new_id),
            serde_json::json!({ "url": url }),
        );
        Ok(text_result(new_id))
    }

    #[tool(
        description = "Go BACK one entry in this page's browser history (like the browser back button). Waits for the destination page's DOM to be ready. Returns the new URL."
    )]
    async fn page_back(
        &self,
        Parameters(PageRefParams {
            session_id,
            page_id,
        }): Parameters<PageRefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        // NATIVE Page.goBack — `history.back()` through evaluate wedges the
        // content process (page stopped responding to every eval).
        let went = page
            .go_back()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        if !went {
            return Ok(text_result("no history to go back to"));
        }
        wait_for_dom(&page).await;
        let url = settled_url(&page).await;
        let _ = self.recorder.record(
            &session_id,
            "page_back",
            Some(&page_id),
            serde_json::json!({ "url": url }),
        );
        Ok(text_result(url))
    }

    #[tool(
        description = "Go FORWARD one history entry (after page_back). NATIVE engine call — returns 'nothing to go forward to' when at the end. Returns the URL."
    )]
    async fn page_forward(
        &self,
        Parameters(PageRefParams {
            session_id,
            page_id,
        }): Parameters<PageRefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let went = page
            .go_forward()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        if !went {
            return Ok(text_result("nothing to go forward to"));
        }
        wait_for_dom(&page).await;
        let url = settled_url(&page).await;
        let _ = self.recorder.record(
            &session_id,
            "page_forward",
            Some(&page_id),
            serde_json::json!({ "url": url }),
        );
        Ok(text_result(url))
    }

    #[tool(
        description = "Reload the current page (like the browser refresh button). NATIVE engine call; waits for the DOM to be ready afterwards. Returns the URL."
    )]
    async fn page_reload(
        &self,
        Parameters(PageRefParams {
            session_id,
            page_id,
        }): Parameters<PageRefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        // NATIVE Page.reload — `location.reload()` through evaluate has the
        // same wedge risk as history.back().
        page.reload_page()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        wait_for_dom(&page).await;
        let url = settled_url(&page).await;
        let _ = self.recorder.record(
            &session_id,
            "page_reload",
            Some(&page_id),
            serde_json::json!({ "url": url }),
        );
        Ok(text_result(url))
    }

    #[tool(
        description = "Close a single page/tab in a session (frees its target). Use session_close to shut the whole session down."
    )]
    async fn page_close(
        &self,
        Parameters(PageRefParams {
            session_id,
            page_id,
        }): Parameters<PageRefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        session
            .close_page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_close",
            Some(&page_id),
            serde_json::json!({}),
        );
        Ok(text_result(format!("page {page_id} closed")))
    }

    #[tool(
        description = "Extract the visible text content of a page as plain text (token-friendly). Returns: url, title, and content (all visible text, no HTML). For semantic element data with refs and values, use page_a11y instead — it gives you interactive elements with roles and names. Use this when you just need to READ page content without needing to interact with elements."
    )]
    async fn page_snapshot(
        &self,
        Parameters(PageRefParams {
            session_id,
            page_id,
        }): Parameters<PageRefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let snap = page
            .snapshot()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        // Evidence: persist the full snapshot for replay/audit.
        let _ = self.recorder.record_snapshot(&session_id, &page_id, &snap);
        Ok(text_result(
            serde_json::to_string_pretty(&snap).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "READ-ONLY: Retrieve the complete audit trail for a session. No side effects — does not modify the session or browser. Returns JSON with: session_id, dir (evidence directory path), identity_toml (path to the identity file used), event_count (total tool calls recorded), events (array of all events with timestamps), and snapshots (list of page snapshot file paths). The session_id must come from a previous session_create call. For a nonexistent session, returns an error. Evidence persists after the session ends and includes: append-only events.jsonl, page snapshots (markdown), screenshots (PNG), and identity.toml. Use session_create first if you need a session."
    )]
    async fn session_evidence(
        &self,
        Parameters(SessionEvidenceParams { session_id }): Parameters<SessionEvidenceParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        match self.recorder.evidence(&session_id) {
            Ok(ev) => Ok(text_result(
                serde_json::to_string_pretty(&ev).unwrap_or_default(),
            )),
            Err(e) => Err(rmcp::model::ErrorData::internal_error(
                format!("no evidence for session `{session_id}`: {e}"),
                None,
            )),
        }
    }

    #[tool(
        description = "Semantic snapshot of the page: every visible interactive element with a stable ref, role (button/link/textbox/...), accessible name and CURRENT value — pierces shadow DOM, so web-component UIs (Reddit, modern frameworks) are fully visible. Use this instead of guessing CSS selectors."
    )]
    async fn page_a11y(
        &self,
        Parameters(PageRefParams {
            session_id,
            page_id,
        }): Parameters<PageRefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let snap = page
            .a11y_snapshot()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_a11y",
            Some(&page_id),
            serde_json::json!({
                "elements": snap.elements.len(),
                "login_state": snap.login_state,
            }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&snap).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "Click an element by its ref from page_a11y. Scrolls it into view first. No selectors needed."
    )]
    async fn page_click_ref(
        &self,
        Parameters(RefParams {
            session_id,
            page_id,
            r#ref,
        }): Parameters<RefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        page.click_ref(&r#ref)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_click_ref",
            Some(&page_id),
            serde_json::json!({ "ref": r#ref }),
        );
        Ok(text_result("clicked"))
    }

    #[tool(
        description = "Move the mouse onto an element (hover) along a HUMAN-LIKE path: bezier arc, ease-in-out velocity, sub-pixel tremor, overshoot+correction. Triggers hover menus and tooltips. Pass the ref from page_a11y."
    )]
    async fn page_move_to(
        &self,
        Parameters(RefParams {
            session_id,
            page_id,
            r#ref,
        }): Parameters<RefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        page.mouse_move_to(&r#ref)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_move_to",
            Some(&page_id),
            serde_json::json!({ "ref": r#ref }),
        );
        Ok(text_result("moved"))
    }

    #[tool(
        description = "Drag with a HUMAN-LIKE movement profile: approaches the source, presses, drags along a bezier arc with ease-in-out velocity and micro-pauses, settles, releases. Pass from_ref + to_ref to drag element onto element, or from_ref + offset_x/offset_y to drag by pixels (slider captchas, resize handles). All from page_a11y refs."
    )]
    async fn page_drag(
        &self,
        Parameters(DragParams {
            session_id,
            page_id,
            from_ref,
            to_ref,
            offset_x,
            offset_y,
        }): Parameters<DragParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let to = to_ref.unwrap_or_default();
        page.drag_ref(
            &from_ref,
            &to,
            offset_x.unwrap_or(0.0),
            offset_y.unwrap_or(0.0),
        )
        .await
        .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_drag",
            Some(&page_id),
            serde_json::json!({
                "from": from_ref,
                "to": to,
                "offset_x": offset_x.unwrap_or(0.0),
                "offset_y": offset_y.unwrap_or(0.0),
            }),
        );
        Ok(text_result("dragged"))
    }

    #[tool(
        description = "SUPERMAN GLASSES: render an element (canvas / img / background-image) as a compact luminance GRID of digits 0-9 the agent READS directly — see shapes, holes, object orientation, image layout WITHOUT a vision model. 0=black, 9=white. Captcha gaps appear as darker cells, upright skies are bright rows on top. Pass the ref from page_a11y. Returns JSON {w, h, grid:[rows of digits]}."
    )]
    async fn page_pixels(
        &self,
        Parameters(PixelsParams {
            session_id,
            page_id,
            r#ref,
            grid_w,
            grid_h,
        }): Parameters<PixelsParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let gw = grid_w.unwrap_or(32).clamp(4, 128);
        let gh = grid_h.unwrap_or(21).clamp(4, 128);
        let grid = page
            .pixels_ref(&r#ref, gw, gh)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_pixels",
            Some(&page_id),
            serde_json::json!({ "ref": r#ref, "grid_w": gw, "grid_h": gh }),
        );
        Ok(text_result(grid))
    }

    #[tool(
        description = "INIT SCRIPTS: run your JS at DOCUMENT START on every navigation — BEFORE any page script loads. The deepest interception layer in Ghostfox: hooks installed here are what page libraries (gt.js, analytics, frameworks) capture, so their stored references are YOURS. Use for: API response interception (JSONP callback swaps), state capture, anti-detection instrumentation, environment patches. Applies to subsequent navigations."
    )]
    async fn page_init_script(
        &self,
        Parameters(InitScriptParams {
            session_id,
            page_id,
            source,
        }): Parameters<InitScriptParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        page.add_init_script(&source)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_init_script",
            Some(&page_id),
            serde_json::json!({ "chars": source.chars().count() }),
        );
        Ok(text_result("init script registered"))
    }

    #[tool(
        description = "PROTOCOL-LEVEL NETWORK CAPTURE: start recording every HTTP response for this page, BELOW the page (invisible to page JS, unpatchable). The answer data of any captcha/API travels here. Start before triggering the flow you want to see."
    )]
    async fn page_network_start(
        &self,
        Parameters(NetParams {
            session_id,
            page_id,
            ..
        }): Parameters<NetParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        page.net_capture_start()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        Ok(text_result("capture started"))
    }

    #[tool(
        description = "List captured HTTP responses [{url, requestId}] since capture start. Optionally filter by URL substring (e.g. 'geetest' or 'api.'). Pair with page_network_body to read the response content BY PROTOCOL."
    )]
    async fn page_network_read(
        &self,
        Parameters(NetParams {
            session_id,
            page_id,
            filter,
            clear,
        }): Parameters<NetParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let entries = page
            .net_read(clear.unwrap_or(false))
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let filtered: Vec<serde_json::Value> = entries
            .into_iter()
            .filter(|e| {
                filter.as_deref().is_none_or(|f| {
                    e.get("url")
                        .and_then(|u| u.as_str())
                        .unwrap_or("")
                        .contains(f)
                })
            })
            .collect();
        Ok(text_result(
            serde_json::to_string_pretty(&filtered).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "Fetch the BODY of a captured response by requestId — Network.getResponseBody BY PROTOCOL. The JSONP/API answer data that page JS can't see, served from the engine. THIS is the 0s and 1s layer."
    )]
    async fn page_network_body(
        &self,
        Parameters(NetBodyParams {
            session_id,
            page_id,
            request_id,
        }): Parameters<NetBodyParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let body = page
            .net_get_body(&request_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        Ok(text_result(body))
    }

    #[tool(
        description = "REAL TEMPLATE MATCHING: multi-scale normalized cross-correlation of a needle against a haystack, computed IN-PAGE at full grayscale resolution (no grid loss). Returns top matches [{x, y, score, scale}] — needle-center positions in haystack-image pixels. needle_rect optionally crops the needle (e.g. an instruction glyph band from the same image — GeeTest icon-click pattern). Images are cached per URL: single-use challenge URLs fetch exactly once. THE tool for: captcha piece->gap, glyph->character, logo->page."
    )]
    async fn page_match_image(
        &self,
        Parameters(MatchImageParams {
            session_id,
            page_id,
            needle_ref,
            needle_rect,
            hay_ref,
            hay_rect,
        }): Parameters<MatchImageParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let rect = needle_rect.unwrap_or_default();
        let (nx, ny, nw, nh) = match rect.as_slice() {
            [x, y, w, h] => (*x, *y, *w, *h),
            _ => (0, 0, 0, 0),
        };
        let hrect = hay_rect.unwrap_or_default();
        let (hx, hy, hw, hh) = match hrect.as_slice() {
            [x, y, w, h] => (*x, *y, *w, *h),
            _ => (0, 0, 0, 0),
        };
        let result = page
            .match_image_ref(&needle_ref, nx, ny, nw, nh, &hay_ref, hx, hy, hw, hh)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_match_image",
            Some(&page_id),
            serde_json::json!({ "needle": needle_ref, "hay": hay_ref }),
        );
        Ok(text_result(result))
    }

    #[tool(
        description = "HIGH-PASS VISION: local-contrast grid of an element's image (|gray - gaussian_blur|). Makes ANYTHING blended into a background VISIBLE — captcha characters on photos, watermarks, hidden strokes. 0 = flat area, 9 = strong edge. This is the native tool born from solving GeeTest icon-click with pure math. Works on canvas / img / background-image (one fetch per challenge)."
    )]
    async fn page_contrast(
        &self,
        Parameters(ContrastParams {
            session_id,
            page_id,
            r#ref,
            grid_w,
            grid_h,
            blur_radius,
        }): Parameters<ContrastParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let gw = grid_w.unwrap_or(100).clamp(4, 256);
        let gh = grid_h.unwrap_or(44).clamp(4, 256);
        let radius = blur_radius.unwrap_or(4).clamp(1, 16);
        let grid = page
            .contrast_ref(&r#ref, gw, gh, radius)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_contrast",
            Some(&page_id),
            serde_json::json!({ "ref": r#ref, "grid_w": gw, "grid_h": gh }),
        );
        Ok(text_result(grid))
    }

    /// Shared helper: produce PNG bytes of the whole viewport or the
    /// element a ref points at (screenshot + crop).
    async fn element_png(
        &self,
        session_id: &str,
        page_id: &str,
        r#ref: Option<String>,
    ) -> Result<Vec<u8>, rmcp::model::ErrorData> {
        let session = self
            .session(session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let png = page
            .screenshot(false)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        match r#ref {
            None => Ok(png),
            Some(r) => {
                // Rect of the element (CSS px) + viewport width + DPR.
                let js = format!(
                    r#"(function() {{
  var el = (window.__gfxRefs || new Map()).get({r});
  if (!el || !el.isConnected) {{
    try {{ el = document.querySelector({r}); }} catch (e) {{ el = null; }}
  }}
  if (!el || !el.isConnected) return 'STALE-REF';
  var r = el.getBoundingClientRect();
  return JSON.stringify({{x: r.x, y: r.y, w: r.width, h: r.height, vw: window.innerWidth}});
}})()"#,
                    r = serde_json::to_string(&r).unwrap_or_default()
                );
                let out = page
                    .evaluate(&js)
                    .await
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                let s = out.as_str().unwrap_or("STALE-REF");
                if s == "STALE-REF" {
                    return Err(rmcp::model::ErrorData::internal_error(
                        format!("ref {r} is stale — rerun page_a11y"),
                        None,
                    ));
                }
                let rect: serde_json::Value = serde_json::from_str(s)
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                let img = image::load_from_memory(&png)
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                let vw = rect.get("vw").and_then(|v| v.as_f64()).unwrap_or(1.0);
                let scale = img.width() as f64 / vw.max(1.0);
                let x = (rect.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0) * scale) as i64;
                let y = (rect.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0) * scale) as i64;
                let w = (rect.get("w").and_then(|v| v.as_f64()).unwrap_or(0.0) * scale) as i64;
                let h = (rect.get("h").and_then(|v| v.as_f64()).unwrap_or(0.0) * scale) as i64;
                let (x, y) = (x.max(0) as u32, y.max(0) as u32);
                let (w, h) = (
                    (w.max(1) as u32).min(img.width().saturating_sub(x)),
                    (h.max(1) as u32).min(img.height().saturating_sub(y)),
                );
                let cropped = image::imageops::crop_imm(&img, x, y, w, h).to_image();
                let mut buf = std::io::Cursor::new(Vec::new());
                cropped
                    .write_to(&mut buf, image::ImageFormat::Png)
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                Ok(buf.into_inner())
            }
        }
    }

    #[tool(
        description = "TIER 2 VISION — EXTRACT TEXT: capture the text region of an element (or the whole viewport if no ref) and return the saved PNG for VISION-BASED reading. TEMPORARY behavior: local ocrs recognition is unreliable on aarch64, so instead of returning extracted text this returns {file, bytes} — the caller (an AI agent with a vision model) reads the image directly. For captcha images use page_captcha_ocr or captcha_solve (local ddddocr, accurate)."
    )]
    async fn page_ocr(
        &self,
        Parameters(OcrParams {
            session_id,
            page_id,
            r#ref,
        }): Parameters<OcrParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let png = self.element_png(&session_id, &page_id, r#ref).await?;
        // TEMPORARY (2026-09-29): ocrs recognition output is scrambled on
        // aarch64 (rten 0.26 numeric issue with these models — model files
        // hash-verified intact, screenshots vision-verified correct), so
        // text extraction is delegated to the caller's vision model: save
        // the captured region and return its path instead of garbage text.
        let path = self
            .recorder
            .record_screenshot(&session_id, &page_id, &png)
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_ocr",
            Some(&page_id),
            serde_json::json!({ "vision_delegated": true, "bytes": png.len() }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&serde_json::json!({
                "file": path.to_string_lossy(),
                "bytes": png.len(),
                "hint": "local OCR is degraded on this architecture (temporary) — read the image with your vision model; for captcha images use page_captcha_ocr (local ddddocr, accurate)",
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "TIER 4 VISION — VISUAL CORTEX: detect word bounding boxes in an element (or viewport) via the built-in text-detection ML model. Returns JSON [{x, y, w, h}, ...] in image pixels. The first shipped visual-cortex model — see where text lives in ANY image (click-order captchas, canvas text, scanned layout). 100% local."
    )]
    async fn page_vision(
        &self,
        Parameters(VisionParams {
            session_id,
            page_id,
            r#ref,
        }): Parameters<VisionParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let png = self.element_png(&session_id, &page_id, r#ref).await?;
        let boxes = crate::ocr::detect_text_boxes(&png)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_vision",
            Some(&page_id),
            serde_json::json!({ "words_found": boxes.len() }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&boxes).unwrap_or_default(),
        ))
    }

    // ===== BATCH TOOLS (2026-09-29: extraction/scraping/debug layer) =====

    #[tool(
        description = "STRUCTURED EXTRACT: pull data out of the page as JSON. Auto mode (no selector) finds every table (headers+rows) and list; with a selector it extracts matched elements {tag, text, href, src, value}. One call replaces manual scraping loops. Returns {tables, lists, items, counts}."
    )]
    async fn page_extract(
        &self,
        Parameters(ExtractParams {
            session_id,
            page_id,
            selector,
        }): Parameters<ExtractParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let sel = selector.unwrap_or_default();
        let expr = EXTRACT_JS.replace("@@SEL@@", &js_escape(&sel));
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: expr,
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "page_extract",
            Some(&page_id),
            serde_json::json!({ "selector": sel }),
        );
        Ok(r)
    }

    #[tool(
        description = "PAGE TO MARKDOWN: convert the page (or a subtree via selector) into clean Markdown — headings, links, lists, code, tables, images. Auto-picks article > main > body. Token-friendly way to feed a whole page to an LLM. Returns {markdown, chars}."
    )]
    async fn page_markdown(
        &self,
        Parameters(MarkdownParams {
            session_id,
            page_id,
            selector,
        }): Parameters<MarkdownParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let sel = selector.unwrap_or_default();
        let expr = MARKDOWN_JS.replace("@@SEL@@", &js_escape(&sel));
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: expr,
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "page_markdown",
            Some(&page_id),
            serde_json::json!({ "selector": sel }),
        );
        Ok(r)
    }

    #[tool(
        description = "BATCH ACTIONS: run an ordered list of page actions in ONE MCP call (fewer round-trips). actions: [{action, ...}] where action = click (selector|ref), click_ref (ref), type (selector+text), fill (selector+text), press (key), eval (expression), wait (selector, timeout_ms?), move_to (ref), back, forward, reload. Stops at the first failure and reports which step broke."
    )]
    async fn page_batch(
        &self,
        Parameters(BatchParams {
            session_id,
            page_id,
            actions,
        }): Parameters<BatchParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        if actions.is_empty() {
            return Err(rmcp::model::ErrorData::invalid_params(
                "actions must not be empty",
                None,
            ));
        }
        let mut done: Vec<serde_json::Value> = Vec::new();
        for (i, a) in actions.iter().enumerate() {
            let action = a.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let gs = |k: &str| a.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
            let res: Result<CallToolResult, rmcp::model::ErrorData> = match action {
                "click" => match (gs("selector"), gs("ref")) {
                    (Some(sel), _) => {
                        self.page_click(Parameters(PageClickParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            selector: sel,
                        }))
                        .await
                    }
                    (None, Some(rf)) => {
                        self.page_click_ref(Parameters(RefParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            r#ref: rf,
                        }))
                        .await
                    }
                    _ => Err(rmcp::model::ErrorData::invalid_params(
                        "click needs selector or ref",
                        None,
                    )),
                },
                "click_ref" => match gs("ref") {
                    Some(rf) => {
                        self.page_click_ref(Parameters(RefParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            r#ref: rf,
                        }))
                        .await
                    }
                    None => Err(rmcp::model::ErrorData::invalid_params(
                        "click_ref needs ref",
                        None,
                    )),
                },
                "type" => match (gs("selector"), gs("text")) {
                    (Some(sel), Some(text)) => {
                        self.page_type(Parameters(PageTypeParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            selector: sel,
                            text,
                        }))
                        .await
                    }
                    _ => Err(rmcp::model::ErrorData::invalid_params(
                        "type needs selector + text",
                        None,
                    )),
                },
                "fill" => match (gs("selector"), gs("text")) {
                    (Some(sel), Some(text)) => {
                        self.page_fill(Parameters(PageFillParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            selector: sel,
                            text,
                        }))
                        .await
                    }
                    _ => Err(rmcp::model::ErrorData::invalid_params(
                        "fill needs selector + text",
                        None,
                    )),
                },
                "press" => match gs("key") {
                    Some(key) => {
                        self.page_press(Parameters(PagePressParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            key,
                        }))
                        .await
                    }
                    None => Err(rmcp::model::ErrorData::invalid_params(
                        "press needs key",
                        None,
                    )),
                },
                "eval" => match gs("expression") {
                    Some(expression) => {
                        self.page_eval(Parameters(PageEvalParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            expression,
                        }))
                        .await
                    }
                    None => Err(rmcp::model::ErrorData::invalid_params(
                        "eval needs expression",
                        None,
                    )),
                },
                "wait" => match gs("selector") {
                    Some(selector) => {
                        self.page_wait_for(Parameters(WaitForParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            selector,
                            timeout_ms: a.get("timeout_ms").and_then(|v| v.as_u64()),
                        }))
                        .await
                    }
                    None => Err(rmcp::model::ErrorData::invalid_params(
                        "wait needs selector",
                        None,
                    )),
                },
                "move_to" => match gs("ref") {
                    Some(rf) => {
                        self.page_move_to(Parameters(RefParams {
                            session_id: session_id.clone(),
                            page_id: page_id.clone(),
                            r#ref: rf,
                        }))
                        .await
                    }
                    None => Err(rmcp::model::ErrorData::invalid_params(
                        "move_to needs ref",
                        None,
                    )),
                },
                "back" => {
                    self.page_back(Parameters(PageRefParams {
                        session_id: session_id.clone(),
                        page_id: page_id.clone(),
                    }))
                    .await
                }
                "forward" => {
                    self.page_forward(Parameters(PageRefParams {
                        session_id: session_id.clone(),
                        page_id: page_id.clone(),
                    }))
                    .await
                }
                "reload" => {
                    self.page_reload(Parameters(PageRefParams {
                        session_id: session_id.clone(),
                        page_id: page_id.clone(),
                    }))
                    .await
                }
                other => Err(rmcp::model::ErrorData::invalid_params(
                    format!("unknown action '{other}' at index {i}"),
                    None,
                )),
            };
            match res {
                Ok(_) => done.push(serde_json::json!({ "i": i, "action": action, "ok": true })),
                Err(e) => {
                    return Err(rmcp::model::ErrorData::internal_error(
                        format!("action #{i} ({action}) failed: {}", e.message),
                        None,
                    ))
                }
            }
        }
        let _ = self.recorder.record(
            &session_id,
            "page_batch",
            Some(&page_id),
            serde_json::json!({ "steps": done.len() }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&serde_json::json!({
                "completed": done.len(),
                "steps": done,
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "WAIT FOR NETWORK IDLE: block until no new HTTP responses arrive for idle_ms (default 800ms), max timeout_ms (default 15000). Replaces manual sleeps after navigation/AJAX — returns {idle, responses, waited_ms}. Requires nothing extra; capture buffer is auto-started."
    )]
    async fn page_wait_for_idle(
        &self,
        Parameters(IdleParams {
            session_id,
            page_id,
            timeout_ms,
            idle_ms,
        }): Parameters<IdleParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(15_000));
        let quiet = std::time::Duration::from_millis(idle_ms.unwrap_or(800));
        let start = std::time::Instant::now();
        let mut last_count: Option<usize> = None;
        let mut last_change = std::time::Instant::now();
        loop {
            let entries = page
                .net_read(false)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            let c = entries.len();
            if last_count != Some(c) {
                last_count = Some(c);
                last_change = std::time::Instant::now();
            }
            if last_change.elapsed() >= quiet {
                return Ok(text_result(
                    serde_json::json!({
                        "idle": true,
                        "responses": c,
                        "waited_ms": start.elapsed().as_millis(),
                    })
                    .to_string(),
                ));
            }
            if start.elapsed() >= timeout {
                return Ok(text_result(
                    serde_json::json!({
                        "idle": false,
                        "responses": c,
                        "waited_ms": start.elapsed().as_millis(),
                        "note": "timeout — traffic still arriving",
                    })
                    .to_string(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        }
    }

    #[tool(
        description = "TOKEN HUNTER: scan localStorage, sessionStorage, cookies, CSRF metas and hidden inputs for JWTs / API-key patterns. Values are REDACTED (preview + length only) — never returns the full secret. For auditing what credentials a page holds."
    )]
    async fn extract_tokens(
        &self,
        Parameters(TokensParams {
            session_id,
            page_id,
        }): Parameters<TokensParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: TOKENS_JS.to_string(),
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "extract_tokens",
            Some(&page_id),
            serde_json::json!({}),
        );
        Ok(r)
    }

    #[tool(
        description = "ANTI-BOT RECON: score the page's bot-detection surface 0-100 — identifies active widgets (Cloudflare, hCaptcha, reCAPTCHA, GeeTest, DataDome, PerimeterX, Imperva, Kasada…), headless signals (webdriver, UA, plugins, languages, WebGL) and returns fingerprint basics (UA, screen, timezone, hw). risk = low|medium|high."
    )]
    async fn detect_anti_bot(
        &self,
        Parameters(AntiBotParams {
            session_id,
            page_id,
        }): Parameters<AntiBotParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: ANTIBOT_JS.to_string(),
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "detect_anti_bot",
            Some(&page_id),
            serde_json::json!({}),
        );
        Ok(r)
    }

    #[tool(
        description = "WEB STORAGE: read/write browser storage from the page. op = get | set | clear | keys; area = local (localStorage, default) | session (sessionStorage) | cookie. get with key returns the value (null if absent); cookie without key returns the full cookie string; keys lists names."
    )]
    async fn page_storage(
        &self,
        Parameters(StorageParams {
            session_id,
            page_id,
            op,
            area,
            key,
            value,
        }): Parameters<StorageParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let op = op.unwrap_or_else(|| "get".to_string());
        let area = area.unwrap_or_else(|| "local".to_string());
        let expr = match (area.as_str(), op.as_str()) {
            ("cookie", "keys") => {
                "JSON.stringify(document.cookie.split('; ').map(c=>c.slice(0,Math.max(0,c.indexOf('=')))))"
                    .to_string()
            }
            ("cookie", "set") => match (&key, &value) {
                (Some(k), Some(v)) => format!(
                    "(()=>{{document.cookie=\"{}={};path=/;max-age=31536000;SameSite=Lax\";return 'ok'}})()",
                    js_escape(k),
                    js_escape(v)
                ),
                _ => {
                    return Err(rmcp::model::ErrorData::invalid_params(
                        "cookie set needs key + value",
                        None,
                    ))
                }
            },
            ("cookie", "clear") => {
                "(()=>{const all=document.cookie.split('; ');for(const c of all){const eq=c.indexOf('=');document.cookie=c.slice(0,Math.max(0,eq))+'=;path=/;max-age=0';}return 'cleared '+all.length})()"
                    .to_string()
            }
            ("cookie", _) => match &key {
                Some(k) => format!(
                    "(()=>{{const n='{};';const all=document.cookie.split('; ');for(const c of all){{if(c.startsWith(n))return c.slice(n.length);}}return null}})()",
                    js_escape(k)
                ),
                None => "document.cookie".to_string(),
            },
            (store, "keys") => {
                let store = if store == "session" { "sessionStorage" } else { "localStorage" };
                format!("JSON.stringify(Object.keys({store}))")
            }
            (store, "set") => {
                let store = if store == "session" { "sessionStorage" } else { "localStorage" };
                match (&key, &value) {
                    (Some(k), Some(v)) => format!(
                        "(()=>{{{}.setItem(\"{}\",\"{}\");return 'ok'}})()",
                        store,
                        js_escape(k),
                        js_escape(v)
                    ),
                    _ => {
                        return Err(rmcp::model::ErrorData::invalid_params(
                            "set needs key + value",
                            None,
                        ))
                    }
                }
            }
            (store, "clear") => {
                let store = if store == "session" { "sessionStorage" } else { "localStorage" };
                format!("(()=>{{{store}.clear();return 'ok'}})()")
            }
            (store, _) => {
                let store = if store == "session" { "sessionStorage" } else { "localStorage" };
                match &key {
                    Some(k) => format!("{store}.getItem(\"{}\")", js_escape(k)),
                    None => {
                        return Err(rmcp::model::ErrorData::invalid_params(
                            "get needs key",
                            None,
                        ))
                    }
                }
            }
        };
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: expr,
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "page_storage",
            Some(&page_id),
            serde_json::json!({ "op": op, "area": area }),
        );
        Ok(r)
    }

    #[tool(
        description = "IN-PAGE HTTP: fetch a URL from INSIDE the page context — runs with the page's cookies, origin and fingerprint (credentials:include). Same-origin API pivots always work; cross-origin still obeys CORS (as any browser would). Use it to call the site's own APIs after login without copying tokens. Returns {status, ok, url, headers, body_len, body} (body capped at 2MB)."
    )]
    async fn page_http(
        &self,
        Parameters(HttpParams {
            session_id,
            page_id,
            url,
            method,
            headers,
            body,
        }): Parameters<HttpParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let method = method.unwrap_or_else(|| "GET".to_string()).to_uppercase();
        let headers_json =
            serde_json::to_string(&headers.unwrap_or_default()).unwrap_or_else(|_| "{}".into());
        let url_json = serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".into());
        // NOTE: synchronous XHR, not fetch — the Juggler Runtime.evaluate
        // scheme has no awaitPromise (async fetch would never resolve).
        let expr = format!(
            "(()=>{{try{{const x=new XMLHttpRequest();x.open('{}',{url_json},false);x.withCredentials=true;const hd={headers_json};for(const k in hd){{x.setRequestHeader(k,hd[k]);}}x.send({body_part_json});const rh={{}};const raw=x.getAllResponseHeaders()||'';for(const line of raw.trim().split(/\\r?\\n/)){{const i=line.indexOf(':');if(i>0)rh[line.slice(0,i).trim().toLowerCase()]=line.slice(i+1).trim();}}const t=x.responseText||'';return JSON.stringify({{status:x.status,ok:x.status>=200&&x.status<300,url:x.responseURL,headers:rh,body_len:t.length,body:t.slice(0,2000000)}});}}catch(e){{return JSON.stringify({{error:String(e)}})}}}})()",
            js_escape(&method),
            body_part_json = match (&body, method.as_str()) {
                (Some(b), m) if m != "GET" && m != "HEAD" => {
                    serde_json::to_string(b).unwrap_or_default()
                }
                _ => "null".to_string(),
            },
        );
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: expr,
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "page_http",
            Some(&page_id),
            serde_json::json!({ "method": method, "url": url }),
        );
        Ok(r)
    }

    #[tool(
        description = "EXPORT HAR: serialize every captured HTTP exchange of this page into a HAR 1.2 log (request url+method, status, response body via Network capture). Start page_network_start first for bodies; without it entries still carry url/method/status. Returns the HAR JSON."
    )]
    async fn export_har(
        &self,
        Parameters(HarParams {
            session_id,
            page_id,
        }): Parameters<HarParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let entries = page
            .net_read(false)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let mut har_entries = Vec::new();
        for e in &entries {
            let url = e.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let method = e.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
            let status = e.get("status").and_then(|v| v.as_i64()).unwrap_or(0);
            let rid = e.get("requestId").and_then(|v| v.as_str());
            let mut body_text: Option<String> = None;
            let mut mime = "application/octet-stream".to_string();
            if let Some(id) = rid {
                if let Ok(b) = page.net_get_body(id).await {
                    mime = "text/plain; charset=utf-8".to_string();
                    body_text = Some(b);
                }
            }
            let ts = e.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
            let started = chrono::DateTime::from_timestamp_millis(ts)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default();
            har_entries.push(serde_json::json!({
                "startedDateTime": started,
                "time": 0,
                "request": {
                    "method": method,
                    "url": url,
                    "httpVersion": "HTTP/1.1",
                    "headers": [],
                    "queryString": [],
                    "cookies": [],
                    "headersSize": -1,
                    "bodySize": -1,
                },
                "response": {
                    "status": status,
                    "statusText": "",
                    "httpVersion": "HTTP/1.1",
                    "headers": [],
                    "cookies": [],
                    "content": {
                        "size": body_text.as_deref().map(|b| b.len()).unwrap_or(0),
                        "mimeType": mime,
                        "text": body_text,
                    },
                    "redirectURL": "",
                    "headersSize": -1,
                    "bodySize": body_text.as_deref().map(|b| b.len() as i64).unwrap_or(-1),
                },
                "cache": {},
                "timings": { "send": 0, "wait": 0, "receive": 0 },
            }));
        }
        let _ = self.recorder.record(
            &session_id,
            "export_har",
            Some(&page_id),
            serde_json::json!({ "entries": har_entries.len() }),
        );
        Ok(text_result(
            serde_json::to_string(&serde_json::json!({
                "log": {
                    "version": "1.2",
                    "creator": { "name": "ghostcloak", "version": "0.8" },
                    "entries": har_entries,
                }
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "NATIVE SCROLL: dispatch REAL mouse-wheel events (Juggler Page.dispatchWheelEvent) instead of JS window.scrollTo — lazy-loaders, infinite feeds and anti-bot scroll detectors see genuine input events. dy px down (negative = up), dx px right; selector scrolls the element into view then wheels at its center (nested containers scroll correctly); steps splits into N events (default 8) for a natural feel. Returns {at, steps, before, after} scroll positions."
    )]
    async fn page_scroll(
        &self,
        Parameters(ScrollParams {
            session_id,
            page_id,
            selector,
            x,
            y,
            dx,
            dy,
            steps,
        }): Parameters<ScrollParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let dy = dy.unwrap_or(600.0);
        let dx = dx.unwrap_or(0.0);
        if dx == 0.0 && dy == 0.0 {
            return Err(rmcp::model::ErrorData::invalid_params(
                "need dy and/or dx (px)",
                None,
            ));
        }
        let expr: String = match (&selector, x, y) {
            (Some(sel), _, _) => format!(
                "(()=>{{const e=document.querySelector('{}');if(!e)return JSON.stringify({{error:'no element for selector'}});e.scrollIntoView({{block:'center',inline:'center'}});const r=e.getBoundingClientRect();return JSON.stringify({{x:Math.round(r.left+r.width/2),y:Math.round(r.top+r.height/2)}})}})()",
                js_escape(sel)
            ),
            (None, Some(px), Some(py)) => format!("JSON.stringify({{x:{px},y:{py}}})"),
            _ => "JSON.stringify({x:Math.round(innerWidth/2),y:Math.round(innerHeight/2)})"
                .to_string(),
        };
        let v = page
            .evaluate(&expr)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let at: serde_json::Value =
            serde_json::from_str(v.as_str().unwrap_or("{}")).unwrap_or(serde_json::json!({}));
        if let Some(e) = at.get("error") {
            return Err(rmcp::model::ErrorData::invalid_params(
                format!("wheel position: {e}"),
                None,
            ));
        }
        let wx = at["x"].as_f64().unwrap_or(600.0);
        let wy = at["y"].as_f64().unwrap_or(400.0);
        let pos_expr =
            "JSON.stringify({x:Math.round(window.scrollX),y:Math.round(window.scrollY)})";
        let b = page
            .evaluate(pos_expr)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let before: serde_json::Value =
            serde_json::from_str(b.as_str().unwrap_or("{}")).unwrap_or(serde_json::json!({}));
        let steps = steps.unwrap_or(8).clamp(1, 64);
        for _ in 0..steps {
            page.dispatch_wheel(wx, wy, dx / steps as f64, dy / steps as f64)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            tokio::time::sleep(std::time::Duration::from_millis(6)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await; // compositor settle
        let a = page
            .evaluate(pos_expr)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let after: serde_json::Value =
            serde_json::from_str(a.as_str().unwrap_or("{}")).unwrap_or(serde_json::json!({}));
        let _ = self.recorder.record(
            &session_id,
            "page_scroll",
            Some(&page_id),
            serde_json::json!({ "dy": dy, "dx": dx, "steps": steps, "selector": selector }),
        );
        Ok(text_result(
            serde_json::json!({
                "at": { "x": wx, "y": wy },
                "steps": steps,
                "before": before,
                "after": after
            })
            .to_string(),
        ))
    }

    #[tool(
        description = "BROWSER-LEVEL COOKIE JAR (root session): get ALL cookies INCLUDING httpOnly (invisible to JS document.cookie / page_storage), set cookies (name+value+url|domain, path, secure, httpOnly, sameSite, expires), or clear the whole jar. action=get supports name/domain filters; action=set takes cookies=[...]. Use to clone a logged-in jar into a fresh context."
    )]
    async fn page_cookies(
        &self,
        Parameters(CookiesParams {
            session_id,
            page_id,
            action,
            name,
            domain,
            cookies,
        }): Parameters<CookiesParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let out = match action.as_str() {
            "get" => {
                let arr = page
                    .browser_cookies("get", serde_json::json!([]))
                    .await
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                let mut list: Vec<serde_json::Value> = arr.as_array().cloned().unwrap_or_default();
                if let Some(n) = &name {
                    list.retain(|c| c.get("name").and_then(|v| v.as_str()) == Some(n.as_str()));
                }
                if let Some(d) = &domain {
                    let dl = d.to_lowercase();
                    list.retain(|c| {
                        c.get("domain")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_lowercase().contains(&dl))
                            .unwrap_or(false)
                    });
                }
                let count = list.len();
                serde_json::json!({ "count": count, "cookies": list })
            }
            "set" => {
                let cookies = cookies.ok_or_else(|| {
                    rmcp::model::ErrorData::invalid_params("action=set needs cookies=[...]", None)
                })?;
                if cookies.is_empty() {
                    return Err(rmcp::model::ErrorData::invalid_params(
                        "cookies array is empty",
                        None,
                    ));
                }
                for (i, c) in cookies.iter().enumerate() {
                    let has_name = c
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| !s.is_empty())
                        .unwrap_or(false);
                    let has_val = c.get("value").is_some();
                    let has_scope = c
                        .get("url")
                        .and_then(|v| v.as_str())
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
                        || c.get("domain")
                            .and_then(|v| v.as_str())
                            .map(|s| !s.is_empty())
                            .unwrap_or(false);
                    if !has_name || !has_val || !has_scope {
                        return Err(rmcp::model::ErrorData::invalid_params(
                            format!(
                                "cookies[{i}] needs name + value + url|domain (got name={has_name} value={has_val} scope={has_scope})"
                            ),
                            None,
                        ));
                    }
                }
                page.browser_cookies("set", serde_json::json!(cookies))
                    .await
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?
            }
            "clear" => page
                .browser_cookies("clear", serde_json::json!(null))
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?,
            other => {
                return Err(rmcp::model::ErrorData::invalid_params(
                    format!("action must be get|set|clear, got {other}"),
                    None,
                ))
            }
        };
        let _ = self.recorder.record(
            &session_id,
            "page_cookies",
            Some(&page_id),
            serde_json::json!({ "action": action }),
        );
        Ok(text_result(out.to_string()))
    }

    #[tool(
        description = "PAGE PERMISSIONS (Browser.grantPermissions / resetPermissions): grant geolocation/camera/microphone/notifications/clipboard-* for an origin BEFORE the page prompts (e.g. allow geo before a maps site loads), or reset ALL grants. action=grant needs origin (\"https://site.tld\") + permissions=[...]; action=reset clears everything."
    )]
    async fn page_permissions(
        &self,
        Parameters(PermissionsParams {
            session_id,
            page_id,
            action,
            origin,
            permissions,
        }): Parameters<PermissionsParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let out = match action.as_str() {
            "grant" => {
                let origin = origin.filter(|o| !o.trim().is_empty()).ok_or_else(|| {
                    rmcp::model::ErrorData::invalid_params(
                        "action=grant needs origin, e.g. \"https://example.com\"",
                        None,
                    )
                })?;
                let perms = permissions.filter(|p| !p.is_empty()).ok_or_else(|| {
                    rmcp::model::ErrorData::invalid_params(
                        "action=grant needs permissions=[\"geolocation\", ...]",
                        None,
                    )
                })?;
                page.grant_permissions(&origin, &perms)
                    .await
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                serde_json::json!({ "action": "grant", "origin": origin, "permissions": perms, "ok": true })
            }
            "reset" => {
                page.reset_permissions()
                    .await
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                serde_json::json!({ "action": "reset", "ok": true })
            }
            other => {
                return Err(rmcp::model::ErrorData::invalid_params(
                    format!("action must be grant|reset, got {other}"),
                    None,
                ))
            }
        };
        let _ = self.recorder.record(
            &session_id,
            "page_permissions",
            Some(&page_id),
            serde_json::json!({ "action": action }),
        );
        Ok(text_result(out.to_string()))
    }

    #[tool(
        description = "DOWNLOAD a file through the REAL browser (Browser.setDownloadOptions saveToDisk + Juggler download interceptor — redirects, cookies and auth headers all apply). Trigger via selector (click a download/export button) or url (navigate to a file URL); dir is prepared (default /root/tmp/ghostfox-dl); the tool waits until a NEW file appears and its size stabilizes, then returns {files:[{name,path,size}], dir, elapsed_ms}. Errors if nothing lands within wait_ms (default 20s)."
    )]
    async fn page_download(
        &self,
        Parameters(DownloadParams {
            session_id,
            page_id,
            selector,
            url,
            dir,
            wait_ms,
        }): Parameters<DownloadParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        if selector.is_none() && url.is_none() {
            return Err(rmcp::model::ErrorData::invalid_params(
                "need selector (click target) or url (file to open)",
                None,
            ));
        }
        let dir = dir.unwrap_or_else(|| "/root/tmp/ghostfox-dl".to_string());
        std::fs::create_dir_all(&dir).map_err(|e| {
            rmcp::model::ErrorData::internal_error(format!("mkdir {dir}: {e}"), None)
        })?;
        let before: std::collections::HashSet<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        page.set_download_options("saveToDisk", &dir)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        if let Some(sel) = &selector {
            page.click(sel)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        } else if let Some(u) = &url {
            page.navigate(u)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        }
        let wait = wait_ms.unwrap_or(20_000).clamp(1_000, 120_000);
        let started = std::time::Instant::now();
        loop {
            if (started.elapsed().as_millis() as u64) >= wait {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let mut found: Vec<serde_json::Value> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.filter_map(|e| e.ok()) {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !before.contains(&name) {
                        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                        found.push(serde_json::json!({
                            "name": name,
                            "path": format!("{dir}/{name}"),
                            "size": size
                        }));
                    }
                }
            }
            if found.is_empty() {
                continue;
            }
            // Wait for sizes to stabilize (file still being written).
            let mut stable = 0u32;
            let mut last: Vec<u64> = Vec::new();
            while stable < 3 && (started.elapsed().as_millis() as u64) < wait {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let mut sizes: Vec<u64> = Vec::new();
                let mut all_ok = true;
                for f in &found {
                    match std::fs::metadata(f["path"].as_str().unwrap_or("")) {
                        Ok(m) => sizes.push(m.len()),
                        Err(_) => {
                            all_ok = false;
                            break;
                        }
                    }
                }
                if !all_ok {
                    continue;
                }
                if sizes == last {
                    stable += 1;
                } else {
                    stable = 0;
                    last = sizes.clone();
                }
                for (f, s) in found.iter_mut().zip(sizes.iter()) {
                    f["size"] = (*s).into();
                }
            }
            let _ = self.recorder.record(
                &session_id,
                "page_download",
                Some(&page_id),
                serde_json::json!({ "dir": dir, "files": found.len() }),
            );
            return Ok(text_result(
                serde_json::json!({
                    "files": found,
                    "dir": dir,
                    "elapsed_ms": started.elapsed().as_millis() as u64
                })
                .to_string(),
            ));
        }
        Err(rmcp::model::ErrorData::internal_error(
            format!(
                "no new file in {dir} within {wait}ms — the target may not trigger a download (or a save prompt opened instead)"
            ),
            None,
        ))
    }

    #[tool(
        description = "NATIVE JS DIALOG HANDLER (Page.handleDialog): alert()/confirm()/prompt() BLOCK the JS engine — every eval hangs until answered. This tool auto-responds: action=arm sets the policy (accept=true accepts dialogs and answers prompts with prompt_text, accept=false dismisses), and it's armed by default on page_open. action=list reads the log of dialogs that opened {count, events:[{type, message, accepted, handled}]}. Use after evaluating scripts that might pop dialogs."
    )]
    async fn page_dialog(
        &self,
        Parameters(DialogParams {
            session_id,
            page_id,
            action,
            accept,
            prompt_text,
            clear,
        }): Parameters<DialogParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let action = action.unwrap_or_else(|| "list".to_string());
        let out = match action.as_str() {
            "arm" => {
                let a = accept.unwrap_or(true);
                let pt = prompt_text.or_else(|| Some(String::new()));
                page.dialog_setup(a, pt)
                    .await
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                serde_json::json!({ "action": "arm", "accept": a, "ok": true })
            }
            "list" => {
                let c = clear.unwrap_or(false);
                page.dialog_log(c)
                    .await
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?
            }
            other => {
                return Err(rmcp::model::ErrorData::invalid_params(
                    format!("action must be arm|list, got {other}"),
                    None,
                ))
            }
        };
        let _ = self.recorder.record(
            &session_id,
            "page_dialog",
            Some(&page_id),
            serde_json::json!({ "action": action }),
        );
        Ok(text_result(out.to_string()))
    }

    #[tool(
        description = "Apply MANY emulation overrides in ONE call — only the keys you send are \
        changed. OP KEYS: color_scheme (dark|light|none), media (print|screen|none), \
        reduced_motion (reduce|none), forced_colors (active|none), contrast \
        (less|more|custom|none), viewport {width,height}, online (false = simulate offline \
        traffic), geolocation {latitude,longitude,accuracy?}, user_agent, timezone, locale, \
        platform, headers {\"name\":\"value\"} sent on EVERY request (auth injection / host \
        overrides), http_auth {username,password} for HTTP Basic-protected targets. Examples: \
        set color_scheme=dark before a dark-mode screenshot; set timezone+locale+user_agent \
        together to align a foreign fingerprint in one shot; set online=false to prove a \
        feature needs the network. Returns {applied:[keys], errors:[...]} — invalid keys are \
        reported individually without rolling back the rest. Omit keys you do NOT need to \
        change."
    )]
    async fn page_emulate(
        &self,
        Parameters(EmulateParams {
            session_id,
            page_id,
            color_scheme,
            online,
            geolocation,
            user_agent,
            timezone,
            locale,
            platform,
            headers,
            http_auth,
            viewport,
            media,
            reduced_motion,
            forced_colors,
            contrast,
        }): Parameters<EmulateParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let mut ops = serde_json::Map::new();
        let mut insert = |key: &str, val: serde_json::Value| {
            ops.insert(key.to_string(), val);
        };
        if let Some(v) = color_scheme {
            insert("color_scheme", serde_json::json!(v));
        }
        if let Some(v) = media {
            insert("media", serde_json::json!(v));
        }
        if let Some(v) = reduced_motion {
            insert("reduced_motion", serde_json::json!(v));
        }
        if let Some(v) = forced_colors {
            insert("forced_colors", serde_json::json!(v));
        }
        if let Some(v) = contrast {
            insert("contrast", serde_json::json!(v));
        }
        if let Some(v) = viewport {
            insert(
                "viewport",
                serde_json::json!({ "width": v.width, "height": v.height }),
            );
        }
        if let Some(v) = online {
            insert("online", serde_json::json!(v));
        }
        if let Some(v) = geolocation {
            let mut geo = serde_json::json!({ "latitude": v.latitude, "longitude": v.longitude });
            if let Some(a) = v.accuracy {
                geo["accuracy"] = serde_json::json!(a);
            }
            insert("geolocation", geo);
        }
        if let Some(v) = user_agent {
            insert("user_agent", serde_json::json!(v));
        }
        if let Some(v) = timezone {
            insert("timezone", serde_json::json!(v));
        }
        if let Some(v) = locale {
            insert("locale", serde_json::json!(v));
        }
        if let Some(v) = platform {
            insert("platform", serde_json::json!(v));
        }
        if let Some(v) = headers {
            insert("headers", v);
        }
        if let Some(v) = http_auth {
            insert(
                "http_auth",
                serde_json::json!({ "username": v.username, "password": v.password }),
            );
        }
        if ops.is_empty() {
            return Err(rmcp::model::ErrorData::invalid_params(
                "no emulation keys provided — send at least one of color_scheme/media/viewport/online/user_agent/timezone/...",
                None,
            ));
        }
        let res = page
            .emulate(&serde_json::Value::Object(ops))
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        Ok(text_result(
            serde_json::to_string_pretty(&res).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "IN-PAGE GREP (zero LLM cost, instant): search the rendered DOM for text \
        or a regex and return matches with surrounding context. Use this to VERIFY whether a \
        string exists ('is the API key echoed?'), to quote evidence with context, or to \
        pre-filter before page_extract — much cheaper and deterministic vs page_eval or LLM \
        extraction. Options: regex=true for patterns, case_sensitive=true, css_scope to \
        restrict to a subtree, context_chars (default 150) of surrounding text, max_results \
        (default 25). Returns {found, truncated, items:[{match, context, tag, id?, offset}]}."
    )]
    async fn page_search(
        &self,
        Parameters(SearchParams {
            session_id,
            page_id,
            pattern,
            regex,
            case_sensitive,
            css_scope,
            context_chars,
            max_results,
        }): Parameters<SearchParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let expr = SEARCH_JS
            .replace("@@PAT@@", &js_escape(&pattern))
            .replace(
                "@@ISRE@@",
                if regex.unwrap_or(false) {
                    "true"
                } else {
                    "false"
                },
            )
            .replace(
                "@@CS@@",
                if case_sensitive.unwrap_or(false) {
                    "true"
                } else {
                    "false"
                },
            )
            .replace(
                "@@SCOPE@@",
                &js_escape(&css_scope.clone().unwrap_or_default()),
            )
            .replace("@@CTX@@", &context_chars.unwrap_or(150).to_string())
            .replace("@@MAX@@", &max_results.unwrap_or(25).to_string());
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: expr,
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "page_search",
            Some(&page_id),
            serde_json::json!({ "pattern": pattern }),
        );
        Ok(r)
    }

    #[tool(
        description = "DOM STRUCTURE ENUMERATION (zero LLM cost, instant): run \
        querySelectorAll(selector) and list what matched — tag, id, trimmed text, plus any \
        attributes you ask for (href, src, class, ...). Use BEFORE crafting links list, \
        counting elements ('how many rows?'), or pulling every endpoint from [href]/[src] — \
        deterministic and token-cheap vs a full snapshot. Returns {total, returned, \
        items:[{index, tag, id?, text?, attrs?}]}. Errors name the bad selector."
    )]
    async fn page_query(
        &self,
        Parameters(QueryParams {
            session_id,
            page_id,
            selector,
            attributes,
            max_results,
            include_text,
        }): Parameters<QueryParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let attrs_json = match &attributes {
            Some(a) => serde_json::to_string(a).unwrap_or_else(|_| "null".into()),
            None => "null".to_string(),
        };
        let expr = QUERY_JS
            .replace("@@SEL@@", &js_escape(&selector))
            .replace("@@MAX@@", &max_results.unwrap_or(50).to_string())
            .replace(
                "@@TEXT@@",
                if include_text.unwrap_or(true) {
                    "true"
                } else {
                    "false"
                },
            )
            .replace("@@ATTRS@@", &attrs_json);
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: expr,
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "page_query",
            Some(&page_id),
            serde_json::json!({ "selector": selector }),
        );
        Ok(r)
    }

    #[tool(
        description = "DROPDOWN INSPECT & PICK: action=list enumerates every option of a native \
        <select> (text/value/selected/disabled) or an ARIA menu (role=option/menuitem) so you \
        never guess labels; action=select picks an option by EXACT text (fallback: exact \
        value), fires input+change events so React/Vue listeners react, and refuses disabled \
        options. On a miss it returns the available labels — fix your text and retry. Typical \
        flow: list → pick → verify via page_eval. Returns {kind: select|aria, ...}."
    )]
    async fn page_dropdown(
        &self,
        Parameters(DropdownParams {
            session_id,
            page_id,
            selector,
            action,
            text,
            value,
        }): Parameters<DropdownParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let act = action.unwrap_or_else(|| "list".to_string());
        if act != "list" && act != "select" {
            return Err(rmcp::model::ErrorData::invalid_params(
                "action must be list or select",
                None,
            ));
        }
        let expr = DROPDOWN_JS
            .replace("@@SEL@@", &js_escape(&selector))
            .replace("@@ACT@@", &js_escape(&act))
            .replace(
                "@@TXT@@",
                &serde_json::to_string(&text).unwrap_or_else(|_| "null".into()),
            )
            .replace(
                "@@VAL@@",
                &serde_json::to_string(&value).unwrap_or_else(|_| "null".into()),
            );
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: expr,
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "page_dropdown",
            Some(&page_id),
            serde_json::json!({ "selector": selector, "action": act }),
        );
        Ok(r)
    }

    #[tool(
        description = "SCROLL UNTIL TEXT IS VISIBLE: finds the element containing the given text \
        and scrolls it into the center of the viewport, then reports its rect + inViewport \
        verdict. Use for verification ('is Terms of Service in the footer?') — kills the \
        blind scroll-and-screenshot loop. direction=down (default) targets the first \
        occurrence from the top, direction=up the last one. Returns {found, occurrences, tag, \
        inViewport, rect, scrollY}; {found:false} when the text is not in the DOM at all."
    )]
    async fn page_scroll_to_text(
        &self,
        Parameters(ScrollTextParams {
            session_id,
            page_id,
            text,
            direction,
        }): Parameters<ScrollTextParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let dir = direction.unwrap_or_else(|| "down".to_string());
        if dir != "down" && dir != "up" {
            return Err(rmcp::model::ErrorData::invalid_params(
                "direction must be down or up",
                None,
            ));
        }
        let expr = SCROLL_TEXT_JS
            .replace("@@TXT@@", &js_escape(&text))
            .replace("@@DIR@@", &js_escape(&dir));
        let r = self
            .page_eval(Parameters(PageEvalParams {
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                expression: expr,
            }))
            .await?;
        let _ = self.recorder.record(
            &session_id,
            "page_scroll_to_text",
            Some(&page_id),
            serde_json::json!({ "text": text, "direction": dir }),
        );
        Ok(r)
    }

    #[tool(
        description = "GEETEST ICON-CLICK SOLVER: solve the GeeTest v3 word-click captcha (characters on a photo, click in the strip's order) from the live page. Fetches the challenge image off the element's background, runs the trained ONNX pair (YOLOv8s char detection + siamese order matching, 100% local CPU, ~0.5s), and returns click targets in page coordinates plus the raw boxes. Call AFTER the challenge popup is open. Then click each 'click' point via page_drag and press the geetest confirm button. Returns JSON {clicks: [{x, y}, ...] in page px, boxes_raw: [[x, y], ...] in image px, rect: {...}, img: {w, h}}."
    )]
    async fn page_geetest_click(
        &self,
        Parameters(GeetestClickParams {
            session_id,
            page_id,
            r#ref,
        }): Parameters<GeetestClickParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        // 1. Background image URL + element rect (CSS px).
        let js = format!(
            r#"(function() {{
  var el = (window.__gfxRefs || new Map()).get({r});
  if (!el || !el.isConnected) return 'STALE-REF';
  var bg = getComputedStyle(el).backgroundImage;
  var m = bg.match(/url\(["']?([^"')]+)["']?\)/);
  var r = el.getBoundingClientRect();
  return JSON.stringify({{url: m ? m[1] : '', x: r.x, y: r.y, w: r.width, h: r.height}});
}})()"#,
            r = serde_json::to_string(&r#ref).unwrap_or_default()
        );
        let out = page
            .evaluate(&js)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let s = out.as_str().unwrap_or_default();
        if s == "STALE-REF" {
            return Err(rmcp::model::ErrorData::invalid_params(
                format!("ref {} is stale — rerun page_a11y", r#ref),
                None,
            ));
        }
        let info: serde_json::Value = serde_json::from_str(s)
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let url = info.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() {
            return Err(rmcp::model::ErrorData::internal_error(
                "element has no background image — is the challenge open?",
                None,
            ));
        }
        // 2. Fetch the challenge image.
        let bytes = reqwest::get(url)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?
            .bytes()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?
            .to_vec();
        let img = image::load_from_memory(&bytes)
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let (iw, ih) = (img.width(), img.height());
        // 3. Trained eyes: YOLO detection + siamese order.
        let boxes = crate::geetest::solve_image(&bytes)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        // 4. Map box centers to page coords. The raw image = field (top ih-40 px)
        //    + instruction strip (bottom 40 px); the element shows the field region.
        let rx = info.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let ry = info.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let rw = info.get("w").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let rh = info.get("h").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let sx = if iw > 0 { rw / iw as f64 } else { 1.0 };
        let sy = if ih > 40 { rh / (ih - 40) as f64 } else { 1.0 };
        let clicks: Vec<serde_json::Value> = boxes
            .iter()
            .map(|b| {
                serde_json::json!({
                    "x": (rx + (b[0] as f64 + 31.0) * sx).round(),
                    "y": (ry + (b[1] as f64 + 31.0) * sy).round(),
                })
            })
            .collect();
        let _ = self.recorder.record(
            &session_id,
            "page_geetest_click",
            Some(&page_id),
            serde_json::json!({ "clicks": boxes.len(), "img": [iw, ih] }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&serde_json::json!({
                "clicks": clicks,
                "boxes_raw": boxes,
                "rect": { "x": rx, "y": ry, "w": rw, "h": rh },
                "img": { "w": iw, "h": ih },
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "GEETEST SLIDE SOLVER: solve the GeeTest v3-style slide puzzle from the live page, then perform the human drag itself. Extracts the three canvases (bg, puzzle slice, full reference bg) via toDataURL, finds the hole with |bg-fullbg| diff + largest-blob + morphological closing (the JPEG-noise trap), measures the piece's solid-alpha left edge, and drags the slider by hole_x0 - piece_x0 with the engine's humanized mouse. Returns JSON {drag_x, hole: [x0, x1], piece_x0}. Call AFTER the challenge popup is open. No vision model, pure pixel math."
    )]
    async fn page_geetest_slide(
        &self,
        Parameters(GeetestSlideParams {
            session_id,
            page_id,
            slider_ref,
        }): Parameters<GeetestSlideParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        // 1. Grab the three canvases as data URLs + register the slider ref.
        let slider_js = match &slider_ref {
            Some(r) => serde_json::to_string(r).unwrap_or_default(),
            None => "null".into(),
        };
        let js = format!(
            r#"(function() {{
  window.__gfxRefs = window.__gfxRefs || new Map();
  var bg = document.querySelector('canvas.geetest_canvas_bg');
  var sl = document.querySelector('canvas.geetest_canvas_slice');
  var fb = document.querySelector('canvas.geetest_canvas_fullbg');
  if (!bg || !sl) return JSON.stringify({{err: 'canvases missing — is the slide challenge open?'}});
  var btn = {slider_js} !== null && (window.__gfxRefs.get({slider_js}) || null);
  if (!btn) {{
    btn = document.querySelector('.geetest_slider_button');
    if (btn) window.__gfxRefs.set('gs_btn', btn);
  }}
  if (!btn) return JSON.stringify({{err: 'slider handle not found'}});
  try {{
    return JSON.stringify({{bg: bg.toDataURL(), sl: sl.toDataURL(), fb: fb.toDataURL(), ref: 'gs_btn'}});
  }} catch(e) {{ return JSON.stringify({{err: 'tainted canvas: ' + e.message}}); }}
}})()"#
        );
        let out = page
            .evaluate(&js)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let s = out.as_str().unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(s)
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        if let Some(err) = v.get("err").and_then(|e| e.as_str()).map(|e| e.to_string()) {
            return Err(rmcp::model::ErrorData::internal_error(err, None));
        }
        let b64 = |k: &str| -> Vec<u8> {
            v.get(k)
                .and_then(|x| x.as_str())
                .and_then(|d| d.split(',').nth(1))
                .map(|p| {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD
                        .decode(p)
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        let bg = image::load_from_memory(&b64("bg"))
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?
            .to_luma8();
        let sl = image::load_from_memory(&b64("sl"))
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?
            .to_rgba8();
        let fb = image::load_from_memory(&b64("fb"))
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?
            .to_luma8();
        let drag_x = crate::geetest::slide_gap(&bg, &sl, &fb)
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        // 2. Human drag.
        page.drag_ref("gs_btn", "", drag_x as f64, 0.0)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_geetest_slide",
            Some(&page_id),
            serde_json::json!({ "drag_x": drag_x }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&serde_json::json!({
                "drag_x": drag_x,
                "dragged": true,
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "CAPTCHA OCR: classify the text of a normal image captcha (the distorted-text family) with the ddddocr model (CRNN+LSTM trained specifically on captcha text, 8210-char charset). 100% local — runs on onnxruntime inside the runtime. Takes the ref of the captcha <img> element (or the whole viewport) and returns the recognized text. Feed it into the answer field and submit. Far stronger than generic OCR on captcha fonts."
    )]
    async fn page_captcha_ocr(
        &self,
        Parameters(CaptchaOcrParams {
            session_id,
            page_id,
            r#ref,
        }): Parameters<CaptchaOcrParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let png = self.element_png(&session_id, &page_id, r#ref).await?;
        let text = crate::ddddocr::classify_png(&png)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_captcha_ocr",
            Some(&page_id),
            serde_json::json!({ "chars": text.chars().count() }),
        );
        Ok(text_result(text))
    }

    /// One real mouse click at viewport-absolute (x, y) via a dummy
    /// fixed-position anchor + drag_ref, same humanized path the original
    /// solver used for its GLM-derived clicks.
    async fn hc_click_xy(
        page: &Arc<dyn ghostcloak_core::engine::PageHandle>,
        x: f64,
        y: f64,
        name: &str,
    ) -> Result<(), rmcp::model::ErrorData> {
        let mk = format!(
            r#"(function() {{
  window.__gfxRefs = window.__gfxRefs || new Map();
  var d = document.createElement('div');
  d.style.cssText = 'position:fixed;left:{x}px;top:{y}px;width:2px;height:2px;z-index:99999;pointer-events:none;';
  document.body.appendChild(d);
  window.__gfxRefs.set('{name}', d);
  return 'OK';
}})()"#,
            x = x,
            y = y,
            name = name
        );
        let _ = page.evaluate(&mk).await;
        page.drag_ref(name, "", 0.0, 0.0)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))
    }

    #[tool(
        description = "HCAPTCHA SOLVER (agent-driven, 100% local — no API keys, no third-party vision): two-phase flow. PREPARE (no tiles param): opens the challenge (clicking the anchor if needed), then returns {phase:'need_answer', round, screenshot, challenge:{x,y,w,h,vw,png_width}} — look at the screenshot, pick the viewport pixel coordinates of the tiles to click, then call AGAIN WITH tiles=[{x,y}...] (plus optional verify={x,y} submit button, drag={from:{x,y},to:{x,y}} for drag puzzles). After applying, the tool re-checks the token: returns success=true, or the next need_answer screenshot (hCaptcha chains 2-4 rounds) until max_rounds (default 6). The agent does the seeing; the tool does the clicking."
    )]
    async fn page_hcaptcha(
        &self,
        Parameters(HcaptchaParams {
            session_id,
            page_id,
            checkbox_ref,
            tiles,
            drag,
            verify,
            max_rounds,
        }): Parameters<HcaptchaParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let max_rounds = max_rounds.unwrap_or(6) as usize;
        let token_js = r#"(function() {
  var r = document.querySelector('[name*=h-captcha-response], textarea[name*=h-captcha-response]');
  return r ? (r.value || '').length : 0;
})()"#;
        let chall_js = r#"(function() {
  var f = null;
  document.querySelectorAll('iframe').forEach(function(i) {
    var b = i.getBoundingClientRect();
    if (b.width > 400 && b.y > -100 && !f) f = i;
  });
  if (!f) return 'NO';
  var b = f.getBoundingClientRect();
  return JSON.stringify({x: b.x, y: b.y, w: b.width, h: b.height, vw: window.innerWidth});
})()"#;

        // 1. Locate the challenge iframe; if absent, click the anchor first.
        let mut out = page
            .evaluate(chall_js)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        if out.as_str() == Some("NO") {
            let anchor_js = match &checkbox_ref {
                Some(r) => format!(
                    r#"(function() {{
  var el = (window.__gfxRefs || new Map()).get({r});
  if (!el || !el.isConnected) return JSON.stringify({{err: 'stale ref'}});
  var b = el.getBoundingClientRect();
  return JSON.stringify({{x: b.x + b.width/2, y: b.y + b.height/2}});
}})()"#,
                    r = serde_json::to_string(r).unwrap_or_default()
                ),
                None => r#"(function() {
  var f = null;
  document.querySelectorAll('iframe').forEach(function(i) {
    var src = (i.src || '');
    if ((src.indexOf('hcaptcha') >= 0 || src.indexOf('newassets') >= 0) && !f) {
      var b = i.getBoundingClientRect();
      if (b.width > 200 && b.width < 400 && b.y > -100) f = i;
    }
  });
  if (!f) return JSON.stringify({err: 'no hCaptcha anchor iframe found'});
  var b = f.getBoundingClientRect();
  return JSON.stringify({x: b.x + b.width/2, y: b.y + b.height/2});
})()"#
                    .to_string(),
            };
            let av_out = page
                .evaluate(&anchor_js)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            let av: serde_json::Value = serde_json::from_str(av_out.as_str().unwrap_or("{}"))
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            if let Some(err) = av.get("err").and_then(|e| e.as_str()) {
                return Err(rmcp::model::ErrorData::internal_error(
                    err.to_string(),
                    None,
                ));
            }
            let (ax, ay) = (
                av.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
                av.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
            );
            Self::hc_click_xy(&page, ax, ay, "hc_anchor").await?;
            for _ in 0..14 {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                out = page
                    .evaluate(chall_js)
                    .await
                    .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
                if out.as_str() != Some("NO") {
                    break;
                }
            }
        }
        if out.as_str() == Some("NO") {
            // Risk-based auto-pass: the token can be issued without any
            // challenge popup — check it before declaring failure.
            let tok = page
                .evaluate(token_js)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            let response_len = tok.as_i64().unwrap_or(0);
            if response_len > 0 {
                let _ = self.recorder.record(
                    &session_id,
                    "page_hcaptcha",
                    Some(&page_id),
                    serde_json::json!({ "phase": "auto_pass", "response_len": response_len }),
                );
                return Ok(text_result(
                    serde_json::to_string_pretty(&serde_json::json!({
                        "success": true,
                        "rounds": 1,
                        "response_len": response_len,
                        "auto_pass": true,
                    }))
                    .unwrap_or_default(),
                ));
            }
            return Err(rmcp::model::ErrorData::internal_error(
                "hCaptcha challenge iframe never appeared",
                None,
            ));
        }
        let chall: serde_json::Value =
            serde_json::from_str(out.as_str().unwrap_or("")).map_err(|_| {
                rmcp::model::ErrorData::internal_error(
                    "hCaptcha challenge iframe never appeared",
                    None,
                )
            })?;
        let (cx, cy, cw, ch, vw) = (
            chall.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0),
            chall.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0),
            chall.get("w").and_then(|v| v.as_f64()).unwrap_or(0.0),
            chall.get("h").and_then(|v| v.as_f64()).unwrap_or(0.0),
            chall.get("vw").and_then(|v| v.as_f64()).unwrap_or(1.0),
        );
        let round_now = async {
            let out = page
                .evaluate("String(window.__hcRound||0)")
                .await
                .unwrap_or_default();
            out.as_str().unwrap_or("0").parse::<u32>().unwrap_or(0)
        }
        .await;

        // 2a. PREPARE: nothing to click yet — hand the screenshot to the agent.
        if tiles.is_none() && drag.is_none() && verify.is_none() {
            // Let lazy-loaded tiles settle so the screenshot shows content.
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            let png = page
                .screenshot(false)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            let png_width = image::load_from_memory(&png)
                .map(|i| i.width())
                .unwrap_or(0);
            let path = self
                .recorder
                .record_screenshot(&session_id, &page_id, &png)
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            let out = page
                .evaluate("window.__hcRound = (window.__hcRound||0)+1; String(window.__hcRound)")
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            let round = out.as_str().unwrap_or("1").parse::<u32>().unwrap_or(1);
            if round as usize > max_rounds {
                return Err(rmcp::model::ErrorData::internal_error(
                    format!("max_rounds ({max_rounds}) exhausted — no token yet"),
                    None,
                ));
            }
            let _ = self.recorder.record(
                &session_id,
                "page_hcaptcha",
                Some(&page_id),
                serde_json::json!({ "phase": "prepare", "round": round }),
            );
            return Ok(text_result(
                serde_json::to_string_pretty(&serde_json::json!({
                    "phase": "need_answer",
                    "round": round,
                    "screenshot": path.to_string_lossy(),
                    "challenge": { "x": cx, "y": cy, "w": cw, "h": ch, "vw": vw, "png_width": png_width },
                    "hint": "Look at the screenshot: pick viewport pixel coordinates of the tiles to click, then call again with tiles=[{x,y}...] (add verify={x,y} for the submit button; drag={from:{x,y},to:{x,y}} for drag puzzles). Each successful call returns either success=true or the next need_answer screenshot.",
                }))
                .unwrap_or_default(),
            ));
        }

        // 2b. APPLY: click the agent's coordinates with the humanized mouse.
        if let Some(list) = &tiles {
            for (i, t) in list.iter().enumerate() {
                Self::hc_click_xy(&page, t.x, t.y, &format!("hc_t{i}")).await?;
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
        }
        if let Some(d) = &drag {
            Self::hc_click_xy(&page, d.from.x, d.from.y, "hc_drag").await?;
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let mk = format!(
                r#"(function() {{
  window.__gfxRefs = window.__gfxRefs || new Map();
  var d = document.createElement('div');
  d.style.cssText = 'position:fixed;left:{x}px;top:{y}px;width:2px;height:2px;z-index:99999;pointer-events:none;';
  document.body.appendChild(d);
  window.__gfxRefs.set('hc_dragmove', d);
  return 'OK';
}})()"#,
                x = d.from.x,
                y = d.from.y
            );
            let _ = page.evaluate(&mk).await;
            page.drag_ref("hc_dragmove", "", d.to.x - d.from.x, d.to.y - d.from.y)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        }
        if let Some(v) = &verify {
            Self::hc_click_xy(&page, v.x, v.y, "hc_verify").await?;
        }
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
        let out = page
            .evaluate(token_js)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let response_len = out.as_i64().unwrap_or(0);
        let round = round_now.max(1);

        if response_len > 0 {
            let _ = self.recorder.record(
                &session_id,
                "page_hcaptcha",
                Some(&page_id),
                serde_json::json!({ "phase": "apply", "round": round, "response_len": response_len }),
            );
            return Ok(text_result(
                serde_json::to_string_pretty(&serde_json::json!({
                    "success": true,
                    "rounds": round,
                    "response_len": response_len,
                }))
                .unwrap_or_default(),
            ));
        }
        if round as usize >= max_rounds {
            return Ok(text_result(
                serde_json::to_string_pretty(&serde_json::json!({
                    "success": false,
                    "phase": "max_rounds",
                    "rounds": round,
                }))
                .unwrap_or_default(),
            ));
        }

        // Not yet — hand over the next round's screenshot.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let png = page
            .screenshot(false)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let png_width = image::load_from_memory(&png)
            .map(|i| i.width())
            .unwrap_or(0);
        let path = self
            .recorder
            .record_screenshot(&session_id, &page_id, &png)
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = page
            .evaluate("window.__hcRound = (window.__hcRound||0)+1")
            .await;
        Ok(text_result(
            serde_json::to_string_pretty(&serde_json::json!({
                "phase": "need_answer",
                "round": round + 1,
                "screenshot": path.to_string_lossy(),
                "challenge": { "x": cx, "y": cy, "w": cw, "h": ch, "vw": vw, "png_width": png_width },
                "hint": "Answer not accepted yet — look at the fresh screenshot and call again with the next tiles=[{x,y}...].",
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "ROTATE CAPTCHA SOLVER: brute-force sweep + human replay. Rotate challenges ask to turn an image upright; standard demos rotate 15deg per button click and answer with a check button. The tool first sweeps every angle programmatically (instant JS clicks, ~30s), reads the visible feedback after each check (success keywords: pass/通过/正确/成功/verif/succeeded), then REPLAYS the winning rotation with real human mouse drags and the final check. Returns JSON {clicks, angle, status}. For fixed-image demos the sweep converges every time."
    )]
    async fn page_captcha_rotate(
        &self,
        Parameters(RotateParams {
            session_id,
            page_id,
            rot_right_ref,
            rot_left_ref,
            check_ref,
            reset_ref,
        }): Parameters<RotateParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let js = format!(
            r#"(async function() {{
  window.__gfxRefs = window.__gfxRefs || new Map();
  var rr = window.__gfxRefs.get({rr});
  var rl = window.__gfxRefs.get({rl});
  var ck = window.__gfxRefs.get({ck});
  var rs = window.__gfxRefs.get({rs});
  if (!rr || !rl || !ck || !rs) return JSON.stringify({{err: 'stale refs — rerun page_a11y and re-set refs'}});
  var read = function() {{
    var out = [];
    document.querySelectorAll('[class*=alert],[class*=status],[class*=result],[class*=success],[class*=error],[class*=message],[class*=tip]').forEach(function(el){{
      var t = (el.innerText||'').trim();
      var st = getComputedStyle(el);
      if (t && t.length < 70 && st.display !== 'none' && el.getBoundingClientRect().width > 0) out.push(t.slice(0,55));
    }});
    return Array.from(new Set(out)).join(' | ');
  }};
  var ok = function(t) {{ return /pass|通过|正确|成功|success|verif|solved/i.test(t) && !/错误|wrong|incorrect/i.test(t); }};
  var history = [];
  var winner = -1;
  for (var k = 0; k < 24; k++) {{
    rs.click();
    await new Promise(function(r){{ setTimeout(r, 120); }});
    for (var i = 0; i < k; i++) {{ rr.click(); }}
    await new Promise(function(r){{ setTimeout(r, 120); }});
    ck.click();
    await new Promise(function(r){{ setTimeout(r, 500); }});
    var t = read();
    history.push({{k: k, text: t.slice(0,60)}});
    if (t && ok(t) && winner < 0) winner = k;
  }}
  return JSON.stringify({{winner: winner, history: history.slice(-24)}});
}})()"#,
            rr = serde_json::to_string(&rot_right_ref).unwrap_or_default(),
            rl = serde_json::to_string(&rot_left_ref).unwrap_or_default(),
            ck = serde_json::to_string(&check_ref).unwrap_or_default(),
            rs = serde_json::to_string(&reset_ref).unwrap_or_default(),
        );
        let out = page
            .evaluate(&js)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let s = out.as_str().unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(s)
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        if let Some(err) = v.get("err").and_then(|e| e.as_str()).map(|e| e.to_string()) {
            return Err(rmcp::model::ErrorData::internal_error(err, None));
        }
        let winner = v.get("winner").and_then(|w| w.as_i64()).unwrap_or(-1);
        if winner < 0 {
            return Ok(text_result(
                serde_json::to_string_pretty(&serde_json::json!({
                    "solved": false,
                    "history": v.get("history"),
                }))
                .unwrap_or_default(),
            ));
        }
        // Human replay of the winning rotation. Re-resolve refs first:
        // the JS sweep mutates the page (React re-renders), stale DOM nodes
        // fail the engine's a11y ref validation.
        let rejs = format!(
            r#"(function() {{
  window.__gfxRefs = window.__gfxRefs || new Map();
  var btns = document.querySelectorAll('button');
  btns.forEach(function(b) {{
    var t = (b.innerText||'').trim();
    if (t.indexOf('向右') >= 0) window.__gfxRefs.set({rr}, b);
    if (t.indexOf('向左') >= 0) window.__gfxRefs.set({rl}, b);
    if (t.indexOf('检查') >= 0) window.__gfxRefs.set({ck}, b);
    if (t.indexOf('重置') >= 0) window.__gfxRefs.set({rs}, b);
  }});
  return 'OK';
}})()"#,
            rr = serde_json::to_string(&rot_right_ref).unwrap_or_default(),
            rl = serde_json::to_string(&rot_left_ref).unwrap_or_default(),
            ck = serde_json::to_string(&check_ref).unwrap_or_default(),
            rs = serde_json::to_string(&reset_ref).unwrap_or_default(),
        );
        let _ = page.evaluate(&rejs).await;
        page.drag_ref(&reset_ref, "", 0.0, 0.0)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        for _ in 0..winner {
            page.drag_ref(&rot_right_ref, "", 0.0, 0.0)
                .await
                .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        }
        page.drag_ref(&check_ref, "", 0.0, 0.0)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_captcha_rotate",
            Some(&page_id),
            serde_json::json!({ "clicks": winner, "angle": winner * 15 }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&serde_json::json!({
                "solved": true,
                "clicks": winner,
                "angle": winner * 15,
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "DEBUG CORTEX: read the page's console output (log/warning/error) captured at the PROTOCOL level — the page cannot hide or patch it. Invisible debugger for AI agents: reproduce the bug, read what the page logged. Optionally filter noise by clearing after read. Returns JSON [{kind, text, url, line, ts}]. Auto-starts capture on first call."
    )]
    async fn page_console(
        &self,
        Parameters(ConsoleParams {
            session_id,
            page_id,
            clear,
        }): Parameters<ConsoleParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let entries = page
            .console_read(clear.unwrap_or(false))
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_console",
            Some(&page_id),
            serde_json::json!({ "entries": entries.len() }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&entries).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "DEBUG CORTEX: read the page's UNCAUGHT JS EXCEPTIONS with stack traces, captured at the protocol level. The error console a developer opens in DevTools — as a tool. Returns JSON [{text, url, line, stack: [frames], ts}]. Auto-starts capture on first call."
    )]
    async fn page_errors(
        &self,
        Parameters(ErrorsParams {
            session_id,
            page_id,
            clear,
        }): Parameters<ErrorsParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let entries = page
            .errors_read(clear.unwrap_or(false))
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_errors",
            Some(&page_id),
            serde_json::json!({ "errors": entries.len() }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&entries).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "POPUP/TAB VISION: list EVERY browser target — pages you opened AND popups the site opened (OAuth windows, payment flows). Popups are auto-attached and given a page_id usable with every page_* tool immediately. Returns JSON [{page_id, target_id, url}]."
    )]
    async fn session_pages(
        &self,
        Parameters(PagesParams { session_id }): Parameters<PagesParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let overview = session.pages_overview().await;
        let _ = self.recorder.record(
            &session_id,
            "session_pages",
            None,
            serde_json::json!({ "targets": overview.len() }),
        );
        let items: Vec<serde_json::Value> = overview
            .into_iter()
            .map(|(page_id, target_id, url)| {
                serde_json::json!({ "page_id": page_id, "target_id": target_id, "url": url })
            })
            .collect();
        Ok(text_result(
            serde_json::to_string_pretty(&items).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "Type text into the element a page_a11y ref points at (inputs and rich editors). Returns landed chars as a receipt."
    )]
    async fn page_type_ref(
        &self,
        Parameters(TypeRefParams {
            session_id,
            page_id,
            r#ref,
            text,
        }): Parameters<TypeRefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        page.type_ref(&r#ref, &text)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_type_ref",
            Some(&page_id),
            serde_json::json!({ "ref": r#ref, "chars": text.chars().count() }),
        );
        Ok(text_result("typed"))
    }

    #[tool(
        description = "Read the FULL value of an element by its page_a11y ref — no truncation. Use when the snapshot's 200-char preview isn't enough (body text, long input fields)."
    )]
    async fn page_read_ref(
        &self,
        Parameters(ReadRefParams {
            session_id,
            page_id,
            r#ref,
        }): Parameters<ReadRefParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let value = page
            .read_ref_full(&r#ref)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_read_ref",
            Some(&page_id),
            serde_json::json!({ "ref": r#ref, "chars": value.len() }),
        );
        Ok(text_result(value))
    }

    #[tool(
        description = "Wait until a CSS selector becomes visible on the page (replaces manual sleeps). Returns true if found, false on timeout."
    )]
    async fn page_wait_for(
        &self,
        Parameters(WaitForParams {
            session_id,
            page_id,
            selector,
            timeout_ms,
        }): Parameters<WaitForParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let timeout = timeout_ms.unwrap_or(10_000);
        let found = page
            .wait_for(&selector, timeout)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_wait_for",
            Some(&page_id),
            serde_json::json!({ "selector": selector, "timeout_ms": timeout, "found": found }),
        );
        Ok(text_result(if found { "true" } else { "false" }))
    }

    #[tool(
        description = "Upload a file to an input[type=file] by CSS selector. The file must exist on the machine running the engine."
    )]
    async fn page_upload_file(
        &self,
        Parameters(UploadParams {
            session_id,
            page_id,
            selector,
            file_path,
        }): Parameters<UploadParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        // Verify the file exists, then set it on the input via a DataTransfer.
        let content = std::fs::read(&file_path).map_err(|e| {
            rmcp::model::ErrorData::invalid_params(format!("cannot read {file_path}: {e}"), None)
        })?;
        let b64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&content)
        };
        let sel = serde_json::to_string(&selector).unwrap_or_default();
        let expr = format!(
            r#"(function() {{
  var el = document.querySelector({sel});
  if (!el) return 'NOT-FOUND';
  if (el.tagName !== 'INPUT' || el.type !== 'file') return 'NOT-FILE-INPUT';
  var b64 = {b64};
  var bin = atob(b64);
  var bytes = new Uint8Array(bin.length);
  for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  var name = {name}.split('/').pop() || 'upload';
  var mime = 'application/octet-stream';
  var file = new File([bytes], name, {{ type: mime }});
  var dt = new DataTransfer();
  dt.items.add(file);
  el.files = dt.files;
  el.dispatchEvent(new Event('input', {{ bubbles: true }}));
  el.dispatchEvent(new Event('change', {{ bubbles: true }}));
  return 'UPLOADED:' + el.files.length;
}})()"#,
            sel = sel,
            b64 = serde_json::to_string(&b64).unwrap_or_default(),
            name = serde_json::to_string(&file_path).unwrap_or_default(),
        );
        let result = page
            .evaluate(&expr)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        // Surface real failures as errors instead of a fake success string:
        // 'NOT-FOUND' used to come back as a normal result (isError=false),
        // so callers believed an upload happened when nothing matched.
        match result.as_str().unwrap_or("") {
            "NOT-FOUND" => {
                return Err(rmcp::model::ErrorData::invalid_params(
                    format!("selector not found: {selector} (need an input[type=file])"),
                    None,
                ))
            }
            "NOT-FILE-INPUT" => {
                return Err(rmcp::model::ErrorData::invalid_params(
                    format!("element matched but is not input[type=file]: {selector}"),
                    None,
                ))
            }
            _ => {}
        }
        let _ = self.recorder.record(
            &session_id,
            "page_upload_file",
            Some(&page_id),
            serde_json::json!({ "selector": selector, "file": file_path }),
        );
        Ok(text_result(result.as_str().unwrap_or("unknown")))
    }

    #[tool(
        description = "Detect the currently logged-in username on the page (Reddit, X, GitHub, HN). Returns username or 'unknown'. Use before commenting to avoid duplicates."
    )]
    async fn session_me(
        &self,
        Parameters(SessionMeParams {
            session_id,
            page_id,
        }): Parameters<SessionMeParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page =
            match page_id {
                Some(pid) => session
                    .page(&pid)
                    .await
                    .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?,
                None => {
                    let ids = session.page_ids().await;
                    if let Some(first) = ids.first() {
                        session.page(first).await.map_err(|e| {
                            rmcp::model::ErrorData::invalid_params(e.to_string(), None)
                        })?
                    } else {
                        let pg = session.new_page(Some("about:blank")).await.map_err(|e| {
                            rmcp::model::ErrorData::internal_error(e.to_string(), None)
                        })?;
                        pg
                    }
                }
            };
        // Use a11y_snapshot (pierces shadow DOM) to find username
        let snap =
            page.a11y_snapshot()
                .await
                .unwrap_or_else(|_| ghostcloak_core::engine::A11ySnapshot {
                    elements: vec![],
                    login_state: "unknown".into(),
                    page_url: String::new(),
                    page_title: String::new(),
                    danger_zone: None,
                    suspicious_elements: 0,
                    page_archived: None,
                    own_elements: 0,
                    username: None,
                    below_viewport: 0,
                    max_scroll_pages: 0,
                    notifications: vec![],
                    rate_limit_seconds: None,
                });
        // v0.5.3: page_a11y now detects the username from header profile
        // links (Reddit /user/X, X /@handle, HN logout link) — far more
        // reliable than scraping "Comment from X" which matches other users.
        let mut username = snap
            .username
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        if username == "unknown" {
            for e in &snap.elements {
                let name = &e.name;
                // Reddit: "Comment from [username]"
                if let Some(rest) = name.strip_prefix("Comment from ") {
                    let uname = rest.split_whitespace().next().unwrap_or("unknown");
                    if uname.len() >= 3 {
                        username = uname.to_string();
                        break;
                    }
                }
                // Reddit: "Expand user menu" → check for @username nearby
                if name.contains("user menu") || name.contains("User menu") {
                    if let Some(at) = name.find('@') {
                        let rest = &name[at + 1..];
                        let uname = rest.split_whitespace().next().unwrap_or("unknown");
                        if uname.len() >= 3 {
                            username = uname.to_string();
                            break;
                        }
                    }
                }
                // Reddit: profile link with /user/username
                if name.contains("/user/") || name.contains("u/") {
                    let parts: Vec<&str> = name.split('/').collect();
                    for (i, part) in parts.iter().enumerate() {
                        if (*part == "user" || *part == "u") && i + 1 < parts.len() {
                            let uname = parts[i + 1];
                            if uname.len() >= 3
                                && uname
                                    .chars()
                                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                            {
                                username = uname.to_string();
                                break;
                            }
                        }
                    }
                    if username != "unknown" {
                        break;
                    }
                }
            }
        }
        let username = username;
        Ok(text_result(username))
    }

    #[tool(
        description = "Submit a comment on the current page. Automatically: 1) detects your username via session_me, 2) checks if you already commented (skips if duplicate), 3) finds the comment editor (Reply or Join conversation), 4) opens it, 5) types your text, 6) clicks submit, 7) verifies. Returns result JSON."
    )]
    async fn page_comment(
        &self,
        Parameters(PageCommentParams {
            session_id,
            page_id,
            text,
        }): Parameters<PageCommentParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;

        // STEP 1: Get own username via a11y (fast, uses shadow DOM piercing)
        let snap = page
            .a11y_snapshot()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;

        let mut own_username = String::new();
        for e in &snap.elements {
            if let Some(rest) = e.name.strip_prefix("Comment from ") {
                let uname = rest.split_whitespace().next().unwrap_or("");
                if uname.len() >= 3 {
                    own_username = uname.to_string();
                    break;
                }
            }
        }

        // STEP 2: Check for duplicate comments
        if !own_username.is_empty() {
            let has_own = snap.elements.iter().any(|e| {
                e.name.contains(&format!("Comment from {}", own_username))
                    || (e.name.contains(&own_username) && e.name.contains("ago"))
            });
            if has_own {
                let _ = self.recorder.record(
                    &session_id,
                    "page_comment",
                    Some(&page_id),
                    serde_json::json!({
                        "action": "skipped", "reason": "duplicate", "username": own_username
                    }),
                );
                return Ok(text_result(
                    serde_json::to_string_pretty(&serde_json::json!({
                        "action": "skipped",
                        "reason": "already_commented",
                        "username": own_username,
                    }))
                    .unwrap_or_default(),
                ));
            }
        }

        // STEP 3: Find comment trigger (Reply preferred, Join conversation fallback)
        let trigger = page
            .evaluate(
                r#"(() => {
                var btns = document.querySelectorAll('button');
                for (var i=0;i<btns.length;i++) {
                    if (btns[i].offsetParent && btns[i].textContent.trim() === 'Reply') {
                        return 'reply';
                    }
                }
                var tbs = document.querySelectorAll('[role=textbox]');
                for (var j=0;j<tbs.length;j++) {
                    var label = tbs[j].getAttribute('aria-label')||tbs[j].textContent||'';
                    if (label.includes('Join the conversation')) return 'join';
                }
                return 'none';
            })()"#,
            )
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;

        let trigger = trigger.as_str().unwrap_or("none");
        if trigger == "none" {
            return Err(rmcp::model::ErrorData::internal_error(
                "No comment box or Reply button found on this page".to_string(),
                None,
            ));
        }

        // STEP 4: Click the trigger
        let click_js = if trigger == "reply" {
            r#"(() => {
                var btns = document.querySelectorAll('button');
                for (var i=0;i<btns.length;i++) {
                    if (btns[i].offsetParent && btns[i].textContent.trim() === 'Reply') {
                        btns[i].click(); return 'clicked-reply';
                    }
                }
                return 'not-found';
            })()"#
        } else {
            r#"(() => {
                var els = document.querySelectorAll('[role=textbox]');
                for (var i=0;i<els.length;i++) {
                    if ((els[i].getAttribute('aria-label')||'').includes('Join the conversation')) {
                        els[i].focus(); els[i].click(); return 'clicked-join';
                    }
                }
                return 'not-found';
            })()"#
        };

        page.evaluate(click_js)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;

        // Wait for editor to render
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;

        // STEP 5: Find editor and type
        let typed = page
            .evaluate(&format!(
                r#"(() => {{
                    var els = document.querySelectorAll('[contenteditable=true][role=textbox]');
                    for (var i=0;i<els.length;i++) {{
                        var r = els[i].getBoundingClientRect();
                        if (r.width > 50 && r.height > 20 && els[i].offsetParent) {{
                            var el = els[i];
                            el.focus();
                            var text = {text};
                            try {{
                                var dt = new DataTransfer();
                                dt.setData('text/plain', text);
                                el.dispatchEvent(new ClipboardEvent('paste', {{clipboardData: dt, bubbles: true, cancelable: true}}));
                                if (el.textContent.length > 0) return 'TYPED:' + el.textContent.length;
                            }} catch(e) {{}}
                            el.textContent = text;
                            el.dispatchEvent(new InputEvent('input', {{bubbles: true, data: text, inputType: 'insertText'}}));
                            return 'TYPED:' + el.textContent.length;
                        }}
                    }}
                    return 'NO-EDITOR';
                }})()"#,
                text = serde_json::to_string(&text).unwrap_or_default(),
            ))
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;

        let typed_str = typed.as_str().unwrap_or("");
        if !typed_str.starts_with("TYPED:") {
            return Err(rmcp::model::ErrorData::internal_error(
                format!("Comment editor not found after clicking {}", trigger),
                None,
            ));
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // STEP 6: Find and click submit
        let submitted = page
            .evaluate(
                r#"(() => {
                var btns = document.querySelectorAll('button');
                for (var i=0;i<btns.length;i++) {
                    if (btns[i].offsetParent && btns[i].textContent.trim() === 'Comment') {
                        btns[i].click(); return 'SUBMITTED';
                    }
                }
                return 'NO-SUBMIT';
            })()"#,
            )
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;

        // STEP 7: Verify
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
        let verify_snippet: String = text.chars().take(25).collect();
        let verified = page
            .evaluate(&format!(
                r#"(() => document.body.innerText.includes({snippet}) ? 'LIVE' : 'PENDING')()"#,
                snippet = serde_json::to_string(&verify_snippet).unwrap_or_default(),
            ))
            .await
            .unwrap_or_default();

        let result = serde_json::json!({
            "action": "commented",
            "method": trigger,
            "username": own_username,
            "duplicate_check": "passed",
            "typed": typed_str.trim_start_matches("TYPED:"),
            "submitted": submitted.as_str().unwrap_or(""),
            "verified": verified.as_str().unwrap_or(""),
        });

        let _ = self
            .recorder
            .record(&session_id, "page_comment", Some(&page_id), result.clone());
        Ok(text_result(
            serde_json::to_string_pretty(&result).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "SAFETY GATE: confirm a dangerous action before executing it. Call this BEFORE clicking refs on pages flagged with danger_zone (financial/medical/legal/auth)."
    )]
    async fn confirm_action(
        &self,
        Parameters(ConfirmActionParams {
            session_id,
            page_id,
            action,
            r#ref,
        }): Parameters<ConfirmActionParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let snap = page
            .snapshot()
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "confirm_action",
            Some(&page_id),
            serde_json::json!({
                "action": action, "ref": r#ref, "url": snap.url, "confirmed": true,
            }),
        );
        Ok(text_result(
            serde_json::to_string_pretty(&serde_json::json!({
                "confirmed": true, "action": action, "ref": r#ref,
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "Detect and dismiss common modals: cookie banners, consent dialogs, popups, overlays. Returns what was dismissed."
    )]
    async fn page_dismiss_modal(
        &self,
        Parameters(DismissModalParams {
            session_id,
            page_id,
        }): Parameters<DismissModalParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let result = page.evaluate(r#"(() => {
            const phrases = ['accept all','allow all','accept cookies','accept cookie','i agree','agree','reject all','got it','dismiss','not now','no thanks','allow all','continue'];
            const singles = ['ok','close','accept','agree'];
            const clicked = [];
            const tryClick = (el, how) => {
                const text = (el.textContent || '').trim().toLowerCase();
                if (!text || text.length > 30) return false;
                if (!el.offsetParent && getComputedStyle(el).position !== 'fixed') return false;
                // phrases match by substring; single words only EXACTLY —
                // substring 'ok' would otherwise match "cookies".
                const hit = phrases.some(p => text.includes(p)) || singles.some(w => text === w);
                if (!hit) return false;
                el.click();
                clicked.push({ how, text: text.slice(0, 25) });
                return true;
            };
            // Pass 1: dialog/consent-scoped selectors (the original set).
            const selectors = ['[aria-label*="close"]','[aria-label*="dismiss"]','[aria-label*="accept"]','[class*="cookie"] button','[class*="consent"] button','[role="dialog"] button'];
            for (const sel of selectors) {
                try {
                    for (const el of document.querySelectorAll(sel)) {
                        if (tryClick(el, sel) && clicked.length >= 3) return JSON.stringify({dismissed: clicked.length, details: clicked});
                    }
                } catch(e) {}
                if (clicked.length >= 3) break;
            }
            // Pass 2: ANY visible button whose TEXT matches (the old code
            // could only click elements matching the narrow selectors above,
            // so plain cookie banners with an "Accept" button were missed).
            if (clicked.length === 0) {
                for (const el of document.querySelectorAll('button, [role="button"], input[type="submit"], input[type="button"], a')) {
                    if (tryClick(el, 'text-match') && clicked.length >= 3) break;
                }
            }
            return JSON.stringify({dismissed: clicked.length, details: clicked});
        })()"#).await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        Ok(text_result(result.as_str().unwrap_or("no modals found")))
    }

    #[tool(
        description = "Evaluate a JavaScript expression in the page's main frame and return its JSON value. Read-only introspection is safest; treat results of mutations with care."
    )]
    async fn page_eval(
        &self,
        Parameters(PageEvalParams {
            session_id,
            page_id,
            expression,
        }): Parameters<PageEvalParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        // Hard 20s cap: an expression like `while(true){}` otherwise blocks
        // the call for ~100s (engine default) and leaves the page's event
        // loop busy. Fail fast with an actionable message instead.
        let value = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            page.evaluate(&expression),
        )
        .await
        .map_err(|_| {
            rmcp::model::ErrorData::internal_error(
                "page_eval timed out after 20s — the expression probably blocks (infinite loop); \
                 avoid while(true)/for(;;) and prefer incremental, awaitable expressions. \
                 NOTE: the page may stay unresponsive until the loop ends.",
                None,
            )
        })?
        .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_eval",
            Some(&page_id),
            serde_json::json!({ "len": expression.len() }),
        );
        Ok(text_result(
            serde_json::to_string(&value).unwrap_or_default(),
        ))
    }

    #[tool(
        description = "Capture a PNG screenshot of a page (viewport by default, full page with full_page=true). Saved under the session recordings dir; returns the file path. Feeds the live view when enabled."
    )]
    async fn page_screenshot(
        &self,
        Parameters(PageScreenshotParams {
            session_id,
            page_id,
            full_page,
        }): Parameters<PageScreenshotParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let png = page
            .screenshot(full_page.unwrap_or(false))
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let path = self
            .recorder
            .record_screenshot(&session_id, &page_id, &png)
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let url = settled_url(&page).await;
        crate::liveview::update(&session_id, &page_id, &url, png);
        Ok(text_result(
            serde_json::to_string_pretty(&serde_json::json!({
                "file": path.to_string_lossy(),
                "bytes": std::path::Path::new(&path).metadata().map(|m| m.len()).unwrap_or_default(),
            }))
            .unwrap_or_default(),
        ))
    }

    #[tool(
        description = "Solve an image/text CAPTCHA 100% LOCALLY with the built-in ddddocr ONNX model (no API keys, no third-party services): pass image_base64 (PNG, raw or data-URL). Standalone Turnstile/reCAPTCHA tokens are issued by the vendor server and cannot be minted on-device — for those, solve the interactive challenge on the live page instead (page_hcaptcha for image grids, page_click for checkboxes)."
    )]
    async fn captcha_solve(
        &self,
        Parameters(CaptchaSolveParams {
            session_id,
            sitekey: _,
            pageurl: _,
            image_base64,
        }): Parameters<CaptchaSolveParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let _ = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let started = std::time::Instant::now();
        let result = if let Some(b64) = image_base64 {
            crate::captcha::solve_image(&b64).await
        } else {
            return Err(rmcp::model::ErrorData::invalid_params(
                crate::captcha::NO_STANDALONE_TOKEN.to_string(),
                None,
            ));
        };
        match result {
            Ok(token) => {
                let _ = self.recorder.record(
                    &session_id,
                    "captcha_solve",
                    None,
                    serde_json::json!({ "ok": true, "seconds": started.elapsed().as_secs() }),
                );
                Ok(text_result(token))
            }
            Err(e) => {
                let _ = self.recorder.record(
                    &session_id,
                    "captcha_solve",
                    None,
                    serde_json::json!({ "ok": false, "error": e }),
                );
                Err(rmcp::model::ErrorData::internal_error(e, None))
            }
        }
    }

    #[tool(
        description = "Click an element by CSS selector. For form controls (buttons, inputs), uses a JS click; for links and other elements, dispatches real mouse events at coordinates. Prefer page_click_ref when you have a page_a11y ref — it scrolls into view first and is more reliable on web-component UIs. Returns 'ok' on success."
    )]
    async fn page_click(
        &self,
        Parameters(PageClickParams {
            session_id,
            page_id,
            selector,
        }): Parameters<PageClickParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        page.click(&selector)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_click",
            Some(&page_id),
            serde_json::json!({ "selector": selector }),
        );
        Ok(text_result("ok"))
    }

    #[tool(
        description = "Type text character-by-character into an element by CSS selector (human-like key events). Prefer page_type_ref when you have a page_a11y ref — it handles rich editors (Lexical/Draft/ProseMirror) and returns a verified receipt. Use this only when you only have a CSS selector and don't need rich editor support. Returns 'ok' on success."
    )]
    async fn page_type(
        &self,
        Parameters(PageTypeParams {
            session_id,
            page_id,
            selector,
            text,
        }): Parameters<PageTypeParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        page.type_text(&selector, &text)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_type",
            Some(&page_id),
            serde_json::json!({ "selector": selector, "chars": text.chars().count() }),
        );
        Ok(text_result("ok"))
    }

    #[tool(
        description = "Set an input's value directly (form fill). Works where key-event typing hits engine bugs; fires input/change events like real edits."
    )]
    async fn page_fill(
        &self,
        Parameters(PageFillParams {
            session_id,
            page_id,
            selector,
            text,
        }): Parameters<PageFillParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let sel = serde_json::to_string(&selector).unwrap_or_default();
        let expr = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return 'MISSING'; \
             el.value = {val}; el.dispatchEvent(new Event('input', {{bubbles: true}})); \
             el.dispatchEvent(new Event('change', {{bubbles: true}})); return 'OK'; }})()",
            sel = sel,
            val = serde_json::to_string(&text).unwrap_or_default(),
        );
        let res = page
            .evaluate(&expr)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        if res.as_str() == Some("OK") {
            // Receipt: confirm what landed where (guard against silent
            // page swaps eating the fill).
            let check = format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return 'GONE'; \
                 return el.value === null ? 'NOTFIELD' : String(el.value.length); }})()",
                sel = serde_json::to_string(&selector).unwrap_or_default()
            );
            let receipt = page
                .evaluate(&check)
                .await
                .unwrap_or(serde_json::Value::Null);
            let landed = receipt.as_str().and_then(|v| v.parse::<usize>().ok());
            let _ = self.recorder.record(&session_id, "page_fill", Some(&page_id), serde_json::json!({ "selector": selector, "chars": text.chars().count(), "landed": landed }));
            Ok(text_result(
                serde_json::to_string_pretty(&serde_json::json!({
                    "filled": selector,
                    "landed_chars": landed,
                    "requested_chars": text.chars().count(),
                }))
                .unwrap_or_default(),
            ))
        } else {
            Err(rmcp::model::ErrorData::internal_error(
                "selector not found".to_string(),
                None,
            ))
        }
    }

    #[tool(
        description = "Press a named key (Enter, Tab, Escape, ArrowDown, ...) — e.g. Enter to submit a search box."
    )]
    async fn page_press(
        &self,
        Parameters(PagePressParams {
            session_id,
            page_id,
            key,
        }): Parameters<PagePressParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let session = self
            .session(&session_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        let page = session
            .page(&page_id)
            .await
            .map_err(|e| rmcp::model::ErrorData::invalid_params(e.to_string(), None))?;
        // The core PageHandle trait has no press; the camoufox page does.
        // Downcast through the engine-specific handle.
        page.press_key(&key)
            .await
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        let _ = self.recorder.record(
            &session_id,
            "page_press",
            Some(&page_id),
            serde_json::json!({ "key": key }),
        );
        Ok(text_result("ok"))
    }

    #[tool(description = "Generate a new coherent browser identity, returned as TOML.")]
    async fn identity_generate(&self) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let id =
            ghostcloak_fingerprint::generate(&ghostcloak_fingerprint::GenerateOptions::default());
        let toml_str = id
            .to_toml()
            .map_err(|e| rmcp::model::ErrorData::internal_error(e.to_string(), None))?;
        Ok(text_result(toml_str))
    }

    #[tool(
        description = "Audit an identity TOML for coherence violations (contradictory signals a detector would flag)."
    )]
    async fn identity_audit(
        &self,
        Parameters(IdentityAuditParams { identity_toml }): Parameters<IdentityAuditParams>,
    ) -> Result<CallToolResult, rmcp::model::ErrorData> {
        let id: ghostcloak_fingerprint::Identity = toml::from_str(&identity_toml)
            .map_err(|e| rmcp::model::ErrorData::invalid_params(format!("bad TOML: {e}"), None))?;
        let violations = ghostcloak_fingerprint::audit(&id);
        if violations.is_empty() {
            Ok(text_result("clean: no violations"))
        } else {
            Ok(text_result(
                violations
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        }
    }
}

#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for GhostcloakServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        use rmcp::model::*;
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            server_info: Implementation::from_build_env(),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            instructions: Some(
                "Stealth browser for AI agents. Create a session, open pages, take snapshots, click and type. Identities are coherent by construction; use identity_audit to check any identity TOML.".into(),
            ),
        }
    }
}
