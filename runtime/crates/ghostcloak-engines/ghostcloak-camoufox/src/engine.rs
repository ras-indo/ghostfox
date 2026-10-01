//! Camoufox engine implementation: launches the patched Firefox binary with
//! our identity injected via CAMOU_CONFIG env vars, drives pages over the
//! Juggler pipe.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use ghostcloak_core::engine::{
    dom_click_variant_js, Engine, EngineKind, LaunchOptions, PageHandle, PageSnapshot,
};
use ghostcloak_core::error::{GhostError, Result};
use ghostcloak_fingerprint::Identity;

use crate::config;
use crate::juggler::JugglerConnection;

pub struct CamoufoxEngine {
    conn: Arc<JugglerConnection>,
    identity: Identity,
    #[allow(dead_code)]
    home: PathBuf,
    /// Ephemeral profile dir to remove on shutdown (None = user-provided,
    /// persistent).
    ephemeral_profile: Option<PathBuf>,
    /// v0.6.2: POPUP/TAB REGISTRY — every attached browser target
    /// (targetId -> sessionId), including site-opened popups. Juggler
    /// auto-attaches new targets and announces them via
    /// `Browser.attachedToTarget`; previously only pages WE created were
    /// tracked, making OAuth popups (Google login, payment windows...)
    /// invisible to the agent. Kept in sync by a dedicated listener.
    targets: tokio::sync::Mutex<std::collections::HashMap<String, String>>,
    /// v0.6.2: session -> latest (executionContextId, frameId) so popup
    /// handles can be born fully initialized (contexts fire at popup
    /// creation, before anyone attaches).
    contexts: tokio::sync::Mutex<std::collections::HashMap<String, (String, String)>>,
}

/// Parse a proxy URL: `socks5://host:port`, `socks://host:port`,
/// `socks4://host:port`, `http(s)://host:port`, or bare `host:port`
/// (treated as http). Returns (scheme, host, port).
fn parse_proxy(raw: &str) -> std::result::Result<(&'static str, String, u16), String> {
    let (scheme, rest) = match raw.find("://") {
        Some(i) => {
            let sc = match &raw[..i] {
                "socks5" | "socks" => "socks5",
                "socks4" => "socks4",
                "http" => "http",
                "https" => "https",
                other => {
                    return Err(format!(
                        "unsupported scheme `{other}` — use socks5://host:port or http://host:port"
                    ))
                }
            };
            (sc, &raw[i + 3..])
        }
        None => ("http", raw),
    };
    // Strip optional userinfo (user:pass@host:port).
    let rest = rest.rsplit('@').next().unwrap_or(rest);
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| format!("missing `:port` in `{raw}` (e.g. socks5://127.0.0.1:9050)"))?;
    let port: u16 = port
        .trim()
        .parse()
        .map_err(|_| format!("invalid port in `{raw}`"))?;
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Err(format!("missing host in `{raw}`"));
    }
    Ok((scheme, host.to_string(), port))
}

/// TCP reachability check for a proxy endpoint (host may be a name).
fn probe_tcp(host: &str, port: u16, timeout: std::time::Duration) -> std::io::Result<()> {
    use std::net::{TcpStream, ToSocketAddrs};
    let addrs: Vec<_> = (host, port).to_socket_addrs()?.collect();
    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "host did not resolve",
        ));
    }
    let mut last = std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no addresses");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(_) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Write Firefox `network.proxy.*` prefs into the profile's user.js.
/// Idempotent: a previously generated block (marker comments) is stripped
/// first, so relaunching with a different proxy never leaves stale rules.
fn write_proxy_prefs(profile: &std::path::Path, raw: &str) -> std::io::Result<()> {
    let (scheme, host, port) =
        parse_proxy(raw).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mut block = String::from("// GHOSTFOX-PROXY-BEGIN (generated, safe to replace)\n");
    block.push_str("user_pref(\"network.proxy.type\", 1);\n"); // 1 = manual config
    match scheme {
        "socks5" | "socks4" => {
            let version = if scheme == "socks4" { 4 } else { 5 };
            block.push_str(&format!(
                "user_pref(\"network.proxy.socks\", \"{host}\");\n"
            ));
            block.push_str(&format!(
                "user_pref(\"network.proxy.socks_port\", {port});\n"
            ));
            block.push_str(&format!(
                "user_pref(\"network.proxy.socks_version\", {version});\n"
            ));
            // Resolve .onion / remote hostnames through the proxy itself.
            block.push_str("user_pref(\"network.proxy.socks_remote_dns\", true);\n");
        }
        _ => {
            block.push_str(&format!("user_pref(\"network.proxy.http\", \"{host}\");\n"));
            block.push_str(&format!(
                "user_pref(\"network.proxy.http_port\", {port});\n"
            ));
            block.push_str(&format!("user_pref(\"network.proxy.ssl\", \"{host}\");\n"));
            block.push_str(&format!("user_pref(\"network.proxy.ssl_port\", {port});\n"));
        }
    }
    block.push_str("// GHOSTFOX-PROXY-END\n");

    let path = profile.join("user.js");
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    const BEGIN: &str = "// GHOSTFOX-PROXY-BEGIN";
    const END: &str = "// GHOSTFOX-PROXY-END";
    if let Some(start) = content.find(BEGIN) {
        match content[start..].find(END) {
            Some(rel) => {
                let end = start + rel + END.len() + 1;
                content.replace_range(start..end.min(content.len()), "");
            }
            None => content.truncate(start),
        }
    }
    content.push_str(&block);
    std::fs::write(&path, content)
}

