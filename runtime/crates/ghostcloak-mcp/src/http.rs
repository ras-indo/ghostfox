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
use tokio::sync::RwLock;

use crate::auth::{auth_middleware, AuthConfig};
use crate::config::HttpConfig;
use crate::server::GhostcloakServer;

type SessionManager = rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub struct AppState {
    /// Runtime auth configuration. `RwLock` so hot reload can swap the
    /// API key / allowed origins without rebuilding the router.
    pub auth: Arc<RwLock<AuthConfig>>,
    pub server: GhostcloakServer,
    /// Shared session manager — MUST be created once and shared across all
    /// requests, otherwise each request creates a fresh session store and
    /// `initialize` sessions are lost immediately.
    pub session_manager: Arc<SessionManager>,
}

impl AppState {
    pub fn new(server: GhostcloakServer, http: &HttpConfig) -> Self {
        Self {
            auth: Arc::new(RwLock::new(AuthConfig::from_config(http))),
            server,
            session_manager: Arc::new(SessionManager::default()),
        }
    }

    /// Apply a reloaded config (hot reload). Only credential/origin fields
    /// are swapped; host/port/endpoint still require a restart.
    pub async fn apply_reload(&self, http: &HttpConfig) {
        let mut auth = self.auth.write().await;
        auth.update(http.api_key.clone(), http.allowed_origins.clone());
    }
}

/// Build the Axum router with auth middleware and MCP endpoint.
///
/// Auth is applied ONLY to the MCP endpoint via `route_layer`. `/healthz`
/// must stay unauthenticated (liveness probe; it exposes no internals and
/// calls no MCP tools).
pub fn build_router(state: AppState, endpoint: &str) -> Router {
    let mcp_route = Router::new()
        .route(endpoint, get(mcp_handler).post(mcp_handler))
        .route_layer(middleware::from_fn_with_state(
            state.auth.clone(),
            auth_middleware,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(mcp_route)
        .with_state(state)
}

/// Health check endpoint (liveness, not MCP).
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
        state.session_manager.clone(),
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

/// Start the HTTP server. Returns the router state (for hot reload) and a
/// JoinHandle for graceful shutdown.
pub async fn start_http_server(
    config: &HttpConfig,
    server: GhostcloakServer,
) -> Result<(AppState, tokio::task::JoinHandle<Result<()>>)> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .context("invalid HTTP bind address")?;

    let state = AppState::new(server, config);

    let router = build_router(state.clone(), &config.endpoint);

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

    Ok((state, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn auth_config_active() {
        let active_cfg = Config {
            http: HttpConfig {
                api_key: "key123".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let state = AppState::new(GhostcloakServer::new(), &active_cfg.http);
        assert!(state.auth.read().await.is_active());

        let inactive_cfg = Config::default();
        let state = AppState::new(GhostcloakServer::new(), &inactive_cfg.http);
        assert!(!state.auth.read().await.is_active());
    }

    #[tokio::test]
    async fn reload_swaps_api_key_and_origins() {
        let mut cfg = Config::default();
        cfg.http.api_key = "old-key".into();
        let state = AppState::new(GhostcloakServer::new(), &cfg.http);
        assert_eq!(state.auth.read().await.api_key.as_str(), "old-key");

        cfg.http.api_key = "new-key".into();
        cfg.http.allowed_origins = vec!["http://localhost:3000".into()];
        state.apply_reload(&cfg.http).await;

        let auth = state.auth.read().await;
        assert_eq!(auth.api_key.as_str(), "new-key");
        assert_eq!(auth.allowed_origins.as_slice(), ["http://localhost:3000"]);
    }
}
