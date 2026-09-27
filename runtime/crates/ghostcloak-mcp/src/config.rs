//! Runtime configuration: TOML file + environment + CLI, deterministic
//! precedence `DEFAULT < FILE < ENV < CLI`, first-run API-key bootstrap,
//! and mtime-based hot reload with last-known-good semantics.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;
use std::time::SystemTime;

use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::cli::Cli;

// ── constants ────────────────────────────────────────────────────────────────

pub const DEFAULT_CONFIG_DIR: &str = ".ghostfox";
pub const DEFAULT_CONFIG_FILE: &str = "config.toml";
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8787;
pub const DEFAULT_ENDPOINT: &str = "/mcp";

// ── transport enum ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Http,
    Both,
}

impl Default for Transport {
    fn default() -> Self {
        Self::Stdio
    }
}

impl Transport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Http => "http",
            Self::Both => "both",
        }
    }

    /// Returns `true` when the transport requires an HTTP listener.
    pub fn uses_http(&self) -> bool {
        !matches!(self, Self::Stdio)
    }
}

impl FromStr for Transport {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "http" => Ok(Self::Http),
            "both" => Ok(Self::Both),
            other => Err(ConfigError::InvalidTransport(other.to_string())),
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── resolved (effective) config ───────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub api_key: String,
    pub allowed_origins: Vec<String>,
    pub endpoint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub transport: Transport,
    pub http: HttpConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            transport: Transport::Stdio,
            http: HttpConfig {
                host: DEFAULT_HOST.into(),
                port: DEFAULT_PORT,
                api_key: String::new(),
                allowed_origins: Vec::new(),
                endpoint: DEFAULT_ENDPOINT.into(),
            },
        }
    }
}

// ── TOML file format ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HttpFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_origins: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl FileConfig {
    /// Load from disk.  `NotFound` is mapped to `None` (caller uses
    /// defaults); all other I/O and TOML parse errors are fatal.
    pub fn load_optional(path: &Path) -> Result<Option<Self>, ConfigError> {
        match Self::load(path) {
            Ok(cfg) => Ok(Some(cfg)),
            Err(ConfigError::Read { ref source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }

    fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }
}

// ── environment variables ────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct EnvVars {
    pub config_path: Option<String>,
    pub transport: Option<String>,
    pub http_host: Option<String>,
    pub http_port: Option<String>,
    pub http_api_key: Option<String>,
    pub http_allowed_origins: Option<String>,
}

impl EnvVars {
    /// Read from `std::env` (OS-level environment).
    pub fn collect() -> Self {
        Self {
            config_path: std::env::var("GHOSTFOX_CONFIG").ok(),
            transport: std::env::var("GHOSTFOX_TRANSPORT").ok(),
            http_host: std::env::var("GHOSTFOX_HTTP_HOST").ok(),
            http_port: std::env::var("GHOSTFOX_HTTP_PORT").ok(),
            http_api_key: std::env::var("GHOSTFOX_HTTP_API_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            http_allowed_origins: std::env::var("GHOSTFOX_HTTP_ALLOWED_ORIGINS")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }
}

// ── errors ───────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid TOML in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("invalid transport `{0}` (expected stdio | http | both)")]
    InvalidTransport(String),

    #[error("invalid port `{value}` (expected 1–65535)")]
    InvalidPort { value: String },

    #[error("endpoint `{0}` must start with '/'")]
    InvalidEndpoint(String),

    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("cannot write config {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("cannot serialize config: {source}")]
    Serialize { source: toml::ser::Error },
}

// ── precedence merge ─────────────────────────────────────────────────────────

/// Merge DEFAULT → FILE → ENV → CLI into a resolved [`Config`].
pub fn resolve(cli: &Cli, env: &EnvVars, file: Option<&FileConfig>) -> Result<Config, ConfigError> {
    let mut cfg = Config::default();

    // 1. File (all fields optional; absent = keep default)
    if let Some(f) = file {
        if let Some(srv) = &f.server {
            if let Some(t) = &srv.transport {
                cfg.transport = Transport::from_str(t)?;
            }
        }
        if let Some(h) = &f.http {
            if let Some(v) = &h.host {
                cfg.http.host = v.clone();
            }
            if let Some(v) = h.port {
                cfg.http.port = validate_port(v)?;
            }
            if let Some(v) = &h.api_key {
                cfg.http.api_key = v.clone();
            }
            if let Some(v) = &h.allowed_origins {
                cfg.http.allowed_origins = v.clone();
            }
            if let Some(v) = &h.endpoint {
                validate_endpoint(v)?;
                cfg.http.endpoint = v.clone();
            }
        }
    }

    // 2. Environment
    if let Some(t) = &env.transport {
        cfg.transport = Transport::from_str(t)?;
    }
    if let Some(v) = &env.http_host {
        cfg.http.host = v.clone();
    }
    if let Some(v) = &env.http_port {
        cfg.http.port = parse_port(v)?;
    }
    if let Some(v) = &env.http_api_key {
        cfg.http.api_key = v.clone();
    }
    if let Some(v) = &env.http_allowed_origins {
        cfg.http.allowed_origins = parse_origins(v);
    }

    // 3. CLI
    if let Some(t) = &cli.transport {
        cfg.transport = Transport::from_str(t)?;
    }
    if let Some(v) = &cli.http_host {
        cfg.http.host = v.clone();
    }
    if let Some(v) = &cli.http_port {
        cfg.http.port = parse_port(v)?;
    }
    if let Some(v) = &cli.http_api_key {
        cfg.http.api_key = v.clone();
    }
    if let Some(v) = &cli.http_allowed_origins {
        cfg.http.allowed_origins = parse_origins(v);
    }

    // Final validation
    validate_endpoint(&cfg.http.endpoint)?;

    Ok(cfg)
}

// ── config path resolution ───────────────────────────────────────────────────

/// Determine which config file to use (CLI > ENV > default).
pub fn resolve_config_path(cli: &Cli, env: &EnvVars) -> Option<PathBuf> {
    cli.config
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| env.config_path.as_ref().map(PathBuf::from))
        .or_else(default_config_path)
}

/// `~/.ghostfox/config.toml` (or platform equivalent).
fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(DEFAULT_CONFIG_DIR).join(DEFAULT_CONFIG_FILE))
}

