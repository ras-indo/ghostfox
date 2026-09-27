//! ghostcloak-mcp: exposes the browser runtime to AI agents over MCP
//! (stdio, Streamable HTTP, or both).
//!
//! This binary is intentionally thin: all logic lives in the library
//! crate (`ghostcloak_mcp`) so the same code paths serve stdio, HTTP,
//! and "both" modes without duplicate module compilation.

use anyhow::Result;
use clap::Parser;
use ghostcloak_mcp::config::{CliArgs, Config, ReloadTracker, Transport};
use ghostcloak_mcp::server::GhostcloakServer;
use ghostcloak_mcp::{config, http};
use rmcp::service::serve_server;
use rmcp::transport::stdio;

#[derive(Parser, Debug)]
#[command(
    name = "ghostcloak-mcp",
    about = "MCP server for the Ghostfox anti-detect browser"
)]
struct Cli {
    /// Transport mode: stdio, http, or both.
    #[arg(long)]
    transport: Option<Transport>,

    /// HTTP host to bind to.
    #[arg(long)]
    http_host: Option<String>,

    /// HTTP port to bind to.
    #[arg(long)]
    http_port: Option<u16>,

    /// HTTP MCP endpoint path.
    #[arg(long)]
    http_endpoint: Option<String>,

    /// API key for HTTP authentication.
    #[arg(long)]
    http_api_key: Option<String>,

    /// Path to config file (default: ~/.ghostfox/config.toml).
    #[arg(long)]
    config: Option<String>,
}

impl Cli {
    fn into_args(self) -> CliArgs {
        CliArgs {
            config: self.config,
            transport: self.transport,
            http_host: self.http_host,
            http_port: self.http_port,
            http_endpoint: self.http_endpoint,
            http_api_key: self.http_api_key,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing: always to stderr so stdout stays clean for MCP stdio.
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    // Parse CLI
    let cli = Cli::parse();

    // Load config: DEFAULT < FILE < ENV
    let mut cfg = if let Some(ref path) = cli.config {
        std::env::set_var("GHOSTFOX_CONFIG", path);
        Config::load()?
    } else {
        Config::load()?
    };

    // Apply CLI overrides (CLI > ENV > FILE > DEFAULT)
    let args = cli.into_args();
    args.apply(&mut cfg);

    // Ensure API key for HTTP modes
    cfg = config::ensure_api_key(cfg)?;

    tracing::info!("transport={}", cfg.server.transport);

    match cfg.server.transport {
        Transport::Stdio => run_stdio().await,
        Transport::Http => run_http(cfg).await?,
        Transport::Both => run_both(cfg).await?,
    }

    Ok(())
}

/// Run MCP over stdio only (default, backward-compatible).
async fn run_stdio() {
    tracing::info!("stdio MCP server starting");
    let service = serve_server(GhostcloakServer::new(), stdio())
        .await
        .expect("failed to start stdio MCP server");
    service.waiting().await.expect("stdio server error");
}

/// Run MCP over HTTP only.
async fn run_http(cfg: Config) -> Result<()> {
    let server = GhostcloakServer::new();
    let mut tracker = ReloadTracker::new(cfg.clone());

    let (state, mut handle) = http::start_http_server(&cfg.http, server).await?;

    // Wait for shutdown signal, checking config reload periodically.
    let reload_interval = std::time::Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal");
                break;
            }
            _ = tokio::time::sleep(reload_interval) => {
                let (reloaded, new_cfg) = tracker.check_reload();
                if reloaded {
                    state.apply_reload(&new_cfg.http).await;
                    tracing::info!(
                        "HTTP auth config reloaded (api_key={}, allowed_origins={:?})",
                        !new_cfg.http.api_key.is_empty(),
                        new_cfg.http.allowed_origins
                    );
                }
            }
            result = &mut handle => {
                match result {
                    Ok(Ok(())) => tracing::info!("HTTP server exited cleanly"),
                    Ok(Err(e)) => tracing::error!("HTTP server error: {e}"),
                    Err(e) => tracing::error!("HTTP server task panicked: {e}"),
                }
                break;
            }
        }
    }

    Ok(())
}

/// Run MCP over both stdio and HTTP concurrently.
async fn run_both(cfg: Config) -> Result<()> {
    let server = GhostcloakServer::new();
    let mut tracker = ReloadTracker::new(cfg.clone());

    tracing::info!("starting both stdio + HTTP MCP transports");

    // Start HTTP server
    let (state, mut http_handle) = http::start_http_server(&cfg.http, server.clone()).await?;

    // Start stdio server in a separate task
    let stdio_server = GhostcloakServer::new();
    let mut stdio_handle = tokio::spawn(async move {
        let service = serve_server(stdio_server, stdio())
            .await
            .expect("failed to start stdio MCP server in both mode");
        service
            .waiting()
            .await
            .expect("stdio server error in both mode");
    });

    // Wait for shutdown
    let reload_interval = std::time::Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received shutdown signal (both mode)");
                break;
            }
            _ = tokio::time::sleep(reload_interval) => {
                let (reloaded, new_cfg) = tracker.check_reload();
                if reloaded {
                    state.apply_reload(&new_cfg.http).await;
                    tracing::info!("HTTP auth config reloaded (both mode)");
                }
            }
            result = &mut http_handle => {
                match result {
                    Ok(Ok(())) => tracing::info!("HTTP server exited"),
                    Ok(Err(e)) => tracing::error!("HTTP server error: {e}"),
                    Err(e) => tracing::error!("HTTP server task panicked: {e}"),
                }
                break;
            }
            _ = &mut stdio_handle => {
                tracing::info!("stdio server exited");
                break;
            }
        }
    }

    // Graceful shutdown
    http_handle.abort();
    stdio_handle.abort();

    Ok(())
}
