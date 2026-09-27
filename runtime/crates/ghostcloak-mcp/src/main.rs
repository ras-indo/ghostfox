//! ghostcloak-mcp: exposes the browser runtime to AI agents over MCP
//! (stdio and/or authenticated streamable HTTP).

use clap::Parser;
use rmcp::service::serve_server;
use rmcp::transport::stdio;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

mod auth;
mod captcha;
mod cli;
mod config;
mod ddddocr;
mod geetest;
mod hcaptcha;
mod http;
mod liveview;
mod ocr;
mod origin;
mod recording;
mod server;
mod signals;

use config::{ConfigStore, Transport};
use server::GhostcloakServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = cli::Cli::parse();
    let env = config::EnvVars::collect();
    let config_path = config::resolve_config_path(&cli, &env);

    let file = match &config_path {
        Some(path) => config::FileConfig::load_optional(path)?,
        None => None,
    };

    let cfg = config::resolve(&cli, &env, file.as_ref())?;
    let store = ConfigStore::new(cfg, config_path.clone(), cli, env);
    store.ensure_api_key().await?;

    if let Some(path) = &config_path {
        info!(config = %path.display(), "config loaded");
    }

    let transport = store.current().await.transport;
    info!(transport = %transport.as_str(), "ghostcloak-mcp starting");

    let server = GhostcloakServer::new();

    match transport {
        Transport::Stdio => run_stdio(server).await,
        Transport::Http => http::run_http(store, server).await,
        Transport::Both => run_both(store, server).await,
    }
}

async fn run_stdio(server: GhostcloakServer) -> anyhow::Result<()> {
    let service = serve_server(server, stdio()).await?;
    tokio::select! {
        result = service.waiting() => { result?; }
        _ = signals::shutdown_signal() => {
            info!("stdio server shutting down");
        }
    }
    Ok(())
}

async fn run_both(
    store: Arc<ConfigStore>,
    server: GhostcloakServer,
) -> anyhow::Result<()> {
    let cfg = store.current().await;
    let addr = http::bind_addr(&cfg.http.host, cfg.http.port)
        .map_err(anyhow::Error::msg)?;

    if http::is_non_loopback(&addr) {
        warn!(
            http_bind = %addr,
            "binding HTTP MCP to a non-loopback address; \
             authentication is enforced, TLS is not provided"
        );
    }

    let listener = TcpListener::bind(addr).await?;
    info!(
        http_bind = %addr,
        endpoint = %cfg.http.endpoint,
        "HTTP MCP server listening (both mode)"
    );

    let cancellation = CancellationToken::new();
    let http_task = tokio::spawn({
        let store = store.clone();
        let server = server.clone();
        let ct = cancellation.clone();
        async move { http::serve(listener, store, server, ct).await }
    });

    let stdio_task = async {
        let service = serve_server(server, stdio()).await?;
        service.waiting().await?;
        anyhow::Result::<()>::Ok(())
    };

    tokio::select! {
        result = stdio_task => {
            if let Err(e) = result {
                warn!(error = %e, "stdio server exited with error");
            } else {
                info!("stdio server exited (stdin closed)");
            }
            cancellation.cancel();
            let _ = http_task.await;
        }
        _ = signals::shutdown_signal() => {
            cancellation.cancel();
            let _ = http_task.await;
            info!("shutdown complete");
        }
    }
    Ok(())
}