// ── API key generation ───────────────────────────────────────────────────────

/// Generate a 32-byte cryptographically random API key encoded as
/// URL-safe base64 without padding.
pub fn generate_api_key() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Atomic write with `0600` permissions on Unix.
pub fn atomic_write(path: &Path, contents: &str) -> Result<(), ConfigError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    let tmp = dir.join(format!(".config-{}.tmp", std::process::id()));

    let mut f = fs::File::create(&tmp).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
    }

    f.write_all(contents.as_bytes())
        .map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    f.sync_all().map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    fs::rename(&tmp, path).map_err(|source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    Ok(())
}

// ── hot-reload store ─────────────────────────────────────────────────────────

/// Shared, reloadable configuration.  Safe to read from many async tasks
/// (`RwLock`); the mtime-check + parse is serialized via an inner `Mutex`
/// (cheap: one `stat` syscall per read).
pub struct ConfigStore {
    path: Option<PathBuf>,
    cli: Cli,
    env: EnvVars,
    last_mtime: Mutex<Option<SystemTime>>,
    config: RwLock<Config>,
}

impl ConfigStore {
    pub fn new(
        config: Config,
        path: Option<PathBuf>,
        cli: Cli,
        env: EnvVars,
    ) -> std::sync::Arc<Self> {
        let last_mtime = path
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        std::sync::Arc::new(Self {
            path,
            cli,
            env,
            last_mtime: Mutex::new(last_mtime),
            config: RwLock::new(config),
        })
    }

    /// Read the current (possibly hot-reloaded) configuration.
    pub async fn current(&self) -> Config {
        self.check_reload().await;
        self.config.read().await.clone()
    }

    /// If the transport uses HTTP and no API key is present, generate a
    /// random one and persist it to the config file (atomic write,
    /// `0600`).  If the key came from the environment or CLI the file
    /// is never touched.
    pub async fn ensure_api_key(&self) -> Result<(), ConfigError> {
        let cfg = self.config.read().await.clone();
        if !cfg.transport.uses_http() || !cfg.http.api_key.is_empty() {
            return Ok(());
        }

        let key = generate_api_key();
        let mut new_cfg = cfg.clone();
        new_cfg.http.api_key = key.clone();

        if let Some(path) = self.path.clone() {
            let mut file = match FileConfig::load_optional(&path)? {
                Some(f) => f,
                // File does not exist yet — create a minimal one
                None => FileConfig {
                    server: Some(ServerFile {
                        transport: Some(cfg.transport.as_str().into()),
                    }),
                    http: Some(HttpFile::default()),
                },
            };
            if file.http.is_none() {
                file.http = Some(HttpFile::default());
            }
            if let Some(h) = file.http.as_mut() {
                h.api_key = Some(key);
            }
            let text = toml::to_string_pretty(&file)
                .map_err(|source| ConfigError::Serialize { source })?;
            atomic_write(&path, &text)?;
            info!(
                config = %path.display(),
                "generated API key and persisted to config file"
            );
        } else {
            warn!("generated API key but no config path to persist it (in-memory only)");
        }

        *self.config.write().await = new_cfg;
        Ok(())
    }

