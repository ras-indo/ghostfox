# Changelog

All notable changes to ghostcloak will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`page_wait_for_timeout`** — explicit sleep/delay tool (blocked
  `timeout_ms` 1–60000, returns `{waited_ms, url}`). Fills the
  missing-tool gap for "wait until the challenge widget paints" when
  `page_wait_for_idle` returns too early; validates session/page FIRST
  so a dead session fails immediately instead of sleeping uselessly.
- **Fresh viewport screenshots after scroll (stale-frame fix)** —
  viewport `page_screenshot` used to send `clip: (0,0,w,h)`, but
  juggler's `drawSnapshot` clip is in DOCUMENT coordinates: the shot
  always rendered the TOP of the document no matter where the page
  was scrolled (scrollY 0→900 came back byte-identical, while paint
  changes in the top band did show up — the confusing "half-stale"
  signature). The capture now reads the live scroll offset and sends
  `clip.y = scrollY`, so a scrolled page returns exactly its visible
  viewport; `full_page` keeps `y=0` for the whole-document render.
  (An intermediate nsScreencastService-based path was tried and
  reverted: the native screencast track serves the same top-band
  frame.)

- **`page_clipboard`** — read/write the system clipboard via
  `navigator.clipboard` (mobile-mcp `mobile_clipboard` parity):
  `action=read` returns the current text, `action=write` places text
  (auto-grants `clipboard-read`/`clipboard-write` for the page origin
  first, so no on-page prompt blocks a copy/paste or 2FA-code flow).
- **`page_video`** — native page recording → `.webm` in the session
  recordings dir (mobile-mcp `start/stop_screen_recording` parity).
  Driven by juggler's OWN recorder (`Browser.setVideoRecordingOptions`
  on the default context → `nsScreencastService`, the same pipeline
  Playwright uses for `record_video_dir` — no JS-side frame pump, no
  re-encode cost in the agent). `start` (width/height default 1280x720)
  keeps recording across navigations and tabs; `stop` finalizes and
  returns `{file, bytes}` with a size-stability wait so the container is
  fully flushed; `status` peeks without stopping. The exact file path
  arrives via the `Page.videoRecordingStarted` event, now captured by
  the popup and live context pumps.
- **Tool annotations port (mobile-mcp parity)** — `#[tool(annotations)]`
  now carries MCP `title` / `destructive_hint` metadata: `page_close`
  and `session_close` are marked destructive (clients can warn before
  killing a page/session), the new clipboard/video tools carry titles.
  `page_screenshot`'s description gains mobile-mcp's anti-stale rule
  ("NEVER cache the returned image bytes across steps").
- **`page_screenshot` returns the image INLINE** — response is now
  `[text JSON metadata, image content block]` (mobile-mcp
  `mobile_take_screenshot` parity): the calling client SEES the screenshot
  directly instead of only receiving a file path. Metadata gains
  `width`/`height` and `inline`; when `max_dim` scaled the capture, a
  `coordinate_mapping` hint (screenshot→page multiply factor) is included so
  the client can map what it sees back to page pixels. Inline gracefully
  skipped above 4 MiB with an `inline_skipped` shrink hint (`max_dim` /
  `format=jpeg`) so a huge full-page capture can never blow the MCP message
  cap.
- **`page_click` variants** — `button: "right"` (contextmenu) and
  `click_count: 2` (double-click) with REAL engine mouse events on Camoufox
  (isTrusted; ~90ms human gap between double-clicks), DOM-dispatch fallback
  for form controls / other engines via the new `PageHandle::click_variant`
  trait method (default impl = portable JS dispatch).
- **`browser_exec`** — browser-use-style escape hatch: run a multi-statement
  JS program in the page with `print()`/console capture, pre-imported
  helpers (`page_info`, `$`/`$$`, `js`, `type_text`, `list_tabs`), and
  QUEUED engine actions (`goto_url`, `new_tab`, `click_at_xy` with real
  mouse, `wait_for_load`) executed after the script returns — inspect-then-
  act in one call. Server-side `ns` scratch object persists across calls
  and navigation (64KB cap, `reset_ns` to clear); calls are serialized by
  an exec lock; errors return as tracebacks without failing the call;
  20s hard timeout.
- **`page_wait_for_text`** — wait until a string appears in visible body
  text + title (browser-use `wait_for(text)` mode), case-insensitive by
  default, 200ms poll, returns `{found, waited_ms}` — the content-based
  counterpart to `page_wait_for` (selector-based).
