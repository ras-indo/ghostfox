//! Runtime configuration: TOML file, environment variables, CLI args.
//!
//! Precedence (highest wins): CLI > ENV > FILE > DEFAULT.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use rand::rngs::OsRng;
use rand::TryRngCore;
use serde::{Deserialize, Serialize};

/// Canonical transport mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Stdio,
    Http,
    Both,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio => write!(f, "stdio"),
            Self::Http => write!(f, "http"),
            Self::Both => write!(f, "both"),
        }
    }
}

impl std::str::FromStr for Transport {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "http" => Ok(Self::Http),
            "both" => Ok(Self::Both),
            _ => Err(format!(
                "invalid transport '{s}': expected stdio, http, or both"
            )),
        }
    }
}

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
}

fn default_endpoint() -> String {
    "/mcp".into()
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8787,
            endpoint: "/mcp".into(),
            api_key: String::new(),
            allowed_origins: Vec::new(),
        }
    }
}

/// Server-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub transport: Transport,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: Transport::Stdio,
        }
    }
}

/// Top-level config (maps to config.toml).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub http: HttpConfig,
}

impl Config {
    /// Load config from file + env, applying defaults.
    pub fn load() -> Result<Self> {
        let file_config = load_file_config();
        let mut cfg = file_config.unwrap_or_default();
        apply_env(&mut cfg);
        Ok(cfg)
    }
}

// ---------------------------------------------------------------------------
// File loading
// ---------------------------------------------------------------------------

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("GHOSTFOX_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".ghostfox").join("config.toml")
}

fn load_file_config() -> Result<Config> {
    let path = config_path();
    load_file_config_at(&path)
}

/// Load config from an explicit path (tests use this to avoid parallel
/// env-var races on GHOSTFOX_CONFIG).
fn load_file_config_at(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&content).with_context(|| format!("failed to parse config {}", path.display()))
}

// ---------------------------------------------------------------------------
// Environment overrides
// ---------------------------------------------------------------------------

fn apply_env(cfg: &mut Config) {
    if let Ok(v) = std::env::var("GHOSTFOX_TRANSPORT") {
        if let Ok(t) = v.parse::<Transport>() {
            cfg.server.transport = t;
        }
    }
    if let Ok(v) = std::env::var("GHOSTFOX_HTTP_HOST") {
        cfg.http.host = v;
    }
    if let Ok(v) = std::env::var("GHOSTFOX_HTTP_PORT") {
        if let Ok(p) = v.parse::<u16>() {
            cfg.http.port = p;
        }
    }
    if let Ok(v) = std::env::var("GHOSTFOX_HTTP_ENDPOINT") {
        cfg.http.endpoint = v;
    }
    if let Ok(v) = std::env::var("GHOSTFOX_HTTP_API_KEY") {
        cfg.http.api_key = v;
    }
    if let Ok(v) = std::env::var("GHOSTFOX_HTTP_ALLOWED_ORIGINS") {
        cfg.http.allowed_origins = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
}

// ---------------------------------------------------------------------------
// CLI overrides (raw Option<T> so we can distinguish "user passed a value"
// from "user didn't touch this flag")
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub config: Option<String>,
    pub transport: Option<Transport>,
    pub http_host: Option<String>,
    pub http_port: Option<u16>,
    pub http_endpoint: Option<String>,
    pub http_api_key: Option<String>,
}

impl CliArgs {
    /// Merge CLI overrides on top of the loaded config.
    pub fn apply(&self, cfg: &mut Config) {
        if let Some(t) = &self.transport {
            cfg.server.transport = *t;
        }
        if let Some(h) = &self.http_host {
            cfg.http.host = h.clone();
        }
        if let Some(p) = self.http_port {
            cfg.http.port = p;
        }
        if let Some(ep) = &self.http_endpoint {
            cfg.http.endpoint = ep.clone();
        }
        if let Some(k) = &self.http_api_key {
            cfg.http.api_key = k.clone();
        }
    }
}

// ---------------------------------------------------------------------------
// First-run API key generation + atomic persistence
// ---------------------------------------------------------------------------