    /// Apply changes that are safe to hot-reload (currently: `api_key`
    /// and `allowed_origins`).  Bind-address, port, endpoint, and
    /// transport changes require a restart and are logged as warnings.
    async fn apply_reloaded(&self, new_cfg: &Config) {
        let mut guard = self.config.write().await;
        let cur = &*guard;

        let mut applied = new_cfg.clone();
        // Preserve values that cannot change without a restart.
        if new_cfg.transport != cur.transport {
            warn!(
                requested = new_cfg.transport.as_str(),
                current = cur.transport.as_str(),
                "transport change requires a restart; ignoring hot reload"
            );
            applied.transport = cur.transport;
        }
        if new_cfg.http.host != cur.http.host || new_cfg.http.port != cur.http.port {
            warn!("HTTP bind address change requires a restart; ignoring");
            applied.http.host = cur.http.host.clone();
            applied.http.port = cur.http.port;
        }
        if new_cfg.http.endpoint != cur.http.endpoint {
            warn!("endpoint change requires a restart; ignoring");
            applied.http.endpoint = cur.http.endpoint.clone();
        }

        if applied != *cur {
            *guard = applied;
            if let Some(p) = &self.path {
                info!(config = %p.display(), "config reloaded");
            }
        }
    }

    /// Cheap mtime-based hot reload: check file metadata, re-parse only
    /// when modified, keep last-known-good on validation failure.
    pub async fn check_reload(&self) {
        let Some(path) = self.path.as_ref() else {
            return;
        };

        let meta = match fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

        {
            let mut last = self.last_mtime.lock().unwrap();
            if *last == Some(mtime) {
                return;
            }
            *last = Some(mtime);
        }

        match FileConfig::load(path) {
            Ok(file) => match resolve(&self.cli, &self.env, Some(&file)) {
                Ok(resolved) => self.apply_reloaded(&resolved).await,
                Err(e) => {
                    warn!(error = %e, "config reload rejected; keeping last known good config");
                }
            },
            Err(e) => {
                warn!(error = %e, "config reload rejected; keeping last known good config");
            }
        }
    }
}

// ── private helpers ──────────────────────────────────────────────────────────

fn parse_port(s: &str) -> Result<u16, ConfigError> {
    let val: u16 = s.trim().parse().map_err(|_| ConfigError::InvalidPort {
        value: s.to_string(),
    })?;
    validate_port(val)
}

fn validate_port(v: u16) -> Result<u16, ConfigError> {
    if v == 0 {
        Err(ConfigError::InvalidPort { value: "0".into() })
    } else {
        Ok(v)
    }
}

fn validate_endpoint(ep: &str) -> Result<(), ConfigError> {
    if ep.starts_with('/') {
        Ok(())
    } else {
        Err(ConfigError::InvalidEndpoint(ep.to_string()))
    }
}