/// Juggler (Firefox) emits FLAT event params — `params.url`/`params.method`
/// — unlike Chrome CDP where they nest under `params.request.*`. The old
/// code only read the Chrome shape, so every captured url was `""` and
/// every status `null`. Read both shapes.
fn net_url(msg: &serde_json::Value) -> String {
    msg.pointer("/params/url")
        .or_else(|| msg.pointer("/params/request/url"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn net_method(msg: &serde_json::Value) -> String {
    msg.pointer("/params/method")
        .or_else(|| msg.pointer("/params/request/method"))
        .and_then(|v| v.as_str())
        .unwrap_or("GET")
        .to_string()
}

fn net_status(msg: &serde_json::Value) -> Option<u64> {
    msg.pointer("/params/status")
        .or_else(|| msg.pointer("/params/response/status"))
        .and_then(|v| v.as_u64())
}

impl CamoufoxEngine {
    pub async fn launch(opts: &LaunchOptions) -> Result<Arc<Self>> {
        // Prefer the caller's pre-generated identity (identity_toml) so the
        // fingerprint recorded in evidence is EXACTLY what the engine runs
        // with. Fall back to load_or_generate only when none was provided —
        // a silently re-randomized identity would break coherence (evidence
        // says "Linux" while the runtime UA says "Android").
        let identity: Identity = match opts
            .identity_toml
            .as_ref()
            .filter(|t| !t.trim().is_empty())
            .and_then(|t| Identity::from_toml_str(t).ok())
        {
            Some(id) => id,
            None => Identity::load_or_generate(opts.profile_dir.as_deref())?,
        };
        let (home, bin_name) = autodetect_engine().ok_or_else(|| {
            GhostError::EngineUnavailable("ghostfox/camoufox binary not found".into())
        })?;

        // Ephemeral profiles live under a sweepable root so shutdown can
        // clean them and a crashed run can't strand them all over /tmp.
        let (profile, ephemeral) = match &opts.profile_dir {
            Some(dir) => {
                let mut p = PathBuf::from(dir);
                if p.is_dir() {
                    p = p.join("camoufox-profile");
                }
                (p, false)
            }
            None => {
                let p = std::env::temp_dir()
                    .join("ghostcloak-profiles")
                    .join(ghostcloak_core::util::short_id());
                (p, true)
            }
        };
        // Camoufox refuses to boot without an existing profile directory.
        std::fs::create_dir_all(&profile)?;

        // NOTE: the launcher (`ghostfox`/`camoufox`) re-execs `-bin` WITHOUT
        // preserving the juggler pipes. Always spawn the -bin binary.
        let mut cmd = std::process::Command::new(home.join(bin_name));
        cmd.arg("--juggler-pipe")
            .arg("-silent")
            .arg("-profile")
            .arg(&profile)
            .arg("-no-remote")
            // Run from the install dir: the engine resolves helper binaries
            // (glxtest etc.) relative to its working directory.
            .current_dir(&home);

        // Juggler pipe convention (from Playwright's FirefoxConnection):
        // stdio = [ignore, pipe, pipe, pipe, pipe] — the juggler channel is
        // fd 3 (browser reads) + fd 4 (browser writes), NOT stdin/stdout.
        // os_pipe::pipe() returns (reader, writer).
        let (cmd_rx, cmd_tx) = os_pipe::pipe().map_err(|e| GhostError::Protocol(e.to_string()))?;
        let (resp_rx, resp_tx) =
            os_pipe::pipe().map_err(|e| GhostError::Protocol(e.to_string()))?;

        // Child fd 3 <- cmd_rx (we write commands into cmd_tx).
        // Child fd 4 -> resp_tx (we read responses from resp_rx).
        #[cfg(unix)]
        {
            let cmd_rx_fd = cmd_rx.as_raw_fd();
            let resp_tx_fd = resp_tx.as_raw_fd();
            // Leak the child-bound ends: their fds must stay open for the
            // child's lifetime (this mirrors the proven fd_probe flow).
            std::mem::forget(cmd_rx);
            std::mem::forget(resp_tx);
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(move || {
                    // Own process group: contentprocs must die with the parent.
                    libc::setsid();
                    // dup the pipe ends onto the juggler fds...
                    if libc::dup2(cmd_rx_fd, 3) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::dup2(resp_tx_fd, 4) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // ...and clear O_CLOEXEC: os_pipe creates pipes with
                    // CLOEXEC set, and dup2 inherits the flag, which would
                    // close fds 3/4 at exec and kill the juggler channel.
                    let flags = libc::fcntl(3, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                    }
                    let flags4 = libc::fcntl(4, libc::F_GETFD);
                    if flags4 >= 0 {
                        libc::fcntl(4, libc::F_SETFD, flags4 & !libc::FD_CLOEXEC);
                    }
                    Ok(())
                });
            }
        }

        // Windows uses the same Playwright protocol, but handles are passed
        // through inherited environment values instead of fixed fd numbers.
        #[cfg(windows)]
        {
            let read_handle = cmd_rx.as_raw_handle();
            let write_handle = resp_tx.as_raw_handle();
            unsafe {
                use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};

                if SetHandleInformation(read_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0
                    || SetHandleInformation(write_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
                        == 0
                {
                    return Err(GhostError::Protocol(
                        "camoufox pipe handles cannot be inherited".into(),
                    ));
                }
            }
            cmd.env("PW_PIPE_READ", format!("{}", read_handle as isize));
            cmd.env("PW_PIPE_WRITE", format!("{}", write_handle as isize));
            std::mem::forget(cmd_rx);
            std::mem::forget(resp_tx);
        }

        if opts.headless {
            cmd.arg("--headless");
        }
        if let Some(proxy) = &opts.proxy {
            // The engine has NO proxy CLI flags: --socks-proxy/--proxy-server
            // were silently ignored (zero such strings exist in libxul), so
            // the proxy param used to be accepted while traffic went DIRECT.
            // Firefox routing is driven purely by network.proxy.* prefs —
            // write them into the profile's user.js before launch.
            write_proxy_prefs(&profile, proxy)
                .map_err(|e| GhostError::Protocol(format!("proxy config `{proxy}`: {e}")))?;
            // Probe the endpoint: a dead proxy used to be accepted silently
            // and only failed later, at request time, on every navigation.
            let (_, phost, pport) = parse_proxy(proxy).map_err(GhostError::Protocol)?;
            probe_tcp(&phost, pport, std::time::Duration::from_secs(4)).map_err(|e| {
                GhostError::Protocol(format!(
                    "proxy `{proxy}` is not reachable ({e}) — refusing to start a session that cannot route traffic"
                ))
            })?;
        }
        for extra in &opts.extra_args {
            cmd.arg(extra);
        }

        // Identity injection — the whole point of this engine.
        for (k, v) in config::env_for_identity(&identity, &home) {
            cmd.env(k, v);
        }

        // Spawn via std::process::Command. The child stays owned by the
        // Juggler connection so shutdown can terminate it deterministically.
        let child = cmd
            .spawn()
            .map_err(|e| GhostError::Protocol(format!("camoufox spawn: {e}")))?;
        let pid = child.id();
        let conn = JugglerConnection::spawn_std(child, pid, cmd_tx, resp_rx)?;

        // Mobile personas: enable the engine's touch override for the default
        // context — (pointer: coarse) media queries + touch event dispatch,
        // the same mechanism Playwright's `hasTouch` uses. maxTouchPoints
        // is handled through CAMOU_CONFIG by the engine patch.
        if identity.platform == ghostcloak_fingerprint::identity::Platform::Android {
            let _ = conn
                .request(
                    "Browser.setTouchOverride",
                    serde_json::json!({ "hasTouch": true }),
                )
                .await;
        }

        let engine = Arc::new(Self {
            conn,
            identity,
            home,
            ephemeral_profile: ephemeral.then_some(profile),
            targets: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            contexts: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        });
        // v0.6.2: POPUP LISTENER — record every attached target so
        // site-opened popups (OAuth, payments) are discoverable and
        // attachable by the agent. Detach removes them.
        {
            let engine_for_listener = engine.clone();
            let mut events = engine_for_listener.conn.subscribe();
            tokio::spawn(async move {
                while let Ok(msg) = events.recv().await {
                    let method = msg.get("method").and_then(|m| m.as_str());
                    if method == Some("Browser.attachedToTarget") {
                        let tid = msg
                            .pointer("/params/targetInfo/targetId")
                            .or_else(|| msg.pointer("/params/targetId"))
                            .and_then(|v| v.as_str());
                        let sid = msg.pointer("/params/sessionId").and_then(|v| v.as_str());
                        if let (Some(tid), Some(sid)) = (tid, sid) {
                            engine_for_listener
                                .targets
                                .lock()
                                .await
                                .insert(tid.to_string(), sid.to_string());
                        }
                    }
                    if method == Some("Browser.detachedFromTarget") {
                        if let Some(tid) = msg.pointer("/params/targetId").and_then(|v| v.as_str())
                        {
                            engine_for_listener.targets.lock().await.remove(tid);
                        }
                    }
                    // Track the latest MAIN-FRAME execution context per
                    // SESSION so popup handles attach fully-initialized.
                    // Juggler frame ids: "mainframe-N" = page main frame,
                    // "subframe-N" = iframes — recording an iframe context
                    // here makes attach evaluate against the wrong world
                    // (live bug: popup attached to a subframe context).
                    if method == Some("Runtime.executionContextCreated") {
                        let sid = msg.get("sessionId").and_then(|s| s.as_str());
                        let cx = msg
                            .pointer("/params/executionContextId")
                            .and_then(|v| v.as_str());
                        let fid = msg
                            .pointer("/params/auxData/frameId")
                            .and_then(|v| v.as_str());
                        if let (Some(sid), Some(cx), Some(fid)) = (sid, cx, fid) {
                            if is_main_frame_id(fid) {
                                engine_for_listener
                                    .contexts
                                    .lock()
                                    .await
                                    .insert(sid.to_string(), (cx.to_string(), fid.to_string()));
                            }
                        }
                    }
                }
            });
        }
        Ok(engine)
    }

    /// v0.6.2: The (targetId -> sessionId) registry of ALL attached
    /// browser targets, including site-opened popups.
    pub async fn all_targets(&self) -> Vec<(String, String)> {
        self.targets
            .lock()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// v0.6.2: Attach to an EXISTING browser target (popup/tab opened by
    /// the site — OAuth windows, payment flows). Returns a fully live
    /// page handle using the recorded session + context.
    pub async fn attach_target(
        &self,
        target_id: &str,
    ) -> Result<Arc<dyn ghostcloak_core::engine::PageHandle>> {
        let sid = self
            .targets
            .lock()
            .await
            .get(target_id)
            .cloned()
            .ok_or_else(|| {
                GhostError::PageOp(format!(
                    "target {target_id} is not attached (open popup unknown?)"
                ))
            })?;
        let handle = CamoufoxPage::new(self.conn.clone(), target_id.to_string());
        *handle.session_id.lock().await = Some(sid.clone());
        if let Some((cx, fid)) = self.contexts.lock().await.get(&sid).cloned() {
            *handle.execution_context_id.lock().await = Some(cx);
            *handle.frame_id.lock().await = Some(fid.clone());
            *handle.main_frame_id.lock().await = Some(fid);
        } else {
            // No context seen yet (e.g. blank popup) — the pump below fills
            // ids as soon as the popup navigates.
            *handle.frame_id.lock().await = Some(String::new());
        }
        // Context pump: keep ids live across the popup's future navigations
        // (same cross-talk protection as new_page).
        {
            let conn = self.conn.clone();
            let _ = conn;
            let handle2 = handle.clone();
            let mut rx = self.conn.subscribe();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(msg) => {
                            let method = msg.get("method").and_then(|m| m.as_str());
                            if let Some(evt_sid) = msg.get("sessionId").and_then(|s| s.as_str()) {
                                let my_sid = handle2.session_id.lock().await.clone();
                                if Some(evt_sid) != my_sid.as_deref() {
                                    continue;
                                }
                            }
                            match method {
                                Some("Browser.attachedToTarget") => {
                                    // v0.6.2 SELF-HEALING SESSIONS: Juggler
                                    // recycles sessions (OAuth popups, process
                                    // swaps) and re-attaches with a NEW id —
                                    // claim ours or the handle keeps a dead id
                                    // ("cannot find session with id ...").
                                    if let Some(tid) = msg
                                        .pointer("/params/targetInfo/targetId")
                                        .or_else(|| msg.pointer("/params/targetId"))
                                        .and_then(|v| v.as_str())
                                    {
                                        if tid == handle2.target_id {
                                            if let Some(sid) = msg
                                                .pointer("/params/sessionId")
                                                .and_then(|v| v.as_str())
                                            {
                                                *handle2.session_id.lock().await =
                                                    Some(sid.to_string());
                                            }
                                            // Fresh attach = fresh world: any
                                            // previous crash flag is stale.
                                            handle2.crashed.store(false, Ordering::SeqCst);
                                        }
                                    }
                                }
                                Some("Browser.detachedFromTarget") => {
                                    if let Some(tid) =
                                        msg.pointer("/params/targetId").and_then(|v| v.as_str())
                                    {
                                        if tid == handle2.target_id {
                                            tracing::debug!(target: "ghostcloak::camoufox", "pump: full pin clear (detachedFromTarget)");
                                            *handle2.session_id.lock().await = None;
                                            *handle2.execution_context_id.lock().await = None;
                                            // Frame ids die with the session —
                                            // leaving them pinned made evaluate
                                            // fall back to a corpse forever.
                                            *handle2.frame_id.lock().await = None;
                                            *handle2.main_frame_id.lock().await = None;
                                        }
                                    }
                                }
                                Some("Inspector.targetCrashed") => {
                                    tracing::debug!(target: "ghostcloak::camoufox", "pump: full pin clear (targetCrashed)");
                                    handle2.crashed.store(true, Ordering::SeqCst);
                                    *handle2.execution_context_id.lock().await = None;
                                    *handle2.frame_id.lock().await = None;
                                    *handle2.main_frame_id.lock().await = None;
                                }
                                Some("Page.videoRecordingStarted") => {
                                    if let Some(f) =
                                        msg.pointer("/params/file").and_then(|v| v.as_str())
                                    {
                                        *handle2.video_file.lock().await = Some(f.to_string());
                                    }
                                    handle2.video_recording.store(true, Ordering::SeqCst);
                                }
                                Some("Page.videoRecordingFinished") => {
                                    handle2.video_recording.store(false, Ordering::SeqCst);
                                }
                                Some("Page.navigationCommitted") => {
                                    if let Some(fid) =
                                        msg.pointer("/params/frameId").and_then(|v| v.as_str())
                                    {
                                        if is_main_frame_id(fid) {
                                            *handle2.main_frame_id.lock().await =
                                                Some(fid.to_string());
                                            *handle2.frame_id.lock().await = Some(fid.to_string());
                                        }
                                    }
                                }
                                Some("Runtime.executionContextCreated") => {
                                    if let Some(cx) = msg
                                        .pointer("/params/executionContextId")
                                        .and_then(|v| v.as_str())
                                    {
                                        let fid = msg
                                            .pointer("/params/auxData/frameId")
                                            .and_then(|v| v.as_str())
                                            .map(str::to_string);
                                        let is_main =
                                            fid.as_deref().map(is_main_frame_id).unwrap_or(false);
                                        if is_main {
                                            *handle2.main_frame_id.lock().await = fid.clone();
                                            *handle2.execution_context_id.lock().await =
                                                Some(cx.to_string());
                                            if let Some(f) = fid {
                                                *handle2.frame_id.lock().await = Some(f);
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            });
        }
        Ok(handle)
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }
}

/// Locate the engine install: Ghostfox (preferred) with legacy Camoufox
/// fallback. Returns (install_dir, binary_name).
fn autodetect_engine() -> Option<(PathBuf, &'static str)> {
    let home = std::env::var("GHOSTFOX_HOME")
        .or_else(|_| std::env::var("CAMOUFOX_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
            let mut p = PathBuf::from(home);
            p.push(".cache");
            p.push("camoufox");
            p
        });
    for bin in ["ghostfox-bin", "camoufox-bin"] {
        if home.join(bin).exists() {
            return Some((home, bin));
        }
    }
    None
}

pub struct CamoufoxPage {
    conn: Arc<JugglerConnection>,
    target_id: String,
    /// Juggler session for this target; all Page/Runtime commands must be
    /// routed through it, not the root session.
    session_id: Mutex<Option<String>>,
    /// The page's MAIN frame. Only this frame's contexts and navigations are
    /// tracked for evaluate(); iframe contexts must never hijack them.
    main_frame_id: Mutex<Option<String>>,
    frame_id: Mutex<Option<String>>,
    execution_context_id: Mutex<Option<String>>,
    /// Renderer death observed on this target (Inspector.targetCrashed):
    /// evaluate() fails fast with an actionable message instead of spinning
    /// ~30s against a context that will never come back.
    crashed: AtomicBool,
    /// Native video recording (Browser.setVideoRecordingOptions on the
    /// default context): path of the .webm this target is writing, plus
    /// whether the recorder is live (Page.videoRecordingStarted/Finished).
    video_file: Mutex<Option<String>>,
    video_recording: AtomicBool,
}

/// Juggler frame ids: `mainframe-N` is a page's TOP frame, `subframe-N` an
/// iframe. Inside a session-filtered pump, ANY `mainframe-*` event belongs to
/// OUR top frame — even after a crash/tab-recreate mints a NEW `N`.
/// Equality against a previously pinned id wedges the handle forever once
/// that frame is replaced: evaluate() then keeps retrying the dead id and
/// juggler throws `Failed to find execution context with id = mainframe-N`.
fn is_main_frame_id(fid: &str) -> bool {
    match fid.strip_prefix("mainframe-") {
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

#[cfg(test)]
mod frame_id_tests {
    use super::is_main_frame_id;

    #[test]
    fn top_frame_ids_are_main() {
        assert!(is_main_frame_id("mainframe-11"));
        assert!(is_main_frame_id("mainframe-1"));
        assert!(is_main_frame_id("mainframe-1234567"));
    }

    #[test]
    fn iframe_and_junk_ids_never_match() {
        assert!(!is_main_frame_id("subframe-3"));
        assert!(!is_main_frame_id(""));
        assert!(!is_main_frame_id("mainframe-"));
        assert!(!is_main_frame_id("mainframex"));
        assert!(!is_main_frame_id("mainframe-11abc"));
        assert!(!is_main_frame_id("iframe-1"));
    }
}

#[cfg(test)]
mod same_document_tests {
    use super::same_document;

    #[test]
    fn fragment_moves_are_same_document() {
        assert!(same_document(
            "http://127.0.0.1:8899/ctxnav.html",
            "http://127.0.0.1:8899/ctxnav.html#tab2"
        ));
        assert!(same_document(
            "http://127.0.0.1:8899/ctxnav.html#tab1",
            "http://127.0.0.1:8899/ctxnav.html#tab2"
        ));
        assert!(same_document("file:///a/b.html#x", "file:///a/b.html"));
    }

    #[test]
    fn real_navigations_are_cross_document() {
        assert!(!same_document(
            "http://127.0.0.1:8899/ctxnav.html",
            "http://127.0.0.1:8899/ctxnav_a.html#top"
        ));
        assert!(!same_document(
            "http://127.0.0.1:8899/ctxnav.html?x=1",
            "http://127.0.0.1:8899/ctxnav.html?x=2"
        ));
        assert!(!same_document(
            "http://127.0.0.1:8899/a.html",
            "about:blank"
        ));
    }

    #[test]
    fn empty_or_unparseable_sides_are_conservative() {
        // Parse failures must fall through to the old reset path.
        assert!(!same_document("", "http://x/y.html"));
        assert!(!same_document("http://x/y.html", ""));
        assert!(!same_document("", ""));
    }
}

use tokio::sync::Mutex;

impl CamoufoxPage {
    fn new(conn: Arc<JugglerConnection>, target_id: String) -> Arc<Self> {
        Arc::new(Self {
            conn,
            target_id,
            session_id: Mutex::new(None),
            main_frame_id: Mutex::new(None),
            frame_id: Mutex::new(None),
            execution_context_id: Mutex::new(None),
            crashed: AtomicBool::new(false),
            video_file: Mutex::new(None),
            video_recording: AtomicBool::new(false),
        })
    }

    async fn session_id(&self) -> Result<String> {
        let guard = self.session_id.lock().await;
        guard
            .clone()
            .ok_or_else(|| GhostError::PageOp("target session not attached".into()))
    }

    async fn execution_context(&self) -> Result<String> {
        let guard = self.execution_context_id.lock().await;
        guard
            .clone()
            .ok_or_else(|| GhostError::PageOp("execution context not established".into()))
    }

    async fn snapshot_inner(&self) -> Result<PageSnapshot> {
        let url = self
            .evaluate("location.href")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string();
        let title = self
            .evaluate("document.title")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string();
        let body = self
            .evaluate("document.body ? document.body.innerText : ''")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string();
        Ok(PageSnapshot {
            url,
            title: Some(title),
            content: body,
            captured_at: chrono::Utc::now(),
        })
    }
}

#[async_trait]
impl Engine for CamoufoxEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Firefox
    }

    async fn new_page(
        &self,
        _opts: &HashMap<String, serde_json::Value>,
    ) -> Result<Arc<dyn PageHandle>> {
        // 1. Enable the browser-side dispatcher (required before anything
        //    else; attachToDefaultContext is mandatory, not optional).
        self.conn
            .request(
                "Browser.enable",
                serde_json::json!({ "attachToDefaultContext": true }),
            )
            .await?;

        // 2. Subscribe to events BEFORE newPage: the attachedToTarget event
        //    fires immediately when the page is created, and we'd otherwise
        //    race the broadcast and miss it.
        let mut events = self.conn.subscribe();

        // 3. Create the page in the default context.
        let result = self
            .conn
            .request("Browser.newPage", serde_json::json!({}))
            .await?;
        let target_id = result
            .get("targetId")
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                GhostError::Protocol(format!(
                    "Browser.newPage returned no targetId: {}",
                    serde_json::to_string(&result).unwrap_or_default()
                ))
            })?
            .to_string();

        let handle = CamoufoxPage::new(self.conn.clone(), target_id);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if std::time::Instant::now() > deadline {
                // No context event seen; fall back to empty ids — evaluate
                // against the main world by convention and let the caller
                // surface any protocol error.
                *handle.frame_id.lock().await = Some(String::new());
                *handle.execution_context_id.lock().await = Some(String::new());
                break;
            }
            if self.conn.is_closed() {
                return Err(GhostError::EngineCrashed);
            }
            match tokio::time::timeout(std::time::Duration::from_secs(2), events.recv()).await {
                Ok(Ok(msg)) => {
                    let method = msg.get("method").and_then(|m| m.as_str());
                    if method == Some("Browser.attachedToTarget") {
                        // targetInfo.targetId names the page this session
                        // belongs to — only claim our own.
                        if let Some(tid) = msg
                            .pointer("/params/targetInfo/targetId")
                            .or_else(|| msg.pointer("/params/targetId"))
                            .and_then(|v| v.as_str())
                        {
                            if tid == handle.target_id {
                                if let Some(sid) =
                                    msg.pointer("/params/sessionId").and_then(|v| v.as_str())
                                {
                                    *handle.session_id.lock().await = Some(sid.to_string());
                                }
                            }
                        }
                    }
                    if method == Some("Browser.detachedFromTarget") {
                        if let Some(tid) = msg.pointer("/params/targetId").and_then(|v| v.as_str())
                        {
                            if tid == handle.target_id {
                                *handle.session_id.lock().await = None;
                                *handle.execution_context_id.lock().await = None;
                            }
                        }
                    }
                    // Once attached, ignore every other page's events —
                    // they'd otherwise hijack our context id.
                    let my_sid = handle.session_id.lock().await.clone();
                    if let Some(evt_sid) = msg.get("sessionId").and_then(|s| s.as_str()) {
                        if my_sid.is_some() && Some(evt_sid) != my_sid.as_deref() {
                            continue;
                        }
                    }
                    if method == Some("Runtime.executionContextCreated") {
                        if let Some(cx) = msg
                            .pointer("/params/executionContextId")
                            .and_then(|v| v.as_str())
                        {
                            let fid = msg
                                .pointer("/params/auxData/frameId")
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                            // Prefix rule (NOT equality against the pin):
                            // a crash/tab-recreate mints a NEW mainframe-N —
                            // the old pin rejected the recovery event forever
                            // and evaluate() kept retrying the dead id.
                            let is_main = fid.as_deref().map(is_main_frame_id).unwrap_or(false);
                            if is_main {
                                *handle.main_frame_id.lock().await = fid.clone();
                                *handle.frame_id.lock().await = fid;
                                *handle.execution_context_id.lock().await = Some(cx.to_string());
                            }
                        }
                    }
                    if method == Some("Page.navigationCommitted") {
                        if let Some(fid) = msg.pointer("/params/frameId").and_then(|v| v.as_str()) {
                            // Top frame of OUR session committed a navigation
                            // (including a crash-recreate with a new id):
                            // re-pin immediately so evaluate()'s frame fallback
                            // never points at a dead frame.
                            if is_main_frame_id(fid) {
                                *handle.main_frame_id.lock().await = Some(fid.to_string());
                                *handle.frame_id.lock().await = Some(fid.to_string());
                            }
                        }
                    }
                    let sid = handle.session_id.lock().await.clone();
                    let cx = handle.execution_context_id.lock().await.clone();
                    if sid.is_some() && cx.is_some() {
                        break;
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                    *handle.frame_id.lock().await = Some(String::new());
                    *handle.execution_context_id.lock().await = Some(String::new());
                    break;
                }
                Err(_) => continue, // timeout, keep waiting until deadline
            }
        }

        // 4b. Give the page a real viewport: headless pages default to a
        // degenerate size and mouse-event dispatch needs actual bounds.
        {
            let sid = handle.session_id().await?;
            // Viewport follows the identity's screen class: a phone persona
            // must present a phone viewport, a desktop a desktop one.
            // (Height shaved slightly for browser chrome.)
            let (vw, vh) = match self.identity.platform {
                ghostcloak_fingerprint::identity::Platform::Android => (
                    self.identity.screen.width,
                    self.identity.screen.height.saturating_sub(80),
                ),
                _ => (1280, 800),
            };
            let _ = self
                .conn
                .request_session(
                    "Page.setViewportSize",
                    serde_json::json!({"viewportSize": {"width": vw, "height": vh}}),
                    Some(&sid),
                )
                .await;
            // v0.7 DEBUG CORTEX: console + exception events from page
            // birth — Juggler needs Page.runtimeEnable (not Runtime.enable;
            // that's the v0.6.3 blocker, decoded 2026-09-25).
            let _ = self
                .conn
                .request_session("Page.runtimeEnable", serde_json::json!({}), Some(&sid))
                .await;
        }

        // 4. Keep execution-context and frame ids live: contexts are
        //    recreated on every navigation, so a static snapshot goes stale.
        //    A background pump tracks the newest ids forever.
        {
            let conn = self.conn.clone();
            let handle2 = handle.clone();
            let mut rx = self.conn.subscribe();
            tokio::spawn(async move {
                // Every Juggler event is stamped with the sessionId of the
                // page that emitted it; only listen to ours. Without this,
                // another page's navigation (e.g. a consent screen click)
                // corrupts our context id — the cross-talk bug.
                loop {
                    match rx.recv().await {
                        Ok(msg) => {
                            let method = msg.get("method").and_then(|m| m.as_str());
                            // Root-session events (Browser.*) are broadcast;
                            // per-page events must match our session, read
                            // live so late attachments are covered too.
                            if let Some(evt_sid) = msg.get("sessionId").and_then(|s| s.as_str()) {
                                let my_sid = handle2.session_id.lock().await.clone();
                                if Some(evt_sid) != my_sid.as_deref() {
                                    continue;
                                }
                            }
                            match method {
                                Some("Runtime.executionContextCreated") => {
                                    if let Some(cx) = msg
                                        .pointer("/params/executionContextId")
                                        .and_then(|v| v.as_str())
                                    {
                                        // Only the MAIN frame's context is a
                                        // valid evaluate target; iframe srcdoc
                                        // contexts must not hijack it (e.g.
                                        // bot.sannysoft.com's trailing test
                                        // iframes). The id PREFIX decides —
                                        // NOT equality against the pinned id:
                                        // after a crash/tab-recreate the top
                                        // frame gets a NEW mainframe-N, and the
                                        // old pin rejected that recovery event
                                        // forever (evaluate kept retrying the
                                        // dead id: "Failed to find execution
                                        // context with id = mainframe-N").
                                        let fid = msg
                                            .pointer("/params/auxData/frameId")
                                            .and_then(|v| v.as_str())
                                            .map(str::to_string);
                                        let is_main =
                                            fid.as_deref().map(is_main_frame_id).unwrap_or(false);
                                        if is_main {
                                            *handle2.main_frame_id.lock().await = fid.clone();
                                            if let Some(f) = fid {
                                                *handle2.frame_id.lock().await = Some(f);
                                            }
                                            *handle2.execution_context_id.lock().await =
                                                Some(cx.to_string());
                                        }
                                    }
                                }
                                Some("Runtime.executionContextDestroyed") => {
                                    // If the context we hold just died, clear
                                    // it so evaluate waits for the successor.
                                    let dead = msg
                                        .pointer("/params/executionContextId")
                                        .and_then(|v| v.as_str());
                                    let mut guard = handle2.execution_context_id.lock().await;
                                    if guard.as_deref() == dead {
                                        tracing::debug!(target: "ghostcloak::camoufox", "pump: ctx pin cleared (executionContextDestroyed)");
                                        *guard = None;
                                    }
                                }
                                // Navigation wipes all contexts: drop the
                                // stale id so evaluate waits for the new one.
                                Some("Runtime.executionContextsCleared") => {
                                    tracing::debug!(target: "ghostcloak::camoufox", "pump: ctx pin cleared (executionContextsCleared)");
                                    *handle2.execution_context_id.lock().await = None;
                                }
                                _ => {}
                            }

                            // v0.7 DEBUG CORTEX (correct pump — this one lives
                            // forever): console/errors/network buffered from
                            // page birth, before the cross-talk filter? AFTER —
                            // events here already passed our-session matching.
                            // v0.7 DEBUG CORTEX (correct pump — this one lives forever):
                            // console/errors/network buffered from page birth.
                            if let Some(m) = method {
                                let buf = debug_buffers_for(&handle2.target_id);
                                match m {
                                    "Runtime.console" => {
                                        let kind = msg
                                            .pointer("/params/type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("log")
                                            .to_string();
                                        let mut parts: Vec<String> = Vec::new();
                                        if let Some(args) =
                                            msg.pointer("/params/args").and_then(|v| v.as_array())
                                        {
                                            for a in args.iter().take(4) {
                                                if let Some(v) = a.get("value") {
                                                    match v {
                                                        serde_json::Value::String(s2) => {
                                                            parts.push(s2.clone())
                                                        }
                                                        other => parts.push(other.to_string()),
                                                    }
                                                } else if let Some(t) =
                                                    a.get("type").and_then(|v| v.as_str())
                                                {
                                                    parts.push(format!("[{t}]"));
                                                }
                                            }
                                        }
                                        let entry = serde_json::json!({
                                            "kind": kind,
                                            "text": parts.join(" "),
                                            "ts": now_ms(),
                                        });
                                        let mut c = buf.console.lock().unwrap();
                                        c.push(entry);
                                        let keep_from = c.len().saturating_sub(500);
                                        if keep_from > 0 {
                                            c.drain(0..keep_from);
                                        }
                                    }
                                    "Page.uncaughtError" => {
                                        let text = msg
                                            .pointer("/params/message")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("exception")
                                            .to_string();
                                        let url = msg
                                            .pointer("/params/location/url")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let line = msg
                                            .pointer("/params/location/lineNumber")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0);
                                        let stack = msg
                                            .pointer("/params/stack")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let entry = serde_json::json!({
                                            "text": text,
                                            "url": url,
                                            "line": line,
                                            "stack": vec![stack],
                                            "ts": now_ms(),
                                        });
                                        let mut e = buf.errors.lock().unwrap();
                                        e.push(entry);
                                        let keep_from = e.len().saturating_sub(200);
                                        if keep_from > 0 {
                                            e.drain(0..keep_from);
                                        }
                                    }
                                    "Network.requestWillBeSent" => {
                                        let rid = msg
                                            .pointer("/params/requestId")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let url2 = net_url(&msg);
                                        let mth = net_method(&msg);
                                        if !url2.starts_with("data:") && !rid.is_empty() {
                                            let mut n = buf.net.lock().unwrap();
                                            let mut ix = buf.net_index.lock().unwrap();
                                            if !ix.contains_key(&rid) {
                                                ix.insert(rid.clone(), n.len());
                                                n.push(serde_json::json!({
                                                    "requestId": rid,
                                                    "url": url2,
                                                    "method": mth,
                                                    "status": serde_json::Value::Null,
                                                    "done": false,
                                                    "ts": now_ms(),
                                                }));
                                                if n.len() > 1000 {
                                                    let drop = n.len().saturating_sub(1000);
                                                    if drop > 0 {
                                                        n.drain(0..drop);
                                                    }
                                                    ix.clear();
                                                    for (i, e2) in n.iter().enumerate() {
                                                        if let Some(r) = e2
                                                            .get("requestId")
                                                            .and_then(|v| v.as_str())
                                                        {
                                                            ix.insert(r.to_string(), i);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    "Network.responseReceived" => {
                                        let rid = msg
                                            .pointer("/params/requestId")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let status = net_status(&msg);
                                        if let (Some(ix_val), Ok(mut n)) = (
                                            buf.net_index.lock().unwrap().get(&rid).copied(),
                                            buf.net.try_lock(),
                                        ) {
                                            if let Some(e2) = n.get_mut(ix_val) {
                                                e2["status"] = serde_json::json!(status);
                                                e2["done"] = serde_json::json!(true);
                                            }
                                        }
                                    }
                                    "Network.requestFinished" => {
                                        let rid = msg
                                            .pointer("/params/requestId")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        if let (Some(ix_val), Ok(mut n)) = (
                                            buf.net_index.lock().unwrap().get(&rid).copied(),
                                            buf.net.try_lock(),
                                        ) {
                                            if let Some(e2) = n.get_mut(ix_val) {
                                                e2["done"] = serde_json::json!(true);
                                            }
                                        }
                                    }
                                    "Network.requestFailed" => {
                                        let rid = msg
                                            .pointer("/params/requestId")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        if let (Some(ix_val), Ok(mut n)) = (
                                            buf.net_index.lock().unwrap().get(&rid).copied(),
                                            buf.net.try_lock(),
                                        ) {
                                            if let Some(e2) = n.get_mut(ix_val) {
                                                e2["status"] = serde_json::json!(0);
                                                e2["done"] = serde_json::json!(true);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if method == Some("Page.navigationCommitted") {
                                if let Some(fid) =
                                    msg.pointer("/params/frameId").and_then(|v| v.as_str())
                                {
                                    if is_main_frame_id(fid) {
                                        *handle2.main_frame_id.lock().await = Some(fid.to_string());
                                        *handle2.frame_id.lock().await = Some(fid.to_string());
                                    }
                                }
                            }
                            if method == Some("Browser.attachedToTarget") {
                                // v0.6.2 BUGFIX: scope the claim to OUR
                                // targetId. This used to claim ANY attach's
                                // sessionId — so when a site-opened popup
                                // (OAuth) attached, the main page STOLE the
                                // popup's session and died with it when the
                                // popup closed ("cannot find session with
                                // id ..." — the live TikTok OAuth case).
                                let tid = msg
                                    .pointer("/params/targetInfo/targetId")
                                    .or_else(|| msg.pointer("/params/targetId"))
                                    .and_then(|v| v.as_str());
                                if tid == Some(handle2.target_id.as_str()) {
                                    if let Some(sid) =
                                        msg.pointer("/params/sessionId").and_then(|v| v.as_str())
                                    {
                                        *handle2.session_id.lock().await = Some(sid.to_string());
                                    }
                                    handle2.crashed.store(false, Ordering::SeqCst);
                                }
                            }
                            if method == Some("Browser.detachedFromTarget") {
                                if let Some(tid) =
                                    msg.pointer("/params/targetId").and_then(|v| v.as_str())
                                {
                                    if tid == handle2.target_id.as_str() {
                                        tracing::debug!(target: "ghostcloak::camoufox", "pump: full pin clear (detachedFromTarget, live)");
                                        *handle2.session_id.lock().await = None;
                                        *handle2.execution_context_id.lock().await = None;
                                        *handle2.frame_id.lock().await = None;
                                        *handle2.main_frame_id.lock().await = None;
                                    }
                                }
                            }
                            if method == Some("Inspector.targetCrashed") {
                                tracing::debug!(target: "ghostcloak::camoufox", "pump: full pin clear (targetCrashed, live)");
                                handle2.crashed.store(true, Ordering::SeqCst);
                                *handle2.execution_context_id.lock().await = None;
                                *handle2.frame_id.lock().await = None;
                                *handle2.main_frame_id.lock().await = None;
                            }
                            if method == Some("Page.videoRecordingStarted") {
                                if let Some(f) =
                                    msg.pointer("/params/file").and_then(|v| v.as_str())
                                {
                                    *handle2.video_file.lock().await = Some(f.to_string());
                                }
                                handle2.video_recording.store(true, Ordering::SeqCst);
                            }
                            if method == Some("Page.videoRecordingFinished") {
                                handle2.video_recording.store(false, Ordering::SeqCst);
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!(target: "ghostcloak::camoufox", "pump: {n} events lost (broadcast lagged), ctx pin dropped");
                            // Event storm overflowed the broadcast buffer;
                            // lifecycle events were lost. Drop the cached
                            // context so evaluate waits for the next
                            // executionContextCreated instead of firing at
                            // an id that may already be dead.
                            *handle2.execution_context_id.lock().await = None;
                            continue;
                        }
                        Err(_) => break,
                    }
                    let _ = &conn;
                }
            });
        }

        // If we have no context yet, navigate to about:blank to force one.
        if handle.execution_context_id.lock().await.is_none() {
            let _ = handle.navigate("about:blank").await;
            // Wait for the fresh context event.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while handle.execution_context_id.lock().await.is_none() {
                if std::time::Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }

        Ok(handle)
    }

    async fn pages(&self) -> Result<Vec<String>> {
        let result = self
            .conn
            .request("Browser.targets", serde_json::json!({}))
            .await?;
        let targets = result
            .get("targets")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(targets
            .iter()
            .filter_map(|t| t.get("targetId").and_then(|i| i.as_str()))
            .map(|s| s.to_string())
            .collect())
    }

    async fn list_targets(&self) -> Result<Vec<(String, Option<String>)>> {
        // TWO DETECTION CHANNELS, MERGED — a popup must be visible from
        // at least one:
        //   A. Browser.targets query (this Juggler build lists only some
        //      targets — observed live missing an OAuth popup).
        //   B. The attachedToTarget event listener registry (push channel
        //      — every auto-attached target, incl. popups, lands here).
        let mut out: Vec<(String, Option<String>)> = vec![];
        let result = self
            .conn
            .request("Browser.targets", serde_json::json!({}))
            .await
            .unwrap_or(serde_json::Value::Null);
        if let Some(targets) = result.get("targets").and_then(|t| t.as_array()) {
            for t in targets {
                if let Some(id) = t.get("targetId").and_then(|i| i.as_str()) {
                    let url = t.get("url").and_then(|u| u.as_str());
                    out.push((id.to_string(), url.map(str::to_string)));
                }
            }
        }
        // Channel B: registry entries not already listed.
        let registry = self.all_targets().await;
        for (tid, _sid) in registry {
            if !out.iter().any(|(id, _)| id == &tid) {
                out.push((tid, None));
            }
        }
        Ok(out)
    }

    async fn attach_target(
        &self,
        target_id: &str,
    ) -> Result<Arc<dyn ghostcloak_core::engine::PageHandle>> {
        CamoufoxEngine::attach_target(self, target_id).await
    }

    async fn shutdown(&self) -> Result<()> {
        // Fire-and-forget close: Camoufox doesn't always send the
        // Browser.close response (known upstream quirk), so we don't wait.
        // kill() does the graceful SIGTERM dance.
        let conn = self.conn.clone();
        tokio::spawn(async move {
            let _ = conn.request("Browser.close", serde_json::json!({})).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        self.conn.kill().await;
        // Remove our ephemeral profile; leave user-provided dirs alone.
        if let Some(dir) = &self.ephemeral_profile {
            if std::fs::remove_dir_all(dir).is_err() {
                tracing::warn!(target: "ghostcloak::camoufox", "failed to clean profile {}", dir.display());
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// v0.6: Human mouse — engine-level hands.
// ---------------------------------------------------------------------------

impl CamoufoxPage {
    /// v0.6.3: INIT SCRIPTS — run agent code at DOCUMENT START, before
    /// any page script. The deepest interception layer (hooks captured
    /// by page libraries become OURS). Ghostfox's engine-level advantage.
    async fn add_init_script(&self, source: &str) -> Result<()> {
        let sid = self.session_id().await?;
        self.conn
            .request_session(
                "Page.setInitScripts",
                serde_json::json!({
                    "scripts": [ { "script": source } ]
                }),
                Some(&sid),
            )
            .await?;
        Ok(())
    }

    /// v0.6: Resolve a ref to its viewport center (scrolls into view).
    async fn ref_center(&self, r: &str) -> Result<(f64, f64)> {
        let out = self.evaluate(&crate::a11y::rect_ref_js(r)).await?;
        let s = out
            .as_str()
            .ok_or_else(|| GhostError::PageOp(format!("rect_ref({r}) bad response")))?;
        if s == "STALE-REF" {
            return Err(GhostError::PageOp(format!(
                "ref {r} is stale — rerun page_a11y"
            )));
        }
        let v: serde_json::Value = serde_json::from_str(s)
            .map_err(|e| GhostError::PageOp(format!("rect_ref({r}) parse: {e}")))?;
        Ok((
            v.get("x").and_then(|x| x.as_f64()).unwrap_or(0.0),
            v.get("y").and_then(|y| y.as_f64()).unwrap_or(0.0),
        ))
    }

    /// v0.6: Dispatch one mouse event at (x, y). `buttons` is the
    /// bitmask of held buttons (1 = left held — drag moves). NOTE: the
    /// Juggler protocol (Playwright-Firefox) uses lowercase event types —
    /// "mousemove", not CDP's "mouseMoved" (down/up match the existing
    /// click path).
    async fn dispatch_mouse(
        &self,
        ty: &str,
        x: f64,
        y: f64,
        buttons: u32,
        count: u32,
    ) -> Result<()> {
        let sid = self.session_id().await?;
        // DOM semantics: `buttons` is the state AFTER the event. On
        // mouseup the button is no longer held — sending buttons=1 there
        // makes pointer-capturing pages (GeeTest etc.) treat the release
        // as "still pressed" and the drag never completes ("Incomplete").
        let held = if ty == "mouseup" { 0 } else { buttons };
        let payload = match ty {
            "mousedown" | "mouseup" => serde_json::json!({
                "type": ty,
                "button": 0,
                "x": x,
                "y": y,
                "modifiers": 0,
                "clickCount": count,
                "buttons": held,
            }),
            _ => serde_json::json!({
                "type": "mousemove",
                "button": 0,
                "x": x,
                "y": y,
                "modifiers": 0,
                "buttons": held,
            }),
        };
        self.conn
            .request_session("Page.dispatchMouseEvent", payload, Some(&sid))
            .await?;
        Ok(())
    }

    /// v0.6: Human-like mouse path from -> to: cubic bezier with a random
    /// arc bulge, ease-in-out velocity, sub-pixel tremor and a small
    /// overshoot+correction at the end. Returns (x, y, delay_ms) triples.
    ///
    /// v0.6.1 CAPTCHA HARDENING — the timing model matters more than the
    /// geometry for behavioral captchas (GeeTest profiles the drag
    /// time-series): total duration scales with distance (~4.5-6.5ms/px,
    /// a 160px human drag takes 1-1.5s, not 300ms), velocity is phased
    /// (slow start, cruise, careful approach, landing dance), real
    /// pauses (60-180ms) punctuate the drag, and the hand drifts in y.
    fn human_path(from: (f64, f64), to: (f64, f64)) -> Vec<(f64, f64, u64)> {
        use rand::Rng;
        let mut rng = rand::rng();
        let (x0, y0) = from;
        let (x1, y1) = to;
        let dx = x1 - x0;
        let dy = y1 - y0;
        let dist = (dx * dx + dy * dy).sqrt();
        if dist < 1.5 {
            return vec![(x1, y1, 6)];
        }
        // Perpendicular bulge on a random side: humans don't move straight.
        let side: f64 = if rng.random_bool(0.5) { 1.0 } else { -1.0 };
        let (px, py) = (-dy / dist * side, dx / dist * side);
        let bulge = dist * rng.random_range(0.04..0.16);
        let c1 = (
            x0 + dx / 3.0 + px * bulge + rng.random_range(-6.0..6.0),
            y0 + dy / 3.0 + py * bulge + rng.random_range(-6.0..6.0),
        );
        let c2 = (
            x0 + 2.0 * dx / 3.0 + px * bulge * 0.4 + rng.random_range(-6.0..6.0),
            y0 + 2.0 * dy / 3.0 + py * bulge * 0.4 + rng.random_range(-6.0..6.0),
        );
        let steps = ((dist / 7.0).ceil() as i32).clamp(14, 60);
        // --- TIME MODEL: the drag must take human-long for its distance.
        let total_ms: f64 = dist * rng.random_range(4.5..6.5) + rng.random_range(250.0..450.0);
        // Velocity profile: v(t) ∝ smoothstep derivative (zero at ends,
        // peak mid) — per-step delay ∝ 1/v, normalized to total_ms.
        let mut raw = Vec::with_capacity(steps as usize);
        let mut raw_sum = 0.0;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let v = (t * (1.0 - t)).max(0.02) * 3.0; // ∝ smoothstep speed
                                                     // Approach phase (last 25%) gets extra care: slower.
            let v = if t > 0.75 { v * 0.45 } else { v };
            raw.push(1.0 / v);
            raw_sum += 1.0 / v;
        }
        // Y-drift: low-frequency wander, hands are never perfectly level.
        let drift_amp = rng.random_range(0.5..1.8);
        let drift_phase = rng.random_range(0.0..std::f64::consts::TAU);
        // Real pauses: 2-4 hold-still moments mid-drag.
        let mut pause_at = [false; 61];
        let n_pauses = rng.random_range(2..=4);
        for _ in 0..n_pauses {
            let idx = rng.random_range((steps / 4) as usize..steps as usize);
            pause_at[idx.min(60)] = true;
        }
        let mut pts = Vec::with_capacity(steps as usize + 4);
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let te = t * t * (3.0 - 2.0 * t);
            let u = 1.0 - te;
            let x = u * u * u * x0
                + 3.0 * u * u * te * c1.0
                + 3.0 * u * te * te * c2.0
                + te * te * te * x1;
            let wander = (drift_phase + t * std::f64::consts::PI).sin() * drift_amp;
            let y = u * u * u * y0
                + 3.0 * u * u * te * c1.1
                + 3.0 * u * te * te * c2.1
                + te * te * te * y1
                + wander * (dy / dist).abs().max(0.0); // only when horizontal-ish
            let jx = rng.random_range(-0.7..0.7);
            let jy = rng.random_range(-0.7..0.7);
            let mut d = raw[i as usize - 1] / raw_sum * total_ms;
            if pause_at[i as usize] {
                d += rng.random_range(60.0..180.0);
            }
            pts.push((x + jx, y + jy, d.max(3.0) as u64));
        }
        // Landing dance: small overshoot, SLOW correction (humans correct
        // deliberately), and 1-2 micro-nudges — the classic human finish.
        if dist > 40.0 {
            let over = rng.random_range(1.5..4.0);
            pts.push((
                x1 + dx / dist * over,
                y1 + dy / dist * over,
                rng.random_range(60.0..140.0) as u64,
            ));
            // micro-nudge back toward the target
            let nudge = over * rng.random_range(0.4..0.8);
            pts.push((
                x1 + dx / dist * (over - nudge),
                y1 + dy / dist * (over - nudge),
                rng.random_range(80.0..180.0) as u64,
            ));
            pts.push((x1, y1, rng.random_range(100.0..240.0) as u64));
        }
        pts
    }

    /// Move the mouse along a path, optionally with the button held
    /// (drag). The per-point delays from human_path set the rhythm.
    /// Response loss is TOLERATED (the event usually landed — same
    /// philosophy as click()); aborting mid-drag would leave the button
    /// stuck in a pressed state for the whole browser session.
    async fn move_along(&self, path: &[(f64, f64, u64)], pressed: bool) -> Result<()> {
        let mut consecutive_errors = 0u8;
        for &(x, y, d) in path {
            if self
                .dispatch_mouse("mousemove", x, y, if pressed { 1 } else { 0 }, 0)
                .await
                .is_err()
            {
                consecutive_errors += 1;
                tracing::warn!(target: "ghostcloak::camoufox", "mousemove response lost ({consecutive_errors}; continuing)");
                if consecutive_errors >= 3 {
                    return Err(GhostError::PageOp(
                        "mouse channel unresponsive — aborting path".into(),
                    ));
                }
            } else {
                consecutive_errors = 0;
            }
            tokio::time::sleep(std::time::Duration::from_millis(d)).await;
        }
        Ok(())
    }

    /// Best-effort button release at (x, y). Used for cleanup when a
    /// drag hits a protocol error — a stuck-pressed button wedges the
    /// whole Juggler session (all later mouse dispatches time out).
    async fn force_release(&self, x: f64, y: f64) {
        for _attempt in 0..3 {
            if self.dispatch_mouse("mouseup", x, y, 1, 1).await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        tracing::warn!(target: "ghostcloak::camoufox", "force_release: mouseup never confirmed after retries");
    }
}

// ---------------------------------------------------------------------------
// v0.6.3: PROTOCOL-LEVEL NETWORK CAPTURE — engine-wide registry.
// Responses live BELOW the page: no page JS can detect or patch this.
// ---------------------------------------------------------------------------

type NetLog = std::sync::Mutex<Vec<(String, String)>>;

static NET_REGISTRY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<NetLog>>>,
> = std::sync::OnceLock::new();

fn net_registry(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<NetLog>>> {
    NET_REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn net_log_for(target_id: &str) -> std::sync::Arc<NetLog> {
    let mut reg = net_registry().lock().unwrap();
    reg.entry(target_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
        .clone()
}

// ---------------------------------------------------------------------------
// v0.7 DEBUG CORTEX — console + errors + structured network, per target.
// All captured at the Juggler protocol level: invisible to page JS.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DebugBuffers {
    console: std::sync::Mutex<Vec<serde_json::Value>>,
    errors: std::sync::Mutex<Vec<serde_json::Value>>,
    net: std::sync::Mutex<Vec<serde_json::Value>>,
    net_index: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    listening: std::sync::atomic::AtomicBool,
}

impl DebugBuffers {
    fn new() -> Self {
        Self::default()
    }
}

static DEBUG_REGISTRY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Arc<DebugBuffers>>>,
> = std::sync::OnceLock::new();

fn debug_buffers_for(target_id: &str) -> Arc<DebugBuffers> {
    let reg =
        DEBUG_REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut reg = reg.lock().unwrap();
    reg.entry(target_id.to_string())
        .or_insert_with(|| Arc::new(DebugBuffers::new()))
        .clone()
}

/// Native JS dialog policy: what to do when alert/confirm/prompt opens.
#[derive(Debug)]
struct DialogPolicy {
    accept: bool,
    prompt_text: Option<String>,
}

impl Default for DialogPolicy {
    fn default() -> Self {
        // Default: accept everything, answer prompts with "" — dialogs must
        // never wedge the page (a bare alert() otherwise blocks every eval).
        Self {
            accept: true,
            prompt_text: Some(String::new()),
        }
    }
}

#[derive(Default)]
struct DialogState {
    log: std::sync::Mutex<Vec<serde_json::Value>>,
    listening: std::sync::atomic::AtomicBool,
    policy: std::sync::Mutex<DialogPolicy>,
}

static DIALOG_REGISTRY: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, Arc<DialogState>>>,
> = std::sync::OnceLock::new();

fn dialog_state_for(target_id: &str) -> Arc<DialogState> {
    let reg =
        DIALOG_REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut reg = reg.lock().unwrap();
    reg.entry(target_id.to_string())
        .or_insert_with(|| Arc::new(DialogState::default()))
        .clone()
}

/// Human epoch ms.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Wait (bounded) for a fresh execution context after a history move
/// (back/forward/reload) — callers hand control back to MCP tools that
/// fire evaluate immediately.
async fn settle_context(page: &CamoufoxPage) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while page.execution_context_id.lock().await.is_none() {
        if std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // Timeout with no successor context event (same-document history moves,
    // lost storm events): adopt the surviving frame id so evaluates keep a
    // resolvable target instead of starving on empty pins.
    if page.execution_context_id.lock().await.is_none() {
        if let Some(f) = page.frame_id.lock().await.clone() {
            tracing::debug!(target: "ghostcloak::camoufox", "settle_context: adopting frame {f} after wait timeout");
            *page.execution_context_id.lock().await = Some(f);
        }
    }
}

/// Same-document check for navigate: a fragment never leaves the document,
/// so compare everything before the first '#'. An empty side (parse failure)
/// conservatively reports "not same document" — the old reset path.
fn same_document(current: &str, target: &str) -> bool {
    let a = current.split('#').next().unwrap_or("");
    let b = target.split('#').next().unwrap_or("");
    !a.is_empty() && a == b
}

#[async_trait]

impl PageHandle for CamoufoxPage {
    fn target_id(&self) -> Option<String> {
        Some(self.target_id.clone())
    }

    async fn navigate(&self, url: &str) -> Result<()> {
        let sid = self.session_id().await?;
        let frame = self.frame_id.lock().await.clone().unwrap_or_default();
        // Same-document moves (hash/query-only) KEEP the live execution
        // context — juggler may emit context destruction with no successor
        // creation event, and pre-clearing here would strand every later
        // evaluate with empty pins (no event is coming to refill them).
        // Read location.href FIRST, while the pins are still valid.
        let same_doc = match self.evaluate("location.href").await {
            Ok(v) => same_document(v.as_str().unwrap_or_default(), url),
            Err(_) => false,
        };
        if same_doc {
            tracing::debug!(target: "ghostcloak::camoufox", "navigate same-doc fast path (pins kept): {url}");
        } else {
            // Navigating invalidates the execution context; drop it now so
            // any concurrent evaluate waits for the fresh one instead of
            // firing at a dead context id.
            *self.execution_context_id.lock().await = None;
        }
        // Some sites (redirect chains) let the navigate response go missing;
        // the navigation itself still proceeds. Bound the wait and treat a
        // timeout as fire-and-forget rather than an error.
        let nav = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.conn.request_session(
                "Page.navigate",
                serde_json::json!({ "frameId": frame, "url": url }),
                Some(&sid),
            ),
        )
        .await;
        let out = match nav {
            Ok(Ok(result)) => {
                let _ = result;
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(()), // response lost mid-redirect; navigation continues
        };
        if same_doc {
            // The document (and its context) survived — nothing to wait for.
            return out;
        }
        // Wait (bounded) for the new page's context before handing control
        // back — callers (MCP page_open) fire evaluate right after.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while self.execution_context_id.lock().await.is_none() {
            if std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        // Cross-document navigation whose successor context event never
        // arrived (lost in a storm): adopt the surviving frame id so
        // evaluates keep a resolvable target instead of starving.
        if self.execution_context_id.lock().await.is_none() {
            if let Some(f) = self.frame_id.lock().await.clone() {
                tracing::debug!(target: "ghostcloak::camoufox", "navigate: context event lost, adopting frame {f}");
                *self.execution_context_id.lock().await = Some(f);
            }
        }
        out
    }

    /// Go back one history entry via the engine's NATIVE protocol call —
    /// never `history.back()` in page JS, which wedges the content process.
    /// Returns whether a previous entry existed.
    async fn go_back(&self) -> Result<bool> {
        let sid = self.session_id().await?;
        let frame = self.frame_id.lock().await.clone().unwrap_or_default();
        // History move invalidates the execution context — drop it so any
        // concurrent evaluate waits for the successor.
        *self.execution_context_id.lock().await = None;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.conn.request_session(
                "Page.goBack",
                serde_json::json!({ "frameId": frame }),
                Some(&sid),
            ),
        )
        .await;
        let success = match out {
            Ok(Ok(v)) => v.get("success").and_then(|b| b.as_bool()).unwrap_or(true),
            Ok(Err(e)) => return Err(e),
            Err(_) => true, // response lost; navigation still proceeds
        };
        settle_context(self).await;
        Ok(success)
    }

    async fn go_forward(&self) -> Result<bool> {
        let sid = self.session_id().await?;
        let frame = self.frame_id.lock().await.clone().unwrap_or_default();
        *self.execution_context_id.lock().await = None;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.conn.request_session(
                "Page.goForward",
                serde_json::json!({ "frameId": frame }),
                Some(&sid),
            ),
        )
        .await;
        let success = match out {
            Ok(Ok(v)) => v.get("success").and_then(|b| b.as_bool()).unwrap_or(true),
            Ok(Err(e)) => return Err(e),
            Err(_) => true,
        };
        settle_context(self).await;
        Ok(success)
    }

    async fn reload_page(&self) -> Result<()> {
        let sid = self.session_id().await?;
        *self.execution_context_id.lock().await = None;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.conn
                .request_session("Page.reload", serde_json::json!({}), Some(&sid)),
        )
        .await;
        match out {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {} // response lost; reload still proceeds
        }
        settle_context(self).await;
        Ok(())
    }

    async fn snapshot(&self) -> Result<PageSnapshot> {
        // Snapshot must self-heal: a mid-browse context loss (challenge
        // pages like Google /sorry kill the content channel) shouldn't
        // wedge the session. One retry via re-navigation to the current URL.
        for attempt in 0..2 {
            match self.snapshot_inner().await {
                Ok(s) => return Ok(s),
                Err(e) if attempt == 0 => {
                    tracing::debug!(target: "ghostcloak::camoufox", "snapshot failed ({e}); retrying via reload");
                    // Reload: recover context by re-navigating to the same URL.
                    if let Ok(u) = self.url().await {
                        if !u.is_empty() {
                            let _ = self.navigate(&u).await;
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }

    async fn click(&self, selector: &str) -> Result<()> {
        // Strategy 0: JS click for form controls. Split into two evaluates:
        // (1) locate the element — this response MUST come back;
        // (2) click it — if the click submits a form, the page starts
        //     navigating and the response can legitimately vanish mid-swap.
        // A vanished step-2 response after a located element = success.
        let locate = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return 'MISSING'; \
             const tag = el.tagName.toLowerCase(); \
             const isForm = tag === 'input' || tag === 'button' || el.closest('form') !== null; \
             return isForm ? 'FORM' : 'SKIP'; }})()",
            sel = serde_json::to_string(selector).unwrap_or_default()
        );
        let res = self.evaluate(&locate).await?;
        match res.as_str() {
            Some("FORM") => {
                let act = format!(
                    "(() => {{ document.querySelector({sel}).click(); return 'OK'; }})()",
                    sel = serde_json::to_string(selector).unwrap_or_default()
                );
                match self.evaluate(&act).await {
                    // Response made it back before the page swap.
                    Ok(_) => return Ok(()),
                    // Click fired a navigation that ate the response — the
                    // click itself landed. Same class as the missing
                    // Browser.close response quirk.
                    Err(e) => {
                        tracing::debug!(target: "ghostcloak::camoufox", "click response lost in nav (treated as success): {e}");
                        return Ok(());
                    }
                }
            }
            Some("SKIP") => { /* fall through to mouse events */ }
            _ => return Err(GhostError::PageOp("selector not found".into())),
        }

        // Strategy 1: real mouse events (best signal for anti-bot). If the
        // page's content channel dies mid-click (navigation), fall through
        // to strategy 2.
        let expr = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return null; \
             el.scrollIntoView({{block: 'center'}}); \
             const r = el.getBoundingClientRect(); \
             return JSON.stringify({{x: r.x + r.width/2, y: r.y + r.height/2, vw: window.innerWidth, vh: window.innerHeight}}); }})()",
            sel = serde_json::to_string(selector).unwrap_or_default()
        );
        let pos = self.evaluate(&expr).await;
        let pos: Option<(f64, f64)> = match pos {
            Ok(v) => v.as_str().and_then(|s| {
                serde_json::from_str::<serde_json::Value>(s)
                    .ok()
                    .and_then(|o| Some((o.get("x")?.as_f64()?, o.get("y")?.as_f64()?)))
            }),
            // Channel death during the coordinate probe = page transition:
            // jump straight to the JS-click fallback.
            Err(_) => None,
        };

        if let Some((x, y)) = pos {
            let mut dispatched = true;
            for ty in ["mousedown", "mouseup"] {
                let sid = {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    loop {
                        match self.session_id().await {
                            Ok(s) => break s,
                            Err(_) if std::time::Instant::now() < deadline => {
                                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                };
                match self
                    .conn
                    .request_session(
                        "Page.dispatchMouseEvent",
                        serde_json::json!({
                            "type": ty,
                            "button": 0,
                            "x": x,
                            "y": y,
                            "modifiers": 0,
                            "clickCount": 1,
                            "buttons": 1,
                        }),
                        Some(&sid),
                    )
                    .await
                {
                    Ok(_) => {}
                    // Click-triggered navigation can kill the channel mid-
                    // response; the click itself landed.
                    Err(_) if ty == "mouseup" => break,
                    Err(_) => {
                        dispatched = false;
                        break;
                    }
                }
            }
            if dispatched {
                return Ok(());
            }
        }

        // Strategy 2: JS click — works through navigation churn and on
        // elements that hide from mouse-event hit testing.
        let js_click = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return 'MISSING'; el.click(); return 'OK'; }})()",
            sel = serde_json::to_string(selector).unwrap_or_default()
        );
        let res = self.evaluate(&js_click).await?;
        match res.as_str() {
            Some("OK") => Ok(()),
            _ => Err(GhostError::PageOp("selector not found".into())),
        }
    }

    async fn click_variant(&self, selector: &str, button: u8, count: u8) -> Result<()> {
        // Plain left single-click: the battle-tested click() path.
        if button == 0 && count <= 1 {
            return self.click(selector).await;
        }
        // Locate first — form controls get DOM events (mouse dispatch into
        // form fields has been observed to kill this build's content
        // channel), everything else gets REAL engine mouse events
        // (isTrusted — what anti-bot gates expect).
        let locate = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return 'MISSING'; \
             const tag = el.tagName.toLowerCase(); \
             const isForm = tag === 'input' || tag === 'button' || el.closest('form') !== null; \
             return isForm ? 'FORM' : 'SKIP'; }})()",
            sel = serde_json::to_string(selector).unwrap_or_default()
        );
        let mode = match self.evaluate(&locate).await {
            Ok(v) => v.as_str().unwrap_or("MISSING").to_string(),
            // Channel death during the probe — the DOM path is the safe one.
            Err(_) => "FORM".to_string(),
        };
        if mode == "MISSING" {
            return Err(GhostError::PageOp("selector not found".into()));
        }
        if mode == "SKIP" {
            // Same coordinate probe as click(): scroll into view, rect center.
            let expr = format!(
                "(() => {{ const el = document.querySelector({sel}); if (!el) return null; \
                 el.scrollIntoView({{block: 'center'}}); \
                 const r = el.getBoundingClientRect(); \
                 return JSON.stringify({{x: r.x + r.width/2, y: r.y + r.height/2}}); }})()",
                sel = serde_json::to_string(selector).unwrap_or_default()
            );
            let pos: Option<(f64, f64)> = match self.evaluate(&expr).await {
                Ok(v) => v.as_str().and_then(|s| {
                    serde_json::from_str::<serde_json::Value>(s)
                        .ok()
                        .and_then(|o| Some((o.get("x")?.as_f64()?, o.get("y")?.as_f64()?)))
                }),
                Err(_) => None,
            };
            if let Some((x, y)) = pos {
                let sid = {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    loop {
                        match self.session_id().await {
                            Ok(s) => break s,
                            Err(_) if std::time::Instant::now() < deadline => {
                                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                };
                // (type, button, buttons, clickCount). DOM semantics:
                // `buttons` is the state AFTER the event (0 on mouseup) —
                // same rule dispatch_mouse documents for pointer capture.
                let seq: &[(&str, u32, u32, u32)] = if button == 2 {
                    &[("mousedown", 2, 2, 1), ("mouseup", 2, 0, 1)]
                } else if count == 2 {
                    &[
                        ("mousedown", 0, 1, 1),
                        ("mouseup", 0, 0, 1),
                        ("mousedown", 0, 1, 2),
                        ("mouseup", 0, 0, 2),
                    ]
                } else {
                    &[("mousedown", 0, 1, 1), ("mouseup", 0, 0, 1)]
                };
                let mut dispatched = true;
                for (ty, b, buttons, cc) in seq {
                    match self
                        .conn
                        .request_session(
                            "Page.dispatchMouseEvent",
                            serde_json::json!({
                                "type": *ty,
                                "button": *b,
                                "x": x,
                                "y": y,
                                "modifiers": 0,
                                "clickCount": *cc,
                                "buttons": *buttons,
                            }),
                            Some(&sid),
                        )
                        .await
                    {
                        Ok(_) => {
                            // Human pause between the two clicks of a double
                            // click (~90ms — under the OS double-click threshold).
                            if count == 2 && button == 0 && *ty == "mouseup" && *cc == 1 {
                                tokio::time::sleep(std::time::Duration::from_millis(90)).await;
                            }
                        }
                        // A mouseup response can legitimately vanish when the
                        // event triggers navigation — it landed.
                        Err(_) if *ty == "mouseup" => break,
                        Err(_) => {
                            dispatched = false;
                            break;
                        }
                    }
                }
                if dispatched {
                    return Ok(());
                }
            }
        }
        // DOM fallback: form controls, or mouse dispatch failed mid-churn.
        let res = self
            .evaluate(&dom_click_variant_js(selector, button, count))
            .await?;
        match res.as_str() {
            Some("OK") => Ok(()),
            _ => Err(GhostError::PageOp("selector not found".into())),
        }
    }

    async fn type_text(&self, selector: &str, text: &str) -> Result<()> {
        // Focus via JS, not a mouse click: dispatching mouse events into
        // form fields has been observed to kill this Camoufox build's
        // content channel on some pages. (Mouse clicks stay available via
        // `click()` for links and buttons.)
        let focus = format!(
            "(() => {{ const el = document.querySelector({sel}); if (!el) return 'MISSING'; el.focus(); return 'OK'; }})()",
            sel = serde_json::to_string(selector).unwrap_or_default()
        );
        let res = self.evaluate(&focus).await?;
        if res.as_str() != Some("OK") {
            return Err(GhostError::PageOp("selector not found".into()));
        }
        let sid = self.session_id().await?;
        // Type per-character via key events — what a human produces.
        // v0.5.3: HUMANIZED CADENCE — random inter-key delays with pauses at
        // spaces/newlines and occasional "thinking" pauses. Machine-gun
        // typing (sub-ms between chars) is a bot detection signal on sites
        // that profile keystroke dynamics (X, Reddit).
        use rand::Rng;
        let key = |c: char| -> (u32, String, String) {
            // (keyCode, code, key) for printable ASCII.
            let code = format!("Key{}", c.to_ascii_uppercase());
            (c.to_ascii_uppercase() as u32, code, c.to_string())
        };
        for ch in text.chars() {
            let spec = match ch {
                '\n' => Some(("Enter".to_string(), 13u32, "Enter".to_string())),
                '\r' => continue, // normalize CRLF: \n carries the Enter
                '\t' => Some(("Tab".to_string(), 9u32, "Tab".to_string())),
                ' ' => Some((" ".to_string(), 32u32, "Space".to_string())),
                _ => {
                    let (kc, code, k) = key(ch);
                    Some((k, kc, code))
                }
            };
            let Some((k, kc, code)) = spec else { continue };
            // Printable non-alphanumeric chars (punctuation etc.) don't
            // insert via plain key events in this engine build. Juggler's
            // dispatchKeyEvent only inserts text through a keydown whose
            // `text` differs from `key` (commitCompositionWith), and it
            // THROWS on CDP-style types ('keyDown'/'char'/'keyUp' — the
            // protocol wants lowercase 'keydown'/'keyup'). Those throws
            // used to be silently swallowed (`let _ =`) and punctuation
            // vanished. Route punctuation through `Page.insertText`
            // (IME-like commit at the caret); the keydown/keyup above
            // still fire for keystroke-dynamics realism.
            //
            // Space/Enter/Tab MUST stay out of this branch: plain
            // keydown already inserts them, and inserting again doubled
            // every space ("aa  bb").
            let needs_text_insert =
                k != " " && k != "\n" && k != "\t" && !k.chars().all(|c| c.is_ascii_alphanumeric());
            for ty in ["keydown", "keyup"] {
                let _ = self
                    .conn
                    .request_session(
                        "Page.dispatchKeyEvent",
                        serde_json::json!({
                            "type": ty,
                            "key": k,
                            "keyCode": kc,
                            "location": 0,
                            "code": code,
                            "repeat": false,
                        }),
                        Some(&sid),
                    )
                    .await;
            }
            if needs_text_insert {
                self.conn
                    .request_session(
                        "Page.insertText",
                        serde_json::json!({ "text": k }),
                        Some(&sid),
                    )
                    .await
                    .map_err(|e| GhostError::PageOp(format!("insertText({k:?}) failed: {e}")))?;
            }
            // Humanized cadence between keystrokes:
            //  - base 45-110ms per char (average typist)
            //  - pause at spaces (+30-80ms), longer at newlines (+120-320ms)
            //  - 5% "thinking" pause (+150-450ms)
            // (RNG scoped here — ThreadRng is !Send, must not live across awaits.)
            let delay = {
                let mut rng = rand::rng();
                let mut d = 45 + rng.random_range(0..65u64);
                if ch == ' ' {
                    d += 30 + rng.random_range(0..50);
                } else if ch == '\n' {
                    d += 120 + rng.random_range(0..200);
                }
                if rng.random_bool(0.05) {
                    d += 150 + rng.random_range(0..300);
                }
                d
            };
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        Ok(())
    }

    /// Press a named key (Enter, Tab, Escape, ArrowDown, ...) in the page.
    async fn press_key(&self, key_name: &str) -> Result<()> {
        let sid = self.session_id().await?;
        // Common named keys; anything else passes through as typed.
        let (code, keyc): (&str, u32) = match key_name {
            "Enter" => ("Enter", 13),
            "Tab" => ("Tab", 9),
            "Escape" => ("Escape", 27),
            "Backspace" => ("Backspace", 8),
            "Delete" => ("Delete", 46),
            "ArrowUp" => ("ArrowUp", 38),
            "ArrowDown" => ("ArrowDown", 40),
            "ArrowLeft" => ("ArrowLeft", 37),
            "ArrowRight" => ("ArrowRight", 39),
            "Home" => ("Home", 36),
            "End" => ("End", 35),
            "PageUp" => ("PageUp", 33),
            "PageDown" => ("PageDown", 34),
            _ => ("KeyUnknown", 0),
        };
        for ty in ["keydown", "keyup"] {
            self.conn
                .request_session(
                    "Page.dispatchKeyEvent",
                    serde_json::json!({
                        "type": ty,
                        "key": key_name,
                        "keyCode": keyc,
                        "location": 0,
                        "code": code,
                        "repeat": false,
                    }),
                    Some(&sid),
                )
                .await?;
        }
        Ok(())
    }

    async fn a11y_snapshot(&self) -> Result<ghostcloak_core::engine::A11ySnapshot> {
        let raw = self.evaluate(crate::a11y::WALK_JS).await?;
        let json: String = raw
            .as_str()
            .ok_or_else(|| GhostError::PageOp("a11y walk returned no data".into()))?
            .to_string();
        let snap: ghostcloak_core::engine::A11ySnapshot = serde_json::from_str(&json)
            .map_err(|e| GhostError::PageOp(format!("a11y parse: {e}")))?;
        Ok(snap)
    }

    async fn read_ref_full(&self, r: &str) -> Result<String> {
        let out = self.evaluate(&crate::a11y::read_ref_full_js(r)).await?;
        let s = out.as_str().unwrap_or_default();
        if s == "STALE-REF" {
            return Err(GhostError::PageOp(format!(
                "ref {r} is stale — rerun page_a11y"
            )));
        }
        Ok(s.to_string())
    }

    async fn wait_for(&self, selector: &str, timeout_ms: u64) -> Result<bool> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let expr = crate::a11y::wait_for_js(selector);
        while std::time::Instant::now() < deadline {
            match self.evaluate(&expr).await {
                Ok(v) if v.as_str() == Some("VISIBLE") => return Ok(true),
                _ => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            }
        }
        Ok(false)
    }

    async fn click_ref(&self, r: &str) -> Result<()> {
        let out = self.evaluate(&crate::a11y::click_ref_js(r)).await?;
        match out.as_str() {
            Some("CLICKED") => Ok(()),
            Some("STALE-REF") => Err(GhostError::PageOp(format!(
                "ref {r} is stale — rerun page_a11y"
            ))),
            _ => Err(GhostError::PageOp(format!(
                "click_ref({r}) unexpected result"
            ))),
        }
    }
    async fn mouse_move_to(&self, r: &str) -> Result<()> {
        let (x, y) = self.ref_center(r).await?;
        // Approach from a small random offset — a hand comes from somewhere,
        // it never spawns on the target. (RNG scoped: ThreadRng is !Send.)
        let start = {
            use rand::Rng;
            let mut rng = rand::rng();
            (
                x + rng.random_range(-90.0..-25.0),
                y + rng.random_range(-70.0..-20.0),
            )
        };
        let path = Self::human_path(start, (x, y));
        self.move_along(&path, false).await
    }

    async fn drag_ref(&self, from: &str, to: &str, dx: f64, dy: f64) -> Result<()> {
        // Initial source measure: only to aim the approach path. The exact
        // press point is re-resolved right before mousedown (layout shifts).
        let (sx, sy) = self.ref_center(from).await?;
        // (RNG scoped per use — ThreadRng is !Send, must not live across awaits.)
        let approach = {
            use rand::Rng;
            let mut rng = rand::rng();
            Self::human_path(
                (
                    sx + rng.random_range(-90.0..-25.0),
                    sy + rng.random_range(-70.0..-20.0),
                ),
                (sx, sy),
            )
        };
        // 1. Approach the source (hover first — hands grab, they don't teleport).
        self.move_along(&approach, false).await?;
        // 1b. RE-RESOLVE the source right before pressing: layout can shift
        // during the approach (settling panels, banners, scroll anchoring) —
        // pressing stale coordinates misses the target element and the page's
        // drag handler never engages (observed: press landing on the parent
        // panel instead of the slider button).
        let (sx, sy) = self.ref_center(from).await?;
        let (tx, ty) = if to.is_empty() {
            (sx + dx, sy + dy)
        } else {
            self.ref_center(to).await?
        };
        tracing::info!(target: "ghostcloak::camoufox",
            "drag_ref: from={from} to={to:?} press=({sx:.1},{sy:.1}) release=({tx:.1},{ty:.1}) dx_param={dx:.1}");
        // 2. Press. Small human grab-pause before moving. If the press
        // response is lost, keep going — aborting here would wedge the
        // session with a stuck button. If it truly didn't land, the
        // drag just won't take (safe, recoverable on the page).
        if self
            .dispatch_mouse("mousedown", sx, sy, 1, 1)
            .await
            .is_err()
        {
            tracing::warn!(target: "ghostcloak::camoufox", "mousedown response lost (continuing drag)");
        }
        {
            use rand::Rng;
            let pause: u64 = rand::rng().random_range(40..120);
            tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
        }
        // 3. Drag along a human path with the button held. On any
        // failure, release the button before returning the error so
        // the session stays usable.
        let path = Self::human_path((sx, sy), (tx, ty));
        if self.move_along(&path, true).await.is_err() {
            self.force_release(tx, ty).await;
            return Err(GhostError::PageOp(
                "drag path interrupted — button released, page intact".into(),
            ));
        }
        // 4. Settle on the target before letting go (humans verify the drop).
        {
            use rand::Rng;
            let settle: u64 = rand::rng().random_range(150..450);
            tokio::time::sleep(std::time::Duration::from_millis(settle)).await;
        }
        // 5. Release. Tolerate response loss here too — the release
        // usually lands, and an error return would mislead the agent.
        if self.dispatch_mouse("mouseup", tx, ty, 1, 1).await.is_err() {
            tracing::warn!(target: "ghostcloak::camoufox", "mouseup response lost (treated as released)");
        }
        Ok(())
    }

    /// Trait-facing safety release (PageHandle::release_mouse_at): reuses
    /// the retrying force_release so a drag future cancelled by an
    /// external timeout cannot leave the Juggler session button-stuck.
    async fn release_mouse_at(&self, x: f64, y: f64) -> Result<()> {
        self.force_release(x, y).await;
        Ok(())
    }

    async fn add_init_script(&self, source: &str) -> Result<()> {
        CamoufoxPage::add_init_script(self, source).await
    }

    /// v0.7 DEBUG CORTEX: start protocol-level capture. Enables the
    /// Juggler Network domain (passive observation — the v0.6.3 bug was
    /// using Network.setRequestInterception, which HOLDS requests and
    /// never emitted events without Network.enable) and spawns one
    /// listener that buffers console messages, uncaught exceptions, and
    /// structured network entries for this page.
    async fn net_capture_start(&self) -> Result<()> {
        let sid = self.session_id().await?;
        let tid = self.target_id.clone();
        let buf = debug_buffers_for(&tid);

        // Enable the debug domains — the missing calls that kept events
        // from ever flowing. Network.enable for request/response events;
        // Runtime.enable for consoleAPICalled + exceptionThrown (context
        // events flow on attach, but console/error events need the domain).
        // JUGGLER PROTOCOL (not CDP!): console + exception events come via
        // Page.runtimeEnable (the v0.6.3 blocker: Runtime.enable does not
        // exist in Juggler). Network observation requires interception +
        // immediate auto-resume — Juggler has no passive Network.enable.
        if let Err(e) = self
            .conn
            .request_session("Page.runtimeEnable", serde_json::json!({}), Some(&sid))
            .await
        {
            tracing::warn!(target: "ghostcloak::netcap", "Page.runtimeEnable failed: {e}");
        }

        // One listener per page (flag-guarded).
        if buf
            .listening
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        let mut rx = self.conn.subscribe();
        let conn = self.conn.clone();
        let buf2 = buf.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        let evt_sid = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
                        if evt_sid != sid {
                            continue;
                        }
                        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        if method.starts_with("Network.")
                            || method.starts_with("Runtime.console")
                            || method.starts_with("Runtime.exception")
                        {
                            tracing::info!(target: "ghostcloak::netcap", "evt: {method}");
                        }
                        match method {
                            "Runtime.consoleAPICalled" => {
                                let kind = msg
                                    .pointer("/params/type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("log")
                                    .to_string();
                                let mut parts: Vec<String> = Vec::new();
                                if let Some(args) =
                                    msg.pointer("/params/args").and_then(|v| v.as_array())
                                {
                                    for a in args.iter().take(4) {
                                        if let Some(v) = a.get("value") {
                                            match v {
                                                serde_json::Value::String(s2) => {
                                                    parts.push(s2.clone())
                                                }
                                                other => parts.push(other.to_string()),
                                            }
                                        } else if let Some(t) =
                                            a.get("type").and_then(|v| v.as_str())
                                        {
                                            parts.push(format!("[{t}]"));
                                        }
                                    }
                                }
                                let (url, line) = msg
                                    .pointer("/params/stackTrace/0/url")
                                    .and_then(|v| v.as_str())
                                    .map(|u| {
                                        (
                                            u.to_string(),
                                            msg.pointer("/params/stackTrace/0/lineNumber")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0),
                                        )
                                    })
                                    .unwrap_or_else(|| (String::new(), 0));
                                let entry = serde_json::json!({
                                    "kind": kind,
                                    "text": parts.join(" "),
                                    "url": url,
                                    "line": line,
                                    "ts": now_ms(),
                                });
                                let mut c = buf2.console.lock().unwrap();
                                c.push(entry);
                                if c.len() > 500 {
                                    let keep_from = c.len().saturating_sub(500);
                                    c.drain(0..keep_from);
                                }
                            }
                            "Runtime.exceptionThrown" => {
                                let text = msg
                                    .pointer("/params/message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("exception")
                                    .to_string();
                                let url = msg
                                    .pointer("/params/location/url")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let line = msg
                                    .pointer("/params/location/lineNumber")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let mut stack: Vec<String> = Vec::new();
                                if let Some(st) =
                                    msg.pointer("/params/stack").and_then(|v| v.as_str())
                                {
                                    stack.push(st.to_string());
                                }
                                let entry = serde_json::json!({
                                    "text": text,
                                    "url": url,
                                    "line": line,
                                    "stack": stack,
                                    "ts": now_ms(),
                                });
                                let mut e = buf2.errors.lock().unwrap();
                                e.push(entry);
                                if e.len() > 200 {
                                    let keep_from = e.len().saturating_sub(200);
                                    e.drain(0..keep_from);
                                }
                            }
                            "Network.requestWillBeSent" => {
                                let rid = msg
                                    .pointer("/params/requestId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let url = net_url(&msg);
                                let mth = net_method(&msg);
                                if url.starts_with("data:") {
                                    continue;
                                }
                                let entry = serde_json::json!({
                                    "requestId": rid,
                                    "url": url,
                                    "method": mth,
                                    "status": serde_json::Value::Null,
                                    "done": false,
                                    "ts": now_ms(),
                                });
                                let mut n = buf2.net.lock().unwrap();
                                let mut ix = buf2.net_index.lock().unwrap();
                                if !ix.contains_key(&rid) {
                                    ix.insert(rid, n.len());
                                    n.push(entry);
                                    if n.len() > 1000 {
                                        let drop = n.len() - 1000;
                                        n.drain(0..drop);
                                        // index now stale for old entries — clear + rebuild lazily
                                        ix.clear();
                                        for (i, e2) in n.iter().enumerate() {
                                            if let Some(r) =
                                                e2.get("requestId").and_then(|v| v.as_str())
                                            {
                                                ix.insert(r.to_string(), i);
                                            }
                                        }
                                    }
                                }
                            }
                            "Network.responseReceived" => {
                                let rid = msg
                                    .pointer("/params/requestId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let status = net_status(&msg);
                                if let (Some(ix_val), Ok(mut n)) = (
                                    buf2.net_index.lock().unwrap().get(&rid).copied(),
                                    buf2.net.try_lock(),
                                ) {
                                    if let Some(e2) = n.get_mut(ix_val) {
                                        e2["status"] = serde_json::json!(status);
                                        e2["done"] = serde_json::json!(true);
                                    }
                                }
                            }
                            // Juggler emits `requestFailed` (not Chrome's
                            // `loadingFailed`); handle both shapes.
                            "Network.loadingFailed" | "Network.requestFailed" => {
                                let rid = msg
                                    .pointer("/params/requestId")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let err = msg
                                    .pointer("/params/errorCode")
                                    .or_else(|| msg.pointer("/params/errorText"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if let (Some(ix_val), Ok(mut n)) = (
                                    buf2.net_index.lock().unwrap().get(&rid).copied(),
                                    buf2.net.try_lock(),
                                ) {
                                    if let Some(e2) = n.get_mut(ix_val) {
                                        e2["status"] = serde_json::json!(0);
                                        e2["done"] = serde_json::json!(true);
                                        e2["error"] = serde_json::json!(err);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        let _ = conn;
        Ok(())
    }

    /// Buffered console messages, protocol-captured (page can't hide them).
    async fn console_read(&self, clear: bool) -> Result<Vec<serde_json::Value>> {
        let tid = self.target_id.clone();
        self.net_capture_start().await?;
        let buf = debug_buffers_for(&tid);
        let out = {
            let mut c = buf.console.lock().unwrap();
            let out = c.clone();
            if clear {
                c.clear();
            }
            out
        };
        Ok(out)
    }

    /// Buffered uncaught exceptions with stack traces.
    async fn errors_read(&self, clear: bool) -> Result<Vec<serde_json::Value>> {
        let tid = self.target_id.clone();
        self.net_capture_start().await?;
        let buf = debug_buffers_for(&tid);
        let out = {
            let mut e = buf.errors.lock().unwrap();
            let out = e.clone();
            if clear {
                e.clear();
            }
            out
        };
        Ok(out)
    }

    /// Structured network entries (passive, Network.enable — never interception).
    async fn net_read(&self, clear: bool) -> Result<Vec<serde_json::Value>> {
        let tid = self.target_id.clone();
        self.net_capture_start().await?;
        let buf = debug_buffers_for(&tid);
        let out = {
            let mut n = buf.net.lock().unwrap();
            let out = n.clone();
            if clear {
                n.clear();
                buf.net_index.lock().unwrap().clear();
            }
            out
        };
        Ok(out)
    }

    async fn net_capture_list(&self) -> Result<Vec<(String, String)>> {
        Ok(net_log_for(&self.target_id).lock().unwrap().clone())
    }

    async fn net_get_body(&self, request_id: &str) -> Result<String> {
        let sid = self.session_id().await?;
        let result = self
            .conn
            .request_session(
                "Network.getResponseBody",
                serde_json::json!({ "requestId": request_id }),
                Some(&sid),
            )
            .await?;
        if let Some(b64) = result.get("base64body").and_then(|v| v.as_str()) {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| GhostError::PageOp(format!("net body decode: {e}")))?;
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        if result.get("evicted").and_then(|v| v.as_bool()) == Some(true) {
            return Err(GhostError::PageOp(
                "response body evicted (too large or consumed) — start capture before the request"
                    .into(),
            ));
        }
        Ok(serde_json::to_string(&result).unwrap_or_default())
    }

    async fn pixels_ref(&self, r: &str, gw: u32, gh: u32) -> Result<String> {
        // Kick off the render + async image load...
        let out = self
            .evaluate(&crate::a11y::pixels_ref_js(r, gw, gh))
            .await?;
        if out.as_str() == Some("STALE-REF") {
            return Err(GhostError::PageOp(format!(
                "ref {r} is stale — rerun page_a11y"
            )));
        }
        // ...then poll for the settled grid (img/background load, CORS).
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let res = self.evaluate(crate::a11y::pixels_poll_js()).await?;
            let s = res.as_str().unwrap_or("PENDING");
            if s != "PENDING" {
                return match s {
                    "CORS-TAINT" => Err(GhostError::PageOp(format!(
                        "pixels_ref({r}): image is CORS-tainted — cannot read"
                    ))),
                    "IMG-LOAD-FAIL" | "BG-LOAD-FAIL" => Err(GhostError::PageOp(format!(
                        "pixels_ref({r}): image failed to load"
                    ))),
                    "DRAW-FAIL" => Err(GhostError::PageOp(format!(
                        "pixels_ref({r}): drawImage failed"
                    ))),
                    other => Ok(other.to_string()),
                };
            }
        }
        Err(GhostError::PageOp(format!(
            "pixels_ref({r}): image load did not settle in time"
        )))
    }

    async fn contrast_ref(&self, r: &str, gw: u32, gh: u32, radius: u32) -> Result<String> {
        let out = self
            .evaluate(&crate::a11y::contrast_ref_js(r, gw, gh, radius))
            .await?;
        if out.as_str() == Some("STALE-REF") {
            return Err(GhostError::PageOp(format!(
                "ref {r} is stale — rerun page_a11y"
            )));
        }
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let res = self.evaluate(crate::a11y::contrast_poll_js()).await?;
            let s = res.as_str().unwrap_or("PENDING");
            if s != "PENDING" {
                return match s {
                    "CORS-TAINT" => Err(GhostError::PageOp(format!(
                        "contrast_ref({r}): image is CORS-tainted"
                    ))),
                    "IMG-LOAD-FAIL" | "BG-LOAD-FAIL" => Err(GhostError::PageOp(format!(
                        "contrast_ref({r}): image failed to load"
                    ))),
                    other => Ok(other.to_string()),
                };
            }
        }
        Err(GhostError::PageOp(format!(
            "contrast_ref({r}): image load did not settle in time"
        )))
    }

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
        let out = self
            .evaluate(&crate::a11y::match_image_js(
                needle_ref, nx, ny, nw, nh, hay_ref, hx, hy, hw, hh,
            ))
            .await?;
        if out.as_str() == Some("STALE-REF") {
            return Err(GhostError::PageOp("ref stale — rerun page_a11y".into()));
        }
        for _ in 0..120 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let res = self.evaluate(crate::a11y::match_poll_js()).await?;
            let s = res.as_str().unwrap_or("PENDING");
            if s != "PENDING" {
                if let Some(err) = s.strip_prefix("MATCH-FAIL:") {
                    return Err(GhostError::PageOp(err.to_string()));
                }
                return Ok(s.to_string());
            }
        }
        Err(GhostError::PageOp(
            "match_image: did not settle in time".into(),
        ))
    }

    async fn type_ref(&self, r: &str, text: &str) -> Result<()> {
        // Fire...
        let out = self
            .evaluate(&crate::a11y::type_ref_action_js(r, text))
            .await?;
        if out.as_str() == Some("STALE-REF") {
            return Err(GhostError::PageOp(format!(
                "ref {r} is stale — rerun page_a11y"
            )));
        }
        // ...let async editors (Lexical and friends) settle...
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        // ...then verify what actually landed.
        let check = self.evaluate(&crate::a11y::read_ref_js(r)).await?;
        let s = check.as_str().unwrap_or_default();
        if s == "STALE-REF" {
            return Err(GhostError::PageOp(format!("ref {r} went stale mid-type")));
        }
        let len: usize = s
            .strip_prefix("LEN:")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if len < text.chars().count() / 2 {
            return Err(GhostError::PageOp(format!(
                "type_ref({r}) landed {len} of {} chars — the ref is probably STALE (page changed since page_a11y): rerun page_a11y, get the new ref, retry",
                text.chars().count()
            )));
        }
        Ok(())
    }

    async fn dispatch_wheel(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<()> {
        let sid = self.session_id().await?;
        let payload = serde_json::json!({
            "x": x, "y": y, "deltaX": dx, "deltaY": dy, "deltaZ": 0, "modifiers": 0
        });
        self.conn
            .request_session("Page.dispatchWheelEvent", payload, Some(&sid))
            .await?;
        Ok(())
    }

    async fn browser_cookies(
        &self,
        action: &str,
        cookies: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // Browser domain commands live on the ROOT session (no sessionId).
        match action {
            "get" => {
                let r = self
                    .conn
                    .request("Browser.getCookies", serde_json::json!({}))
                    .await?;
                Ok(r.get("cookies").cloned().unwrap_or(serde_json::json!([])))
            }
            "set" => {
                let n = cookies.as_array().map(|a| a.len()).unwrap_or(0);
                self.conn
                    .request(
                        "Browser.setCookies",
                        serde_json::json!({ "cookies": cookies }),
                    )
                    .await?;
                Ok(serde_json::json!({ "ok": true, "set": n }))
            }
            "clear" => {
                self.conn
                    .request("Browser.clearCookies", serde_json::json!({}))
                    .await?;
                Ok(serde_json::json!({ "ok": true }))
            }
            other => Err(GhostError::PageOp(format!(
                "browser_cookies action must be get|set|clear, got {other}"
            ))),
        }
    }

    async fn video_start(&self, dir: &str, width: u32, height: u32) -> Result<serde_json::Value> {
        // Native juggler recorder: Browser.setVideoRecordingOptions on the
        // DEFAULT context (no browserContextId — Browser.newPage pages live
        // there). Every page starts writing .webm into `dir`, and the
        // Page.videoRecordingStarted event (session-filtered pump) hands us
        // the exact file path.
        std::fs::create_dir_all(dir)
            .map_err(|e| GhostError::PageOp(format!("video dir create failed: {e}")))?;
        self.conn
            .request(
                "Browser.setVideoRecordingOptions",
                serde_json::json!({
                    "options": { "dir": dir, "width": width, "height": height }
                }),
            )
            .await?;
        // Bounded wait for the Started event (recorder spin-up is async).
        let mut file = None;
        for _ in 0..30 {
            if self.video_recording.load(Ordering::SeqCst) {
                file = self.video_file.lock().await.clone();
                if file.is_some() {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Ok(serde_json::json!({
            "recording": true,
            "dir": dir,
            "file": file,
        }))
    }

    async fn video_stop(&self) -> Result<serde_json::Value> {
        // Options omitted → undefined → the context stops recording every
        // page (BrowserContext.setVideoRecordingOptions stops existing ones).
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.conn
                .request("Browser.setVideoRecordingOptions", serde_json::json!({})),
        )
        .await;
        let file = self.video_file.lock().await.clone();
        // The encoder finalizes the container after stop — poll until the
        // file size stops growing (bounded ~5s) so callers get a valid .webm.
        let mut bytes = 0u64;
        if let Some(f) = &file {
            let mut last = u64::MAX;
            for _ in 0..20 {
                bytes = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                if bytes > 0 && bytes == last {
                    break;
                }
                last = bytes;
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        self.video_recording.store(false, Ordering::SeqCst);
        Ok(serde_json::json!({
            "recording": false,
            "file": file,
            "bytes": bytes,
        }))
    }

    async fn video_status(&self) -> Result<serde_json::Value> {
        let file = self.video_file.lock().await.clone();
        let bytes = file
            .as_ref()
            .and_then(|f| std::fs::metadata(f).ok())
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(serde_json::json!({
            "recording": self.video_recording.load(Ordering::SeqCst),
            "file": file,
            "bytes": bytes,
        }))
    }

    async fn grant_permissions(&self, origin: &str, permissions: &[String]) -> Result<()> {
        self.conn
            .request(
                "Browser.grantPermissions",
                serde_json::json!({ "origin": origin, "permissions": permissions }),
            )
            .await?;
        Ok(())
    }

    async fn reset_permissions(&self) -> Result<()> {
        self.conn
            .request("Browser.resetPermissions", serde_json::json!({}))
            .await?;
        Ok(())
    }

    async fn set_download_options(&self, behavior: &str, dir: &str) -> Result<()> {
        self.conn
            .request(
                "Browser.setDownloadOptions",
                serde_json::json!({ "downloadOptions": { "behavior": behavior, "downloadsDir": dir } }),
            )
            .await?;
        Ok(())
    }

    async fn dialog_setup(&self, accept: bool, prompt_text: Option<String>) -> Result<()> {
        let sid = self.session_id().await?;
        let st = dialog_state_for(&self.target_id);
        *st.policy.lock().unwrap() = DialogPolicy {
            accept,
            prompt_text,
        };
        if st.listening.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(()); // listener already running
        }
        let conn = self.conn.clone();
        let st2 = st.clone();
        tokio::spawn(async move {
            let mut rx = conn.subscribe();
            while let Ok(msg) = rx.recv().await {
                let evt_sid = msg.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
                if evt_sid != sid {
                    continue;
                }
                if let Some("Page.dialogOpened") = msg.get("method").and_then(|m| m.as_str()) {
                    let p = msg.get("params").cloned().unwrap_or(serde_json::json!({}));
                    let dialog_id = p
                        .get("dialogId")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if dialog_id.is_empty() {
                        continue;
                    }
                    let (accept, prompt) = {
                        let pol = st2.policy.lock().unwrap();
                        (pol.accept, pol.prompt_text.clone())
                    };
                    let mut hp = serde_json::json!({ "dialogId": dialog_id, "accept": accept });
                    let answer = if accept { prompt.as_deref() } else { None };
                    if let Some(t) = answer {
                        hp["promptText"] = serde_json::json!(t);
                    }
                    let handled = conn
                        .request_session("Page.handleDialog", hp, Some(&sid))
                        .await
                        .is_ok();
                    st2.log.lock().unwrap().push(serde_json::json!({
                        "id": dialog_id,
                        "type": p.get("type").cloned().unwrap_or(serde_json::json!(null)),
                        "message": p.get("message").and_then(|v| v.as_str()).unwrap_or(""),
                        "accepted": accept,
                        "handled": handled,
                        "ts": now_ms(),
                    }));
                }
            }
        });
        Ok(())
    }

    async fn dialog_log(&self, clear: bool) -> Result<serde_json::Value> {
        let st = dialog_state_for(&self.target_id);
        let listening = st.listening.load(std::sync::atomic::Ordering::SeqCst);
        let mut log = st.log.lock().unwrap();
        let events = if clear {
            std::mem::take(&mut *log)
        } else {
            log.clone()
        };
        let count = events.len();
        let policy = st.policy.lock().unwrap();
        Ok(serde_json::json!({
            "count": count,
            "events": events,
            "listening": listening,
            "policy": { "accept": policy.accept, "prompt_text": policy.prompt_text },
        }))
    }

    async fn emulate(&self, ops: &serde_json::Value) -> Result<serde_json::Value> {
        let sid = self.session_id().await?;
        let mut applied: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        fn push_err<E: std::fmt::Display>(errors: &mut Vec<String>, key: &str, e: E) {
            errors.push(format!("{key}: {e}"));
        }

        // --- Page-level emulation in ONE setEmulatedMedia call ---
        let mut media_p = serde_json::Map::new();
        if let Some(v) = ops.get("color_scheme").and_then(|v| v.as_str()) {
            match v {
                "dark" | "light" | "none" => {
                    let cs = if v == "none" { "no-preference" } else { v };
                    media_p.insert("colorScheme".into(), serde_json::json!(cs));
                    applied.push("color_scheme".into());
                }
                other => errors.push(format!(
                    "color_scheme: invalid value {other:?} (dark|light|none)"
                )),
            }
        }
        if let Some(v) = ops.get("reduced_motion").and_then(|v| v.as_str()) {
            match v {
                "reduce" | "none" => {
                    let rm = if v == "none" { "no-preference" } else { v };
                    media_p.insert("reducedMotion".into(), serde_json::json!(rm));
                    applied.push("reduced_motion".into());
                }
                other => errors.push(format!(
                    "reduced_motion: invalid value {other:?} (reduce|none)"
                )),
            }
        }
        if let Some(v) = ops.get("forced_colors").and_then(|v| v.as_str()) {
            match v {
                "active" | "none" => {
                    media_p.insert("forcedColors".into(), serde_json::json!(v));
                    applied.push("forced_colors".into());
                }
                other => errors.push(format!(
                    "forced_colors: invalid value {other:?} (active|none)"
                )),
            }
        }
        if let Some(v) = ops.get("contrast").and_then(|v| v.as_str()) {
            match v {
                "less" | "more" | "custom" | "none" => {
                    let c = if v == "none" { "no-preference" } else { v };
                    media_p.insert("contrast".into(), serde_json::json!(c));
                    applied.push("contrast".into());
                }
                other => errors.push(format!(
                    "contrast: invalid value {other:?} (less|more|custom|none)"
                )),
            }
        }
        if let Some(v) = ops.get("media").and_then(|v| v.as_str()) {
            match v {
                "print" | "screen" | "none" => {
                    media_p.insert("type".into(), serde_json::json!(v));
                    applied.push("media".into());
                }
                other => errors.push(format!(
                    "media: invalid value {other:?} (print|screen|none)"
                )),
            }
        }
        if !media_p.is_empty() {
            if let Err(e) = self
                .conn
                .request_session(
                    "Page.setEmulatedMedia",
                    serde_json::Value::Object(media_p),
                    Some(&sid),
                )
                .await
            {
                push_err(&mut errors, "media", e);
            }
        }

        // --- Page.setViewportSize ---
        if let Some(v) = ops.get("viewport") {
            if v.is_null() {
                if let Err(e) = self
                    .conn
                    .request_session(
                        "Page.setViewportSize",
                        serde_json::json!({ "viewportSize": serde_json::Value::Null }),
                        Some(&sid),
                    )
                    .await
                {
                    push_err(&mut errors, "viewport", e);
                } else {
                    applied.push("viewport".into());
                }
            } else {
                let w = v.get("width").and_then(|x| x.as_u64());
                let h = v.get("height").and_then(|x| x.as_u64());
                match (w, h) {
                    (Some(w), Some(h)) => {
                        if let Err(e) = self
                            .conn
                            .request_session(
                                "Page.setViewportSize",
                                serde_json::json!({ "viewportSize": { "width": w, "height": h } }),
                                Some(&sid),
                            )
                            .await
                        {
                            push_err(&mut errors, "viewport", e);
                        } else {
                            applied.push("viewport".into());
                        }
                    }
                    _ => errors.push("viewport: need {width, height}".into()),
                }
            }
        }

        // --- Browser-level overrides (root session) ---
        if let Some(v) = ops.get("online") {
            let override_val = match v.as_bool() {
                Some(true) => "online",
                Some(false) => "offline",
                None => {
                    errors.push("online: need true|false".into());
                    ""
                }
            };
            if !override_val.is_empty() {
                if let Err(e) = self
                    .conn
                    .request(
                        "Browser.setOnlineOverride",
                        serde_json::json!({ "override": override_val }),
                    )
                    .await
                {
                    push_err(&mut errors, "online", e);
                } else {
                    applied.push("online".into());
                }
            }
        }
        if let Some(v) = ops.get("geolocation") {
            let params = if v.is_null() {
                serde_json::json!({ "geolocation": serde_json::Value::Null })
            } else {
                let lat = v.get("latitude").and_then(|x| x.as_f64());
                let lon = v.get("longitude").and_then(|x| x.as_f64());
                match (lat, lon) {
                    (Some(lat), Some(lon)) => {
                        let mut geo = serde_json::json!({ "latitude": lat, "longitude": lon });
                        if let Some(a) = v.get("accuracy").and_then(|x| x.as_f64()) {
                            geo["accuracy"] = serde_json::json!(a);
                        }
                        serde_json::json!({ "geolocation": geo })
                    }
                    _ => {
                        errors.push("geolocation: need {latitude, longitude} or null".into());
                        serde_json::Value::Null
                    }
                }
            };
            if !params.is_null() {
                if let Err(e) = self
                    .conn
                    .request("Browser.setGeolocationOverride", params)
                    .await
                {
                    push_err(&mut errors, "geolocation", e);
                } else {
                    applied.push("geolocation".into());
                }
            }
        }
        if let Some(v) = ops.get("user_agent") {
            let params = if v.is_null() {
                serde_json::json!({ "userAgent": serde_json::Value::Null })
            } else {
                match v.as_str() {
                    Some(s) => serde_json::json!({ "userAgent": s }),
                    None => {
                        errors.push("user_agent: need string or null".into());
                        serde_json::Value::Null
                    }
                }
            };
            if !params.is_null() {
                if let Err(e) = self
                    .conn
                    .request("Browser.setUserAgentOverride", params)
                    .await
                {
                    push_err(&mut errors, "user_agent", e);
                } else {
                    applied.push("user_agent".into());
                }
            }
        }
        if let Some(v) = ops.get("timezone") {
            let params = if v.is_null() {
                serde_json::json!({ "timezoneId": serde_json::Value::Null })
            } else {
                match v.as_str() {
                    Some(s) => serde_json::json!({ "timezoneId": s }),
                    None => {
                        errors.push("timezone: need string or null".into());
                        serde_json::Value::Null
                    }
                }
            };
            if !params.is_null() {
                if let Err(e) = self
                    .conn
                    .request("Browser.setTimezoneOverride", params)
                    .await
                {
                    push_err(&mut errors, "timezone", e);
                } else {
                    applied.push("timezone".into());
                }
            }
        }
        if let Some(v) = ops.get("locale") {
            let params = if v.is_null() {
                serde_json::json!({ "locale": serde_json::Value::Null })
            } else {
                match v.as_str() {
                    Some(s) => serde_json::json!({ "locale": s }),
                    None => {
                        errors.push("locale: need string or null".into());
                        serde_json::Value::Null
                    }
                }
            };
            if !params.is_null() {
                if let Err(e) = self.conn.request("Browser.setLocaleOverride", params).await {
                    push_err(&mut errors, "locale", e);
                } else {
                    applied.push("locale".into());
                }
            }
        }
        if let Some(v) = ops.get("platform") {
            let params = if v.is_null() {
                serde_json::json!({ "platform": serde_json::Value::Null })
            } else {
                match v.as_str() {
                    Some(s) => serde_json::json!({ "platform": s }),
                    None => {
                        errors.push("platform: need string or null".into());
                        serde_json::Value::Null
                    }
                }
            };
            if !params.is_null() {
                if let Err(e) = self
                    .conn
                    .request("Browser.setPlatformOverride", params)
                    .await
                {
                    push_err(&mut errors, "platform", e);
                } else {
                    applied.push("platform".into());
                }
            }
        }
        if let Some(v) = ops.get("headers") {
            let params = if v.is_null() {
                serde_json::json!({ "headers": serde_json::json!([]) })
            } else if let Some(map) = v.as_object() {
                let list: Vec<serde_json::Value> = map
                    .iter()
                    .map(|(k, val)| {
                        serde_json::json!({ "name": k, "value": val.as_str().unwrap_or_default() })
                    })
                    .collect();
                serde_json::json!({ "headers": list })
            } else {
                errors.push("headers: need object {name: value} or null".into());
                serde_json::Value::Null
            };
            if !params.is_null() {
                if let Err(e) = self
                    .conn
                    .request("Browser.setExtraHTTPHeaders", params)
                    .await
                {
                    push_err(&mut errors, "headers", e);
                } else {
                    applied.push("headers".into());
                }
            }
        }
        if let Some(v) = ops.get("http_auth") {
            let params = if v.is_null() {
                serde_json::json!({ "credentials": serde_json::Value::Null })
            } else {
                let u = v.get("username").and_then(|x| x.as_str());
                let p = v.get("password").and_then(|x| x.as_str());
                match (u, p) {
                    (Some(u), Some(p)) => {
                        serde_json::json!({ "credentials": { "username": u, "password": p } })
                    }
                    _ => {
                        errors.push("http_auth: need {username, password} or null".into());
                        serde_json::Value::Null
                    }
                }
            };
            if !params.is_null() {
                if let Err(e) = self
                    .conn
                    .request("Browser.setHTTPCredentials", params)
                    .await
                {
                    push_err(&mut errors, "http_auth", e);
                } else {
                    applied.push("http_auth".into());
                }
            }
        }

        Ok(serde_json::json!({ "applied": applied, "errors": errors }))
    }

    async fn screenshot(&self, full_page: bool) -> Result<Vec<u8>> {
        use base64::Engine as _;
        let sid = self.session_id().await?;

        // drawSnapshot's clip is in DOCUMENT coordinates: clip (0,0,w,h)
        // always renders the TOP of the document no matter where the
        // page is scrolled — that is what made viewport shots look
        // "frozen" on scroll (paint changes in the top band still showed
        // up; the scroll offset never did). The fix is therefore to feed
        // the CURRENT scroll offset as clip.y, capturing exactly the
        // visible viewport. full-page shots keep y=0 (whole document).
        let dims_expr = if full_page {
            "JSON.stringify([Math.max(document.documentElement.scrollWidth, document.body ? document.body.scrollWidth : 0), Math.max(document.documentElement.scrollHeight, document.body ? document.body.scrollHeight : 0), 0])"
        } else {
            "JSON.stringify([window.innerWidth, window.innerHeight, Math.round(window.scrollY || window.pageYOffset || 0)])"
        };
        let dims = self.evaluate(dims_expr).await?;
        let parsed = dims
            .as_str()
            .and_then(|s| {
                let inner = s.trim_matches('"');
                let parts: Vec<u32> = inner
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .filter_map(|p| p.trim().parse().ok())
                    .collect();
                if parts.len() == 3 {
                    Some((parts[0], parts[1], parts[2]))
                } else {
                    None
                }
            })
            .unwrap_or((1280, 800, 0));
        // Engine canvas caps: 32767px per side.
        let cap = 32767u32;
        let (w, h, sy) = (parsed.0.min(cap), parsed.1.min(cap), parsed.2);

        let result = self
            .conn
            .request_session(
                "Page.screenshot",
                serde_json::json!({
                    "mimeType": "image/png",
                    "clip": { "x": 0, "y": sy, "width": w, "height": h },
                    // 1:1 pixels regardless of the identity's spoofed DPR.
                    "omitDeviceScaleFactor": true,
                }),
                Some(&sid),
            )
            .await?;
        let b64 = result
            .get("data")
            .and_then(|d| d.as_str())
            .ok_or_else(|| GhostError::Protocol("Page.screenshot returned no data".into()))?;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| GhostError::Protocol(format!("screenshot base64: {e}")))
    }

    async fn evaluate(&self, expression: &str) -> Result<serde_json::Value> {
        // Juggler's executionContextId ("id-N") dies on navigation and is
        // RE-MINTED after a crash/tab-recreate — and so is the frame id
        // ("mainframe-N"). The old ladder cleared ONLY the context id and
        // retried the possibly-dead frame id, so juggler eventually threw
        // "Failed to find execution context with id = mainframe-N" straight
        // at the caller. Recovery ladder (bounded ~12s):
        //   0) fail fast with an actionable message when the renderer died;
        //   1) use the cached context id (fast path);
        //   2) on a stale rejection, drop exactly what was rejected — the
        //      context pin for a rejected "id-N", the frame pins TOO when
        //      the frame id itself was the rejected target (proven dead;
        //      the next mainframe-* event re-pins by prefix rule);
        //   3) with BOTH pins empty there is NO event-free primitive (juggler
        //      REQUIRES executionContextId and does not implement
        //      Runtime.enable) — force one Page.reload, which deterministically
        //      mints fresh context events the pump adopts, at the cost of
        //      in-page state;
        //   4) give up with guidance instead of raw channel noise.
        let mut timeouts = 0u32;
        let mut reloaded = false;
        let mut last_error = String::from("no execution context");
        for attempt in 0..6 {
            if self.crashed.load(Ordering::SeqCst) {
                return Err(GhostError::PageOp(
                    "renderer crashed: the page's execution context is gone — \
                     reopen the page with page_open"
                        .into(),
                ));
            }
            // Cached context id first (fast path). On a miss, give the pump
            // a short window to (re)adopt a live context event before we
            // fall back — a fresh pin usually lands within one event tick.
            let cached = match self.execution_context().await {
                Ok(c) => Some(c),
                Err(_) => {
                    let wait_ms: u64 = if attempt == 0 { 400 } else { 1500 };
                    let started = std::time::Instant::now();
                    loop {
                        if let Ok(c) = self.execution_context().await {
                            break Some(c);
                        }
                        if started.elapsed().as_millis() as u64 >= wait_ms {
                            break None;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    }
                }
            };
            // What we will send: cached context id, else the pinned main
            // frame id (juggler resolves a frame's context from its id),
            // else NOTHING — juggler's default context (no executionContextId).
            let (target, sent_frame) = match cached {
                Some(c) => (Some(c), false),
                None => match self.frame_id.lock().await.clone() {
                    Some(f) => (Some(f), true),
                    None => (None, false),
                },
            };
            let sid = match self.session_id().await {
                Ok(s) => s,
                Err(_) => {
                    return Err(GhostError::PageOp(
                        "target session detached (page closed or renderer died) — \
                         reopen the page with page_open"
                            .into(),
                    ));
                }
            };
            tracing::debug!(target: "ghostcloak::camoufox", "evaluate attempt {attempt} ctx={target:?} sid={sid}");
            let Some(target) = target else {
                // Both pins empty. A params object WITHOUT executionContextId
                // is rejected outright by juggler ("Expected <root>....
                // executionContextId to be |string|; found |undefined|") and
                // Runtime.enable is unsupported — there is no event-free
                // primitive. Force one document reload: it deterministically
                // mints fresh context events the pump adopts.
                if attempt >= 2 && !reloaded {
                    reloaded = true;
                    tracing::debug!(target: "ghostcloak::camoufox",
                        "evaluate: both pins empty at attempt {attempt} — Page.reload self-heal");
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        self.conn
                            .request_session("Page.reload", serde_json::json!({}), Some(&sid)),
                    )
                    .await;
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                }
                continue;
            };
            let params = serde_json::json!({
                "expression": expression,
                "executionContextId": target,
                "returnByValue": true,
            });
            let result = match self
                .conn
                .request_session("Runtime.evaluate", params, Some(&sid))
                .await
            {
                Ok(r) => r,
                Err(error) if attempt < 5 => {
                    let msg = error.to_string();
                    last_error = msg.clone();
                    let timeoutish = msg.contains("timed out") || msg.contains("timeout");
                    if timeoutish {
                        timeouts += 1;
                        // A page that stays busy past one full retry is hung,
                        // not racing a context swap — don't spin for minutes.
                        if timeouts >= 2 {
                            return Err(GhostError::PageOp(format!(
                                "evaluate timed out (page busy or hung): {msg}"
                            )));
                        }
                        // Busy page ≠ dead ids: keep the pins (a same-document
                        // navigation emits NO replacement events, so a clear
                        // here would starve the retry loop forever).
                    } else if msg.contains("cannot find session") {
                        // The session itself is gone (target closed or the
                        // renderer died): no retry can help — drop it cleanly
                        // and say so in terms the caller can act on.
                        *self.session_id.lock().await = None;
                        return Err(GhostError::PageOp(
                            "target session detached (page closed or renderer died) — \
                             reopen the page with page_open"
                                .into(),
                        ));
                    } else if !msg.contains("context") {
                        // Not a stale-id rejection — don't punish the ids.
                        return Err(GhostError::PageOp(format!("evaluate failed: {msg}")));
                    } else {
                        // We just sent THIS target and juggler rejected it.
                        // Context id → only the context pin dies. Frame id →
                        // the frame itself is proven dead: drop every pin so
                        // the next mainframe-* event (prefix rule) re-pins a
                        // fresh one. An omitted (default-context) rejection
                        // means the pins were already empty — nothing to clear.
                        *self.execution_context_id.lock().await = None;
                        if sent_frame {
                            *self.frame_id.lock().await = None;
                            *self.main_frame_id.lock().await = None;
                        }
                        tracing::debug!(target: "ghostcloak::camoufox", "evaluate stale ({msg}); sent_frame={sent_frame}, recovering");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(400 + attempt * 300)).await;
                    continue;
                }
                Err(e) => {
                    return Err(GhostError::PageOp(format!(
                        "evaluate failed after recovery: {e}"
                    )));
                }
            };
            if let Some(exc) = result.get("exceptionDetails") {
                return Err(GhostError::PageOp(format!(
                    "js exception: {}",
                    serde_json::to_string(exc).unwrap_or_default()
                )));
            }
            let value = result
                .pointer("/result/value")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            if value.is_null() && attempt < 5 {
                // A null return on a stale context is indistinguishable from
                // a legitimately-null expression. Nudge: on the next attempt
                // try the frame id directly — Juggler resolves the frame's
                // current default context — before giving up entirely.
                tracing::debug!(target: "ghostcloak::camoufox", "null result on attempt {attempt}, retrying via frame id");
                if attempt >= 1 {
                    let fid = self.frame_id.lock().await.clone();
                    if let Some(f) = fid {
                        *self.execution_context_id.lock().await = Some(f);
                    }
                } else {
                    *self.execution_context_id.lock().await = None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                continue;
            }
            tracing::debug!(target: "ghostcloak::camoufox", "evaluate done attempt {attempt}: {value:?}");
            return Ok(value);
        }
        Err(GhostError::PageOp(format!(
            "execution context unavailable after recovery ({last_error}); \
             the page may have reloaded or crashed — reopen it with page_open"
        )))
    }

    async fn url(&self) -> Result<String> {
        Ok(self
            .evaluate("location.href")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    async fn close(&self) -> Result<()> {
        // Juggler's page close is a Page.* method on the target session
        // (`Target.close` is not implemented by this juggler version).
        let sid = self.session_id().await?;
        self.conn
            .request_session(
                "Page.close",
                serde_json::json!({ "runBeforeUnload": false }),
                Some(&sid),
            )
            .await?;
        Ok(())
    }
}

impl Drop for CamoufoxEngine {
    fn drop(&mut self) {
        // Last-resort cleanup: if shutdown() never ran (caller panicked or
        // the channel died early), still remove the ephemeral profile and
        // SIGKILL any leftover browser process from our launch.
        if let Some(dir) = &self.ephemeral_profile {
            let _ = std::fs::remove_dir_all(dir);
        }
        self.conn.kill_now();
    }
}