- **`page_wait_rate_limit`** — AGENTS.md playbook automation ("NEVER retry
  into a rate limit, WAIT it out"): reuses page_a11y's parsed
  `rate_limit_seconds` + notifications ("try again in N seconds/minutes"
  toasts), sleeps out the stated window with jitter, re-reads a fresh
  snapshot, and reports `{cleared, waited_s, attempts, rate_limit_seconds,
  notifications}` — bounded (default 60s, hard cap 180s), READ-ONLY, and
  does not hold the exec lock, so a throttled site gets one paced wait
  instead of a hammer.
- **`page_geetest_slide` div mode (GeeTest v4 adaptive)** — when the
  challenge ships NO canvases (v4 renders `.geetest_bg` /
  `.geetest_slice_bg` / `.geetest_fullbg` as div background-images),
  the extractor falls back to those layers: Rust fetches the layer
  URLs, replays each element's CSS `background-size`/`position` onto
  an offscreen raster (`render_bg_layer`), and solves from the
  rendered pixels. Slider handle resolves `.geetest_slider_button` →
  `.geetest_btn` → `.geetest_btnFix`; the response reports which path
  it took (`source: "canvas"` / `"div:..."`). When v4 ships no
  fullbg, `slide_gap_no_full` template-matches the slice into the bg
  (highpass + normalized cross-correlation) instead of the |bg-fullbg|
  diff, and implausible drags (> bg width) refuse to drag rather than
  mis-slide.

### Fixed

- **`page_geetest_slide` waits for the layer layout (0x0 poll)** — the
  div-mode background/slice rect can read 0x0 for a moment while the
  challenge is still animating in or the holder has not been laid out;
  the tool used to reject that as `degenerate background layer` even
  though the challenge WAS open. It now re-evaluates up to 10x at
  400ms before giving up (the loud "is the slide challenge open?" error
  is unchanged for a genuinely closed challenge).

- **Android personas no longer render tofu (bug #7)** — every content
  font lookup passes the engine's masked-font-list allowlist
  (`cfg["fonts"]`, checked after fontconfig substitution), and the
  Android identity carried Android's native names (Roboto, Droid Sans
  Mono, Coming Soon, Carrois Gothic SC) which are NOT in the bundled
  Linux font set. The allowlist therefore blocked every family and all
  text rendered as `.notdef` boxes — screenshots, DOM and canvas alike
  — while Windows/Mac/Linux personas drew fine. The Android persona
  now reports the Linux font stack (`LINUX_FONTS`), which this engine
  actually draws with, satisfying camoufox's own invariant that
  everything reported must be renderable.
- **`FONTCONFIG_PATH` pointed at directories that do not exist** —
  `env_for_identity` mapped OS → `fontconfig/win|mac|lin` but the
  shipped tree is `fontconfig/windows|macos|linux`, so the bundled
  per-OS fontconfig was never found and fontconfig fell back to
  whatever it could resolve. Corrected the mapping (Android keeps the
  Linux stack).

- **`page_open` no longer fails silently** — a navigation that dies
  before commit (e.g. `NS_ERROR_NET_EMPTY_RESPONSE`) used to return a
  healthy-looking page id that sat on `about:blank` forever, making
  every follow-up call fail confusingly. `page_open` now polls briefly
  past the DOM wait and, if the page never left `about:`, closes it and
  returns an explicit `NAVIGATION FAILED` error with remediation hints.
- **Actionable `session not found`** — the error now explains the two
  real causes (closed vs server restart) and what to call next
  (`session_list` / `session_create`, `profile_dir` to reuse identity).
  `session_create`'s description documents that every call mints a NEW
  random identity unless the same `profile_dir` is passed.

- **Execution-context recovery overhaul — `Failed to find execution
  context with id = mainframe-N`** — the context pumps (startup, live,
  popup) adopted a frame only by EQUALITY against a pinned
  `main_frame_id`, so after a renderer crash / tab-recreate (the top
  frame comes back with a NEW `mainframe-N`) every recovery event was
  rejected forever and evaluate() kept retrying the dead id, eventually
  leaking juggler's raw channel error. All three pumps now use the id
  PREFIX rule (`mainframe-*` inside a session-filtered pump IS our top
  frame), `Page.navigationCommitted` re-pins on any top-frame commit, and
  the live pump gained `Browser.attachedToTarget` /
  `Browser.detachedFromTarget` / `Inspector.targetCrashed` arms (session
  self-heal for main pages + full id clear on detach/crash).
  `evaluate()` itself is a bounded recovery ladder: fail fast with an
  actionable message when the renderer is flagged crashed; on a stale
  rejection, drop the rejected id — and the frame pins TOO only when the
  frame id itself was the rejected target (a rejected id is proven dead,
  so retrying a corpse frame id 4× is impossible now, while a live frame
  survives to anchor the fallback); busy-page timeouts never touch the
  pins (same-document navigations emit no replacement events, so clearing
  there would starve the retry loop) and stop after two attempts instead
  of spinning for minutes; with BOTH pins empty there is NO event-free
  primitive — juggler REQUIRES `executionContextId` ("Expected <root>…
  to be |string|") and does not implement `Runtime.enable` — so evaluate
  now forces ONE `Page.reload` self-heal instead, which deterministically
  mints fresh `executionContextCreated` events the pump adopts (in-page
  state is lost, but the page stays usable — better than a dead handle;
  this covers event-storm starvation: the juggler broadcast channel grew
  256 → 4096 because a Lagged overflow silently dropped
  `executionContextCreated` and left the handle pinned to nothing).
  Falls back to the freshly pinned frame id, and finally returns guidance
  ("reopen with page_open") instead of raw channel noise.
- **Same-document `Page.navigate` no longer strands the handle** —
  navigate() used to unconditionally clear the context pin and wait 8s for
  a successor event; a hash/query-only jump keeps the SAME document, so
  juggler may emit destruction with NO creation event and the wait timed
  out with pins empty — every later evaluate then starved (the
  `mainframe-11` signature). navigate() now compares `location.href`
  against the target FIRST (pins still valid) and, on a same-document
  move, keeps the pins and skips the wait entirely (also removing the
  silent 8s penalty per hash-goto). `settle_context` (back/forward/reload)
  gained the same safety: when the wait times out it adopts the surviving
  frame id, since juggler resolves a frame's default context from its id.
  All pin-clearing pump arms (detach/crash/lagged/destroyed/cleared) now
  log which event cleared what, so the next regression is traceable in
  one debug run.
- **`browser_exec` same-document `goto_url` wedged the exec lock** — a
  fragment-only jump never recreates the execution context, so navigate()'s
  context clear left evaluate() churning (~30min of retries) and every
  subsequent browser_exec call blocked behind `exec_lock`. Fragment-only
  URLs now jump in-page (`location.href`), and `wait_for_dom` got an
  overall 12s deadline instead of an iteration count alone.
- **`browser_exec` action-loop bounds (audit)** — `settled_url` now has an
  overall 10s deadline (24 × ~112s worst-case evaluate ≈ 45min), the
  queued `click_at_xy` arm is capped at 30s (`drag_ref` is a long
  humanized RPC sequence with only per-step bounds), and the whole queued
  actions loop shares a 90s budget that stops with an honest
  `{op: "budget"}` entry — no single arm can pin `exec_lock` for minutes
  anymore.
- **`browser_exec` remaining naked `page.url()` reads bounded** (post-review)
  — the tab-listing loop (per tab, before the script's 20s cap) and the
  goto arm's same-document probe now hard-cap at 3s each; both were
  evaluate-cost (~112s worst on a dead context) sitting inside `exec_lock`.
- **`page_a11y` sanitization: opacity checked up the ancestor chain**
  (post-review) — opacity does NOT inherit, so an `opacity:0` ancestor
  with an `opacity:1` child reported computed opacity 1 and let hidden
  text through into the sanitized `name`/`value`; `visibleText` now walks
  `parentElement` to the root (font-size/visibility already resolve via
  computed inheritance, `aria-hidden` already used `closest()`).
- **`page_a11y` sanitization: offscreen plants stripped** (post-review) —
  text fully outside the document (`top:-9999px`, `left:-9999px`) or in a
  zero-size box is invisible to a human but innerText still counted it;
  `visibleText` now checks the parent's bounding rect against the
  document scroll size (residual: `clip-path`/`clip` masking not detected).
- **`page_open` DOM wait bounded** (post-review, pre-existing) — the
  inline 60×250ms `!!document.body` loop had no overall deadline (same
  wedge class as `wait_for_dom`); it now reuses `wait_for_dom` (12s).
- **Cancelled click now releases the button** (post-review) — the 30s
  `click_at_xy` timeout drops the `drag_ref` future between mousedown and
  mouseup, and that future can no longer run its internal `force_release`;
  a stuck-pressed button wedges the whole Juggler session. New
  `PageHandle::release_mouse_at(x, y)` (default: unsupported error;
  Camoufox: retrying `force_release`) is called best-effort (5s cap)
  on the timeout path.

### Changed

- **`browser_exec` responses are honest about state** (post-review) —
  an oversize `ns` (>64KB cap) now returns `ns_dropped: true` instead of
  silently discarding the scratch namespace; a queued `wait` action
  reports `settled: bool` (whether the DOM actually appeared within the
  deadline, not just that we waited); an unrecognized queued action op
  returns `{ok: false, error}` instead of being dropped without a trace.
- **`page_a11y` content sanitization** (ROADMAP v0.5 prompt-injection
  defense) — hidden text (`font-size:0`, `opacity<0.01`, `visibility:hidden`,
  `aria-hidden` subtrees) is now STRIPPED from innerText-derived element
  `name`/`value` instead of reaching the agent, and reported: per-element
  `stripped` (invisible chars removed) + snapshot `stripped_content` (count
  of affected elements).

## [0.8.0] — 2026-09-27

### Added

- **MCP Streamable HTTP transport** — `--transport http` (and `both`) serves the
  same 42-tool MCP surface over `http://127.0.0.1:8787/mcp` via official rmcp
  `StreamableHttpService`. Single endpoint, modern protocol semantics (no legacy
  HTTP+SSE split).
- **API-key authentication** — mandatory for HTTP: `Authorization: Bearer
  <key>`; auto-generated 256-bit random key on first start, stored with `0600`
  permissions; constant-time comparison; generic `401` on failure (no key
  material leaked).
- **Origin validation** — `allowed_origins` allowlist; invalid Origin → `403`;
  requests without Origin (non-browser clients) accepted per protocol.
- **Configuration system** — `~/.ghostfox/config.toml` (or `GHOSTFOX_CONFIG`),
  precedence CLI > ENV > FILE > DEFAULT, strict parsing with clear errors.
- **Hot reload** — 5s config poll; API key rotation and origin changes apply
  live; invalid reloads keep the last known-good config. Socket fields require
  restart.
- **`/healthz`** — unauthenticated liveness probe (no auth, no tool calls, no
  internals).
- **Cross-platform CI** — fmt/clippy/test/release-build on Ubuntu, macOS,
  Windows; package checks (npm + Python); Docker build.
- **Deterministic release pipeline** — single `release.yml` DAG: exact-SHA
  checkout, artifact-only flow, checksums + release manifest, all-or-nothing
  release gate before GitHub Release / GHCR push.

### Changed

- `ghostcloak-mcp` binary is now a thin wrapper over the library crate; stdio
  mode unchanged and fully backward compatible.

### Security

- API key never logged; no CORS wildcard for credentialed requests; localhost
  default bind; docs for remote HTTPS reverse-proxy deployment.

## [0.1.0] — 2026-09-03

First public release. Rust-native agent browser runtime with a patched-Firefox engine.

### Added

- **ghostcloak-core** — engine-agnostic runtime: `Engine`/`PageHandle` traits, session vault, engine registry.
- **ghostcloak-fingerprint** — declarative identities as TOML: coherent device-preset generator (platform/screen/GPU/fonts that actually ship together), auditor that rejects contradictory signals, content-hash fingerprinting per identity.
- **ghostcloak-camoufox** — patched-Firefox engine adapter speaking the Juggler wire protocol natively from Rust (fd 3/4 pipes, `\0`-framed JSON, per-target session routing, live execution-context tracking). Zero Python/Node at runtime.
  - Identity injection via `CAMOU_CONFIG` env — spoofing happens inside the engine at the C++ level.
  - Per-character key-event typing (Enter/Tab/Space with correct key codes), named-key `press_key`, mouse-event clicks with JS-click fallback for form controls.
  - Self-healing snapshot (context-loss recovery via reload), process-group teardown, ephemeral profile lifecycle.
- **ghostcloak-mcp** — MCP server (stdio) exposing 9 narrow, typed tools: `session_create`, `page_open`, `page_snapshot`, `page_click`, `page_type`, `page_fill`, `page_press`, `identity_generate`, `identity_audit`.
- **ghostcloak-eval** — the stealth referee: offline identity-coherence audits (T0), JS-surface probes against live pages (T1/T2).

### Verified

- Identity coherence: 500/500 generated identities pass the auditor.
- Full E2E over MCP stdio (8/8): create session → open page → fill form → click submit → value present in the server's POST echo.
- Anti-detect JS surface matches the generated identity (UA/platform/locale/timezone/cores), `navigator.webdriver` false.
- Multi-page sessions keep independent live execution contexts (per-page event routing).

### Known issues

- Heavy fingerprint-test sites (CreepJS) crash the engine's content channel — upstream engine bug, tracked.
- Google may serve `/sorry/` challenge pages on datacenter IPs following a homepage→search pattern; pass a `proxy` to `session_create` or navigate directly to search URLs.

## [0.2.0] — 2026-09-08

### Added

- **Evidence recording** — every session writes an append-only `events.jsonl`
  plus full page snapshots and `identity.toml` under
  `~/.ghostfox/recordings/<session>/`; new `session_evidence` MCP tool
  returns the log, files and persona (evidence primitive for run audit and
  replay).
- **Android personas** — new `Platform::Android`: portrait screens with
  dpr ≥ 2, Adreno/Mali GPU strings, Android font stacks, Firefox-on-Android
  UAs (Android version tracked from the identity), `navigator.platform`
  `Linux aarch64`, auditor rules for portrait/dpr/GPU coherence.
  `session_create {"platform":"android"}`.
- Identity benchmark published: 500/500 coherent (Windows 135 · Android 161 ·
  MacOS 103 · Linux 101), see `docs/benchmark-2026-09-08.md`.

## [0.3.0] — 2026-09-08 (same day, second wave)

### Added

- **Touch-coherent Android personas** — `navigator.maxTouchPoints` spoofed at
  the C++ level (engine patch `navigator-touch-spoofing.patch`), Juggler touch
  override at launch (coarse pointer + touch events), viewport follows the
  identity screen class. Verified end-to-end: 5 touch points, pointer:coarse,
  no hover, portrait viewport, Linux aarch64 platform.
- **page_screenshot MCP tool** — Juggler `Page.screenshot` (viewport or full
  page), PNG evidence under `~/.ghostfox/recordings/<session>/screenshots/`.
- **Live view (opt-in)** — `GHOSTFOX_LIVE_VIEW_PORT` serves per-page latest
  PNGs + index, with a 5s auto-capture ticker.
- **captcha_solve MCP tool** — 2captcha hook (Turnstile + image), env-config
  (GHOSTFOX_CAPTCHA_PROVIDER/GHOSTFOX_CAPTCHA_KEY), attempts recorded in
  evidence without key material.
- **eval `targets` mode** — live-target probe (sannysoft detector panel +
  areyouheadless referee + sanity pages) with JSONL scorecard; first run:
  4/4 ok, 0 gated from a datacenter IP.
- **Python package** — `pip install ghostfox` (installer + sync MCP client).

### Fixed

- **iframe context hijack (major)** — the execution-context pump tracked the
  newest context, so pages with iframes (most real sites, bot.sannysoft.com's
  srcdoc test frames) made snapshots/clicks/typing evaluate inside an iframe.
  The pump now tracks the page's main frame only (via auxData.frameId).
- Android viewport now portrait-sized from the identity (was hardcoded
  1280x800).
- maxTouchPoints config check ordered before the RDM-pane branch (a juggler
  viewport request sets inRDMPane, which masked the config).

## [0.3.0] — 2026-09-09

### Added

- **`page_a11y` — eyes for agents.** Semantic snapshot of the page: every
  visible interactive element with a stable ref, role, accessible name and
  CURRENT value. Walks shadow DOM (the #1 blind spot of selector-based
  automation — modern web-component UIs like Reddit's shreddit-* hide
  fields there). Values are read live, so form state is always visible.
- **`page_click_ref` / `page_type_ref`** — act by ref, no selectors. Clicks
  scroll into view first; typing handles plain inputs AND rich editors
  (Lexical/Draft/ProseMirror) via synthetic-paste + insertText with a
  fire-then-verify receipt (async editors settle before the check).
- Proven the same day it was born: posted to Reddit end-to-end (3 tool
  calls) on a composer that had defeated selector automation for hours.

## [0.3.1] — 2026-09-09 (gap-closing wave)

### Added

- **`page_read_ref`** — full-value read by ref (no 200-char truncation);
  use when the a11y snapshot's preview isn't enough
- **`page_wait_for`** — poll until a CSS selector becomes visible (with
  timeout); replaces agent-side `sleep 8` guessing
- **`page_upload_file`** — upload a local file to `input[type=file]` via
  synthetic DataTransfer (bypasses the native file picker)
- **a11y walker now pierces same-origin iframes** in addition to shadow
  roots (cross-origin is blocked by browser security — by design)

## [0.4.0] — 2026-09-09

### Added

- **`page_a11y` now reports `login_state`** — "logged-in" | "logged-out" |
  "unknown" — detected from login buttons vs user-menu signals, so agents
  check session health before acting instead of discovering a dead session
  the hard way.
- **`page_a11y` returns page URL + title** alongside elements.