/// Generate a cryptographically secure 256-bit API key, base64url-encoded
/// without padding.
pub fn generate_api_key() -> String {
    let mut bytes = [0u8; 32];
    OsRng
        .try_fill_bytes(&mut bytes)
        .expect("failed to generate random bytes");
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// If transport needs HTTP and the API key is empty, generate one and
/// persist it to the config file. Returns the (possibly new) config.
pub fn ensure_api_key(mut cfg: Config) -> Result<Config> {
    if matches!(cfg.server.transport, Transport::Http | Transport::Both)
        && cfg.http.api_key.is_empty()
    {
        cfg.http.api_key = generate_api_key();
        persist_config(&cfg)?;
        tracing::info!("generated new API key and saved to config");
    }
    Ok(cfg)
}

/// Atomic write of config file. On Unix, sets mode 0600.
fn persist_config(cfg: &Config) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let content = toml::to_string_pretty(cfg).context("failed to serialize config")?;
    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    let tmp = dir.join(format!(".config.toml.{}", std::process::id()));
    std::fs::write(&tmp, &content).context("failed to write temp config")?;

    // Set 0600 on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }

    std::fs::rename(&tmp, &path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("failed to rename config to {}", path.display())
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Hot-reload support
// ---------------------------------------------------------------------------

/// Metadata for detecting config file changes.
///
/// Uses (mtime, size) as the change fingerprint: mtime alone is not
/// reliable on filesystems with coarse timestamp granularity (e.g. ext4
/// 1s), so a size change with an unchanged mtime also triggers reload.
#[derive(Debug, Clone)]
pub struct ReloadTracker {
    path: PathBuf,
    last_modified: SystemTime,
    last_len: u64,
    last_config: Config,
}

impl ReloadTracker {
    pub fn new(cfg: Config) -> Self {
        let path = config_path();
        Self::new_at(cfg, path)
    }

    /// Create a tracker watching an explicit config path (used by tests to
    /// avoid parallel-test env-var races on GHOSTFOX_CONFIG).
    pub fn new_at(cfg: Config, path: PathBuf) -> Self {
        let (last_modified, last_len) = file_fingerprint(&path);
        Self {
            path,
            last_modified,
            last_len,
            last_config: cfg,
        }
    }

    /// Check if config file has changed and is valid. Returns whether a
    /// reload happened and the current active config (either new or
    /// last-known-good).
    pub fn check_reload(&mut self) -> (bool, Config) {
        let (current_modified, current_len) = file_fingerprint(&self.path);

        if current_modified <= self.last_modified && current_len == self.last_len {
            return (false, self.last_config.clone());
        }

        // File changed — try to reload
        self.last_modified = current_modified;
        self.last_len = current_len;
        match std::fs::read_to_string(&self.path) {
            Ok(content) => match toml::from_str::<Config>(&content) {
                Ok(mut new_cfg) => {
                    apply_env(&mut new_cfg);
                    tracing::info!("config reloaded from {}", self.path.display());
                    self.last_config = new_cfg.clone();
                    (true, new_cfg)
                }
                Err(e) => {
                    tracing::error!("invalid config file, keeping last known good: {e}");
                    (false, self.last_config.clone())
                }
            },
            Err(e) => {
                tracing::error!("failed to read config file, keeping last known good: {e}");
                (false, self.last_config.clone())
            }
        }
    }

    pub fn current(&self) -> &Config {
        &self.last_config
    }
}

/// Read (mtime, size) of a file; both default to zero when unavailable.
fn file_fingerprint(path: &std::path::Path) -> (SystemTime, u64) {
    match std::fs::metadata(path) {
        Ok(m) => (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()),
        Err(_) => (SystemTime::UNIX_EPOCH, 0),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_config_is_stdio() {
        let cfg = Config::default();
        assert_eq!(cfg.server.transport, Transport::Stdio);
        assert_eq!(cfg.http.host, "127.0.0.1");
        assert_eq!(cfg.http.port, 8787);
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = Config::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let parsed: Config = toml::from_str(&s).unwrap();
        assert_eq!(parsed.server.transport, Transport::Stdio);
        assert_eq!(parsed.http.port, 8787);
    }

    #[test]
    fn invalid_toml_returns_error() {
        let result = toml::from_str::<Config>("this is not valid toml {{{{");
        assert!(result.is_err());
    }

    #[test]
    fn transport_parse() {
        assert_eq!("stdio".parse::<Transport>().unwrap(), Transport::Stdio);
        assert_eq!("http".parse::<Transport>().unwrap(), Transport::Http);
        assert_eq!("both".parse::<Transport>().unwrap(), Transport::Both);
        assert_eq!("HTTP".parse::<Transport>().unwrap(), Transport::Http);
        assert!("invalid".parse::<Transport>().is_err());
    }

    #[test]
    fn cli_precedence_over_env() {
        std::env::set_var("GHOSTFOX_HTTP_PORT", "9000");
        let mut cfg = Config::default();
        apply_env(&mut cfg);
        assert_eq!(cfg.http.port, 9000);

        let cli = CliArgs {
            config: None,
            transport: None,
            http_host: None,
            http_port: Some(10000),
            http_endpoint: None,
            http_api_key: None,
        };
        cli.apply(&mut cfg);
        assert_eq!(cfg.http.port, 10000);
    }

    #[test]
    fn env_overrides_file_default() {
        std::env::set_var("GHOSTFOX_TRANSPORT", "http");
        std::env::set_var("GHOSTFOX_HTTP_PORT", "9999");
        let mut cfg = Config::default(); // simulates file config
        apply_env(&mut cfg);
        assert_eq!(cfg.server.transport, Transport::Http);
        assert_eq!(cfg.http.port, 9999);
    }

    #[test]
    fn generate_api_key_format() {
        let key = generate_api_key();
        assert!(key.len() >= 40, "key too short: {}", key.len());
        // base64url chars: A-Z, a-z, 0-9, -, _
        assert!(key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn file_reload_detects_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "[server]\ntransport = \"stdio\"").unwrap();

        std::env::set_var("GHOSTFOX_CONFIG", path.to_str().unwrap());

        let cfg = load_file_config_at(&path).unwrap();
        assert_eq!(cfg.server.transport, Transport::Stdio);

        // Modify
        let mut tracker = ReloadTracker::new_at(cfg, path.clone());
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(
            &path,
            "[server]\ntransport = \"http\"\n\n[http]\napi_key = \"test\"\n",
        )
        .unwrap();

        let (reloaded, new_cfg) = tracker.check_reload();
        assert!(reloaded);
        assert_eq!(new_cfg.server.transport, Transport::Http);
        assert_eq!(new_cfg.http.api_key, "test");
    }

    #[test]
    fn invalid_hot_reload_keeps_previous() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[server]\ntransport = \"stdio\"").unwrap();
        std::env::set_var("GHOSTFOX_CONFIG", path.to_str().unwrap());

        let cfg = load_file_config_at(&path).unwrap();
        let mut tracker = ReloadTracker::new_at(cfg, path.clone());
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "this is not valid {{{{").unwrap();

        let (reloaded, kept) = tracker.check_reload();
        assert!(!reloaded);
        assert_eq!(kept.server.transport, Transport::Stdio);
    }

    #[test]
    fn parse_allowed_origins_from_env() {
        std::env::set_var(
            "GHOSTFOX_HTTP_ALLOWED_ORIGINS",
            "http://localhost:3000, https://example.com",
        );
        let mut cfg = Config::default();
        apply_env(&mut cfg);
        assert_eq!(cfg.http.allowed_origins.len(), 2);
        assert_eq!(cfg.http.allowed_origins[0], "http://localhost:3000");
        assert_eq!(cfg.http.allowed_origins[1], "https://example.com");
    }

    #[test]
    fn empty_allowed_origins_from_empty_env() {
        std::env::set_var("GHOSTFOX_HTTP_ALLOWED_ORIGINS", "");
        let mut cfg = Config::default();
        apply_env(&mut cfg);
        assert!(cfg.http.allowed_origins.is_empty());
    }
}
