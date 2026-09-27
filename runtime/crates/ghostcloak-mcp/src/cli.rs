//! Command-line interface: all values are `Option` so that config-file
//! and environment values take precedence in the deterministic merge.

use clap::Parser;

#[derive(Debug, Clone, Default, Parser)]
#[command(
    name = "ghostcloak-mcp",
    version,
    about = "Ghostfox MCP server — stdio and/or authenticated streamable HTTP"
)]
pub struct Cli {
    /// Transport mode: stdio | http | both
    #[arg(long, value_name = "stdio|http|both")]
    pub transport: Option<String>,

    /// HTTP bind host as an IP address (default 127.0.0.1).
    #[arg(long, value_name = "IP")]
    pub http_host: Option<String>,

    /// HTTP bind port (default 8787).
    #[arg(long, value_name = "PORT")]
    pub http_port: Option<String>,

    /// API key for Bearer authentication. Generated and persisted to the
    /// config file on first run when the transport uses HTTP and no key was
    /// supplied by any configuration source.
    #[arg(long, value_name = "KEY")]
    pub http_api_key: Option<String>,

    /// Allowed browser Origin headers, comma-separated (empty = no browser
    /// origins accepted; requests without an Origin header still pass).
    #[arg(long, value_name = "ORIGINS")]
    pub http_allowed_origins: Option<String>,

    /// Path to the config file (default ~/.ghostfox/config.toml).
    #[arg(long, value_name = "PATH")]
    pub config: Option<String>,
}
