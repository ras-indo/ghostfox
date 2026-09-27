//! HTTP transport layer: Axum server wrapping rmcp's StreamableHttpService.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use http_body_util::BodyExt;

use crate::auth::{auth_middleware, AuthConfig};
use crate::config::HttpConfig;
use crate::server::GhostcloakServer;

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub struct AppState {
    pub auth: AuthConfig,
    pub server: GhostcloakServer,
}

/// Build the Axum router with auth middleware and MCP endpoint.
pub fn build_router(state: AppState, endpoint: &str) -> Router {
    Router::new()
        .route(endpoint, get(mcp_handler).post(mcp_handler))
        .route("/healthz", get(healthz))
        .layer(middleware::from_fn_with_state(
            state.auth.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// Health check endpoint.
async fn healthz() -> impl IntoResponse {
    StatusCode::OK
}

/// MCP handler: delegates to StreamableHttpService::handle().
///
/// Converts between axum body types and rmcp body types.
async fn mcp_handler(State(state): State<AppState>, req: Request<Body>) -> impl IntoResponse {
    // Convert axum Request<Body> to rmcp-compatible Request
    let (parts, body) = req.into_parts();
    let body = body.map_err(|e| std::io::Error::other(e));
    let rmcp_req = Request::from_parts(parts, body);

    // Create the MCP service for this request.
    // GhostcloakServer is Clone with Arc-based internal state, so cloning
    // is cheap and shares the same session storage.
    let server = state.server.clone();
    let service = rmcp::transport::streamable_http_server::tower::StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        ),
        rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig::default(),
    );

    // Handle the request via the MCP service
    let response = service.handle(rmcp_req).await;

    // Convert response body from UnsyncBoxBody<Bytes, Infallible> to axum body
    let (parts, body) = response.into_parts();
    let body = body.map_err(|e| -> std::io::Error {
        match e {} // Infallible: this branch never executes
    });
    let body = axum::body::Body::new(body);
    axum::http::Response::from_parts(parts, body)
}

/// Start the HTTP server. Returns a JoinHandle for graceful shutdown.
pub async fn start_http_server(
    config: &HttpConfig,
    server: GhostcloakServer,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .context("invalid HTTP bind address")?;

    let auth = AuthConfig {
        api_key: Arc::new(config.api_key.clone()),
        allowed_origins: Arc::new(config.allowed_origins.clone()),
    };

    let state = AppState { auth, server };

    let router = build_router(state, &config.endpoint);

    tracing::info!(
        "HTTP MCP server listening on {}/{}",
        addr,
        config.endpoint.trim_start_matches('/')
    );

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("failed to bind HTTP server")?;

    let handle = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .context("HTTP server error")?;
        Ok(())
    });

    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_config_active() {
        let active = AuthConfig {
            api_key: Arc::new("key123".into()),
            allowed_origins: Arc::new(vec![]),
        };
        assert!(active.is_active());

        let inactive = AuthConfig {
            api_key: Arc::new(String::new()),
            allowed_origins: Arc::new(vec![]),
        };
        assert!(!inactive.is_active());
    }
}
