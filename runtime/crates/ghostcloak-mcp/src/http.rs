//! Streamable HTTP MCP transport: Axum router + `rmcp` `StreamableHttpService`
//! with Bearer auth, origin validation, and graceful shutdown.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::middleware;
use axum::Router;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager,
    tower::StreamableHttpService,
};
use rmcp::transport::StreamableHttpServerConfig;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::auth::auth_middleware;
use crate::config::{ConfigStore, HttpConfig};
use crate::origin::origin_middleware;
use crate::server::GhostcloakServer;

/// Parse the host string and port into a [`SocketAddr`].
pub fn bind_addr(host: &str, port: u16) -> Result<SocketAddr, String> {
    let ip: IpAddr = host.parse().map_err(|_| {
        format!(
            "invalid http host `{host}`: expected an IP address \
             (e.g. 127.0.0.1, 0.0.0.0, ::)"
        )
    })?;
    Ok(SocketAddr::new(ip, port))
}

/// Returns `true` when the address is *not* loopback.
pub fn is_non_loopback(addr: &SocketAddr) -> bool {
    !addr.ip().is_loopback()
}

/// Serve the Streamable HTTP MCP endpoint on the provided listener.
pub async fn serve(
    listener: TcpListener,
    store: Arc<ConfigStore>,
    server: GhostcloakServer,
    cancellation: CancellationToken,
) -> std::io::Result<()> {
    let cfg = store.current().await;
    let endpoint = cfg.http.endpoint.clone();

    let mcp_service: StreamableHttpService<GhostcloakServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(server.clone()),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig {
                cancellation_token: cancellation.clone(),
                ..Default::default()
            },
        );

    let app = Router::new()
        .nest_service(endpoint, mcp_service)
        .layer(middleware::from_fn_with_state(
            store.clone(),
            origin_middleware,
        ))
        .layer(middleware::from_fn_with_state(store, auth_middleware));

    axum::serve(listener, app)
        .with_graceful_shutdown(cancellation.cancelled_owned())
        .await
}

/// Standalone HTTP mode: bind, log, wait for shutdown signal.
pub async fn run_http(store: Arc<ConfigStore>, server: GhostcloakServer) -> anyhow::Result<()> {
    let cfg = store.current().await;
    let addr = bind_addr(&cfg.http.host, cfg.http.port).map_err(anyhow::Error::msg)?;

    if is_non_loopback(&addr) {
        warn!(
            http_bind = %addr,
            "binding MCP to a non-loopback address — the endpoint is \
             reachable on the network; authentication is enforced but \
             TLS is NOT provided (use a reverse proxy for HTTPS)"
        );
    }

    let listener = TcpListener::bind(addr).await?;
    info!(
        http_bind = %addr,
        endpoint = %cfg.http.endpoint,
        "HTTP MCP server listening"
    );

    let cancellation = CancellationToken::new();
    tokio::select! {
        result = serve(listener, store, server, cancellation.clone()) => {
            result?;
        }
        _ = crate::signals::shutdown_signal() => {
            cancellation.cancel();
            info!("HTTP server shutting down");
        }
    }
    Ok(())
}
