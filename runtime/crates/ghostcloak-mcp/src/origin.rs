//! Origin validation for Streamable HTTP MCP.
//!
//! * No `Origin` header → pass (non-browser clients).
//! * `Origin` present and in the allowed list → pass.
//! * `Origin` present but not in the allowed list → `403 Forbidden`.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::ConfigStore;

/// Pure helper: check whether the supplied `Origin` is allowed.
/// An absent `Origin` (non-browser) is always accepted.
pub fn origin_allowed(origin: Option<&str>, allowed: &[String]) -> bool {
    match origin {
        None => true,
        Some(o) => allowed.iter().any(|a| a.as_str() == o),
    }
}

/// Axum middleware: reload config (cheap mtime check), then validate the
/// `Origin` header against the configured allowlist.
pub async fn origin_middleware(
    State(store): State<Arc<ConfigStore>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let cfg = store.current().await;
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());

    if origin_allowed(origin, &cfg.http.allowed_origins) {
        next.run(request).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_origin_accepted() {
        assert!(origin_allowed(None, &[]));
        assert!(origin_allowed(None, &["http://localhost:3000".into()]));
    }

    #[test]
    fn allowed_origin_accepted() {
        let allowed = vec!["http://localhost:3000".into()];
        assert!(origin_allowed(Some("http://localhost:3000"), &allowed));
    }

    #[test]
    fn denied_origin_rejected() {
        let allowed = vec!["http://localhost:3000".into()];
        assert!(!origin_allowed(Some("http://evil.example"), &allowed));
    }

    #[test]
    fn empty_allowlist_rejects_browser() {
        assert!(!origin_allowed(Some("http://localhost"), &[]));
    }
}