fn parse_origins(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn default_config() {
        let c = Config::default();
        assert_eq!(c.transport, Transport::Stdio);
        assert_eq!(c.http.host, DEFAULT_HOST);
        assert_eq!(c.http.port, DEFAULT_PORT);
        assert!(c.http.api_key.is_empty());
        assert!(c.http.allowed_origins.is_empty());
        assert_eq!(c.http.endpoint, DEFAULT_ENDPOINT);
    }

    #[test]
    fn transport_from_str_valid() {
        assert_eq!("stdio".parse::<Transport>().unwrap(), Transport::Stdio);
        assert_eq!("http".parse::<Transport>().unwrap(), Transport::Http);
        assert_eq!("both".parse::<Transport>().unwrap(), Transport::Both);
    }

    #[test]
    fn transport_from_str_invalid() {
        assert!("HTTP".parse::<Transport>().is_err());
        assert!("ws".parse::<Transport>().is_err());
    }

    #[test]
    fn parse_valid_toml() {
        let text = r#"
[server]
transport = "http"

[http]
host = "0.0.0.0"
port = 9000
api_key = "key123"
allowed_origins = ["http://localhost:3000", "http://example.com"]
endpoint = "/mcp"
"#;
        let f = FileConfig::parse(text, Path::new("/test.toml")).unwrap();
        let srv = f.server.unwrap();
        assert_eq!(srv.transport.as_deref(), Some("http"));
        let h = f.http.unwrap();
        assert_eq!(h.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(h.port, Some(9000));
        assert_eq!(h.api_key.as_deref(), Some("key123"));
        assert_eq!(
            h.allowed_origins.as_deref(),
            Some(&[
                "http://localhost:3000".to_string(),
                "http://example.com".to_string()
            ])
        );
    }

    #[test]
    fn parse_invalid_toml() {
        let text = "[[not valid toml!!!";
        let err = FileConfig::parse(text, Path::new("/bad.toml"));
        assert!(matches!(err, Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn unknown_fields_ignored() {
        let text = r#"
[server]
transport = "stdio"
bogus = 42
"#;
        let f = FileConfig::parse(text, Path::new("/test.toml")).unwrap();
        assert_eq!(f.server.unwrap().transport.as_deref(), Some("stdio"));
    }

    #[test]
    fn precedence_default_file_env_cli() {
        let file = FileConfig {
            server: Some(ServerFile {
                transport: Some("stdio".into()),
            }),
            http: Some(HttpFile {
                host: Some("0.0.0.0".into()),
                port: Some(9000),
                ..Default::default()
            }),
        };
        let env = EnvVars {
            transport: Some("http".into()),
            http_host: Some("1.2.3.4".into()),
            ..Default::default()
        };
        let cli = Cli {
            transport: Some("both".into()),
            http_host: Some("5.6.7.8".into()),
            ..Default::default()
        };
        let cfg = resolve(&cli, &env, Some(&file)).unwrap();
        // CLI > ENV > FILE > DEFAULT
        assert_eq!(cfg.transport, Transport::Both);
        assert_eq!(cfg.http.host, "5.6.7.8");
        assert_eq!(cfg.http.port, 9000);
    }

    #[test]
    fn precedence_env_over_file() {
        let file = FileConfig {
            server: None,
            http: Some(HttpFile {
                host: Some("10.0.0.1".into()),
                ..Default::default()
            }),
        };
        let env = EnvVars {
            http_host: Some("20.0.0.2".into()),
            ..Default::default()
        };
        let cli = Cli::default();
        let cfg = resolve(&cli, &env, Some(&file)).unwrap();
        assert_eq!(cfg.http.host, "20.0.0.2");
    }

    #[test]
    fn invalid_port_file() {
        let f = FileConfig {
            server: None,
            http: Some(HttpFile {
                port: Some(0),
                ..Default::default()
            }),
        };
        let cli = Cli::default();
        let env = EnvVars::default();
        let err = resolve(&cli, &env, Some(&f));
        assert!(matches!(err, Err(ConfigError::InvalidPort { .. })));
    }

    #[test]
    fn invalid_port_env() {
        let cli = Cli::default();
        let env = EnvVars {
            http_port: Some("0".into()),
            ..Default::default()
        };
        assert!(matches!(
            resolve(&cli, &env, None),
            Err(ConfigError::InvalidPort { .. })
        ));
    }

    #[test]
    fn invalid_port_cli() {
        let cli = Cli {
            http_port: Some("70000".into()),
            ..Default::default()
        };
        let env = EnvVars::default();
        assert!(matches!(
            resolve(&cli, &env, None),
            Err(ConfigError::InvalidPort { .. })
        ));
    }

    #[test]
    fn transport_parse_invalid() {
        let cli = Cli {
            transport: Some("HTTPS".into()),
            ..Default::default()
        };
        let env = EnvVars::default();
        assert!(matches!(
            resolve(&cli, &env, None),
            Err(ConfigError::InvalidTransport(_))
        ));
    }

    #[test]
    fn allowed_origins_parse() {
        let cli = Cli {
            http_allowed_origins: Some("a, b, ,c".into()),
            ..Default::default()
        };
        let env = EnvVars::default();
        let cfg = resolve(&cli, &env, None).unwrap();
        assert_eq!(cfg.http.allowed_origins, vec!["a", "b", "c"]);
    }

    #[test]
    fn api_key_gen_len_and_uniqueness() {
        let k1 = generate_api_key();
        let k2 = generate_api_key();
        assert_eq!(k1.len(), 43);
        assert!(k1
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(k1, k2);
    }

    #[test]
    fn persistence_api_key() {
        let d = tmp();
        let path = d.path().join("config.toml");
        // Create a minimal valid config file without api_key
        fs::write(&path, "[server]\ntransport = \"http\"\n").unwrap();
        let store = ConfigStore::new(
            Config::default(),
            Some(path.clone()),
            Cli::default(),
            EnvVars::default(),
        );
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(store.ensure_api_key())
            .unwrap();
        let cfg = store.config.read().clone();
        assert!(!cfg.http.api_key.is_empty());
        assert_eq!(cfg.http.api_key.len(), 43);
        // Verify file was updated
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("api_key"));
    }

    #[test]
    fn persistence_no_overwrite_when_key_from_env() {
        let d = tmp();
        let path = d.path().join("config.toml");
        fs::write(&path, "[server]\ntransport = \"http\"\n").unwrap();
        let env = EnvVars {
            http_api_key: Some("from-env-key".into()),
            ..Default::default()
        };
        let store = ConfigStore::new(
            Config {
                transport: Transport::Http,
                http: HttpConfig {
                    api_key: "from-env-key".into(),
                    ..Default::default()
                },
            },
            Some(path.clone()),
            Cli::default(),
            env,
        );
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(store.ensure_api_key())
            .unwrap();
        let cfg = store.config.read().clone();
        assert_eq!(cfg.http.api_key, "from-env-key");
        // File unchanged
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("api_key"));
    }

    #[test]
    fn atomic_write_basic() {
        let d = tmp();
        let path = d.path().join("out.txt");
        atomic_write(&path, "hello world").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[test]
    fn atomic_write_permissions() {
        let d = tmp();
        let path = d.path().join("secret.txt");
        atomic_write(&path, "k").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn hot_reload_valid_change() {
        let d = tmp();
        let path = d.path().join("config.toml");
        fs::write(&path, "[http]\napi_key = \"key-one-0123456789abcdef\"\n").unwrap();
        let store = ConfigStore::new(
            Config {
                transport: Transport::Http,
                http: HttpConfig {
                    api_key: "key-one-0123456789abcdef".into(),
                    ..Default::default()
                },
            },
            Some(path.clone()),
            Cli::default(),
            EnvVars::default(),
        );
        let cfg1 = store.current().await;
        assert_eq!(cfg1.http.api_key, "key-one-0123456789abcdef");

        // Write new config with different key
        fs::write(&path, "[http]\napi_key = \"key-two-aaaaaaaaaaaaaaaaaa\"\n").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let cfg2 = store.current().await;
        assert_eq!(cfg2.http.api_key, "key-two-aaaaaaaaaaaaaaaaaa");
    }

    #[tokio::test]
    async fn hot_reload_invalid_keeps_previous() {
        let d = tmp();
        let path = d.path().join("config.toml");
        fs::write(&path, "[http]\napi_key = \"good-key-0123456789abcdef\"\n").unwrap();
        let store = ConfigStore::new(
            Config {
                transport: Transport::Http,
                http: HttpConfig {
                    api_key: "good-key-0123456789abcdef".into(),
                    ..Default::default()
                },
            },
            Some(path.clone()),
            Cli::default(),
            EnvVars::default(),
        );

        // Write invalid TOML
        fs::write(&path, "not valid toml!!!").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let cfg = store.current().await;
        assert_eq!(cfg.http.api_key, "good-key-0123456789abcdef");
    }

    #[test]
    fn resolve_config_path_cli_over_env() {
        let cli = Cli {
            config: Some("/cli/path".into()),
            ..Default::default()
        };
        let env = EnvVars {
            config_path: Some("/env/path".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_config_path(&cli, &env),
            Some(PathBuf::from("/cli/path"))
        );
    }

    #[test]
    fn endpoint_must_start_with_slash() {
        let cli = Cli {
            http_host: Some("127.0.0.1".into()),
            ..Default::default()
        };
        let f = FileConfig {
            server: None,
            http: Some(HttpFile {
                endpoint: Some("mcp".into()),
                ..Default::default()
            }),
        };
        let env = EnvVars::default();
        assert!(matches!(
            resolve(&cli, &env, Some(&f)),
            Err(ConfigError::InvalidEndpoint(_))
        ));
    }
}
