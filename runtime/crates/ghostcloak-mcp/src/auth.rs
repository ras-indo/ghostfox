//! Bearer-token authentication with constant-time comparison.
//!
//! * Missing / wrong scheme / empty key / wrong key → `401 Unauthorized`
//! * Valid key → pass through to the next layer.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::config::ConfigStore;

/// Parse the Bearer token from the `Authorization` header (case-insensitive
/// scheme).  Returns `None` when the header is absent, has the wrong scheme,
/// or carries an empty key.
pub fn bearer_key(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let key = rest.trim();
    if key.is_empty() {
        return None;
    }
    Some(key)
}

/// Constant-time comparison of two string slices.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Axum middleware: reload config (cheap mtime check), then verify the
/// Bearer token with constant-time comparison.
pub async fn auth_middleware(
    State(store): State<Arc<ConfigStore>>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let cfg = store.current().await;
    let expected = &cfg.http.api_key;
    let provided = bearer_key(&headers);

    let ok = match provided {
        Some(p) if !expected.is_empty() => constant_time_eq(p, expected),
        _ => false,
    };

    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "Unauthorized",
        )
            .into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_equal() {
        assert!(constant_time_eq("abc", "abc"));
    }

    #[test]
    fn constant_time_eq_different() {
        assert!(!constant_time_eq("abc", "abd"));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq("abc", "abcd"));
    }

    #[test]
    fn bearer_key_missing_header() {
        assert!(bearer_key(&HeaderMap::new()).is_none());
    }

    #[test]
    fn bearer_key_wrong_scheme() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Basic dXNlcjpwYXNz".parse().unwrap());
        assert!(bearer_key(&h).is_none());
    }

    #[test]
    fn bearer_key_empty_token() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert!(bearer_key(&h).is_none());
    }

    #[test]
    fn bearer_key_valid() {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            "Bearer my-secret-token".parse().unwrap(),
        );
        assert_eq!(bearer_key(&h), Some("my-secret-token"));
    }

    #[test]
    fn bearer_key_case_insensitive_scheme() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "bearer lower-case".parse().unwrap());
        assert_eq!(bearer_key(&h), Some("lower-case"));
    }
}
