//! API-key authentication and Origin validation middleware for HTTP transport.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

/// Shared HTTP auth configuration (cheaply clonable).
#[derive(Clone)]
pub struct AuthConfig {
    /// The configured API key. Empty means auth is disabled (stdio-only mode).
    pub api_key: Arc<String>,
    /// Allowed Origin values. Empty means any Origin is accepted.
    pub allowed_origins: Arc<Vec<String>>,
}

impl AuthConfig {
    /// Check if this config requires authentication.
    pub fn is_active(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Update from a reloaded config (hot reload).
    pub fn update(&mut self, key: String, origins: Vec<String>) {
        self.api_key = Arc::new(key);
        self.allowed_origins = Arc::new(origins);
    }
}

// ---------------------------------------------------------------------------
// Constant-time API key comparison
// ---------------------------------------------------------------------------

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Verify that the given Bearer token matches the configured API key.
fn verify_api_key(token: &str, configured: &str) -> bool {
    if configured.is_empty() {
        return true; // no key configured = no auth required
    }
    if token.is_empty() {
        return false;
    }
    constant_time_eq(token.as_bytes(), configured.as_bytes())
}

// ---------------------------------------------------------------------------
// Origin validation
// ---------------------------------------------------------------------------

fn validate_origin(headers: &HeaderMap, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true; // no origin restrictions
    }
    match headers.get("origin") {
        None => true, // non-browser clients may omit Origin
        Some(val) => {
            let origin = val.to_str().unwrap_or("");
            allowed.iter().any(|a| a == origin)
        }
    }
}

// ---------------------------------------------------------------------------
// Axum middleware
// ---------------------------------------------------------------------------

/// Tower/axum middleware: checks API-key auth + Origin validation.
pub async fn auth_middleware(
    State(auth): State<AuthConfig>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // --- Origin validation ---
    if !validate_origin(req.headers(), &auth.allowed_origins) {
        tracing::warn!("rejected request: invalid Origin");
        return Err(StatusCode::FORBIDDEN);
    }

    // --- API-key authentication ---
    if auth.is_active() {
        let token = extract_bearer_token(req.headers());
        if !verify_api_key(&token, &auth.api_key) {
            tracing::warn!("rejected request: unauthorized");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    Ok(next.run(req).await)
}

/// Extract the Bearer token from the Authorization header.
fn extract_bearer_token(headers: &HeaderMap) -> String {
    let Some(val) = headers.get("authorization") else {
        return String::new();
    };
    let Ok(val_str) = val.to_str() else {
        return String::new();
    };
    let Some(token) = val_str.strip_prefix("Bearer ") else {
        return String::new();
    };
    token.trim().to_string()
}

/// Build the 401 Unauthorized response with WWW-Authenticate header.
pub fn unauthorized_response() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert("www-authenticate", HeaderValue::from_static("Bearer"));
    (
        StatusCode::UNAUTHORIZED,
        headers,
        r#"{"error":"unauthorized"}"#,
    )
        .into_response()
}

/// Build the 403 Forbidden response.
pub fn forbidden_response() -> Response {
    (StatusCode::FORBIDDEN, r#"{"error":"forbidden"}"#).into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_key_correct() {
        assert!(verify_api_key("secret123", "secret123"));
    }

    #[test]
    fn verify_key_wrong() {
        assert!(!verify_api_key("wrong", "secret123"));
    }

    #[test]
    fn verify_key_empty_token() {
        assert!(!verify_api_key("", "secret123"));
    }

    #[test]
    fn verify_key_empty_config() {
        // empty configured key = auth disabled
        assert!(verify_api_key("anything", ""));
    }

    #[test]
    fn verify_key_different_lengths() {
        assert!(!verify_api_key("abc", "abcdef"));
        assert!(!verify_api_key("abcdef", "abc"));
    }

    #[test]
    fn constant_time_eq_same() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn constant_time_eq_diff() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn constant_time_eq_diff_len() {
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn origin_allowed() {
        let allowed = vec!["http://localhost:3000".into()];
        let mut headers = HeaderMap::new();
        headers.insert("origin", "http://localhost:3000".parse().unwrap());
        assert!(validate_origin(&headers, &allowed));
    }

    #[test]
    fn origin_denied() {
        let allowed = vec!["http://localhost:3000".into()];
        let mut headers = HeaderMap::new();
        headers.insert("origin", "http://evil.com".parse().unwrap());
        assert!(!validate_origin(&headers, &allowed));
    }

    #[test]
    fn origin_no_header_accepted() {
        let allowed = vec!["http://localhost:3000".into()];
        let headers = HeaderMap::new();
        assert!(validate_origin(&headers, &allowed));
    }

    #[test]
    fn origin_empty_list_accepts_all() {
        let allowed: Vec<String> = vec![];
        let mut headers = HeaderMap::new();
        headers.insert("origin", "http://evil.com".parse().unwrap());
        assert!(validate_origin(&headers, &allowed));
    }

    #[test]
    fn extract_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer mytoken".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), "mytoken");
    }

    #[test]
    fn extract_bearer_wrong_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), "");
    }

    #[test]
    fn extract_bearer_missing() {
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer_token(&headers), "");
    }
}
