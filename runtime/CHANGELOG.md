# Changelog

All notable changes to ghostcloak will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

### Changed

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
