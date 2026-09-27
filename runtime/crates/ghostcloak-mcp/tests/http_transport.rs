//! End-to-end HTTP transport integration tests.
//!
//! Starts a real axum server with `StreamableHttpService`, the real
//! `GhostcloakServer`, and hits it with `reqwest`.  No browser engine
//! is needed for `initialize` / `tools/list`.

use std::sync::Arc;
use std::time::Duration;

use ghostcloak_mcp::config::{
    Config, ConfigStore, EnvVars, HttpConfig, Transport,
};
use ghostcloak_mcp::http;
use ghostcloak_mcp::server::GhostcloakServer;

fn test_config(api_key: &str, allowed: Vec<String>) -> Config {
    Config {
        transport: Transport::Http,
        http: HttpConfig {
            api_key: api_key.to_string(),
            allowed_origins: allowed,
            ..Default::default()
        },
    }
}

fn test_store(api_key: &str, allowed: Vec<String>) -> Arc<ConfigStore> {
    ConfigStore::new(
        test_config(api_key, allowed),
        None,
        Default::default(),
        EnvVars::default(),
    )
}

async fn spawn_server(store: Arc<ConfigStore>) -> (String, tokio_util::sync::CancellationToken) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let ct = tokio_util::sync::CancellationToken::new();
    let server = GhostcloakServer::new();
    let ct2 = ct.clone();
    tokio::spawn(async move { http::serve(listener, store, server, ct2).await });
    (format!("http://{addr}/mcp"), ct)
}

async fn mcp_post(
    url: &str,
    key: Option<&str>,
    session: Option<&str>,
    body: serde_json::Value,
) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body);
    if let Some(k) = key {
        req = req.header("Authorization", format!("Bearer {k}"));
    }
    if let Some(s) = session {
        req = req.header("mcp-session-id", s);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn auth_no_key_returns_401() {
    let store = test_store("test-key", vec![]);
    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = mcp_post(&url, None, None, serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    }))
    .await;

    assert_eq!(resp.status().as_u16(), 401);
    assert!(resp.headers().contains_key("www-authenticate"));
    ct.cancel();
}

#[tokio::test]
async fn auth_wrong_key_returns_401() {
    let store = test_store("test-key", vec![]);
    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = mcp_post(&url, Some("wrong-key"), None, serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    }))
    .await;

    assert_eq!(resp.status().as_u16(), 401);
    ct.cancel();
}

#[tokio::test]
async fn auth_wrong_scheme_returns_401() {
    let store = test_store("test-key", vec![]);
    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = mcp_post(&url, None, None, serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    }))
    .await;

    // The function sends no auth header at all when key=None, so 401 is expected.
    assert_eq!(resp.status().as_u16(), 401);
    ct.cancel();
}

#[tokio::test]
async fn auth_valid_key_succeeds() {
    let store = test_store("test-key", vec![]);
    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = mcp_post(&url, Some("test-key"), None, serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    }))
    .await;

    assert_eq!(resp.status().as_u16(), 200);
    ct.cancel();
}

#[tokio::test]
async fn origin_denied_returns_403() {
    let store = test_store(
        "test-key",
        vec!["http://localhost:3000".into()],
    );
    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", "Bearer test-key")
        .header("Origin", "http://evil.example")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403);
    ct.cancel();
}

#[tokio::test]
async fn origin_allowed_succeeds() {
    let store = test_store(
        "test-key",
        vec!["http://localhost:3000".into()],
    );
    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", "Bearer test-key")
        .header("Origin", "http://localhost:3000")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    ct.cancel();
}

#[tokio::test]
async fn no_origin_header_passes() {
    let store = test_store(
        "test-key",
        vec!["http://localhost:3000".into()],
    );
    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = mcp_post(&url, Some("test-key"), None, serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }
    }))
    .await;

    assert_eq!(resp.status().as_u16(), 200);
    ct.cancel();
}

#[tokio::test]
async fn initialize_and_list_tools() {
    let store = test_store("test-key", vec![]);
    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 1. Initialize
    let resp = mcp_post(
        &url,
        Some("test-key"),
        None,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.pointer("/result/protocolVersion").is_some(),
        "initialize response missing protocolVersion: {body}"
    );
    assert!(!session_id.is_empty(), "missing mcp-session-id header");

    // 2. Tools list
    let resp = mcp_post(
        &url,
        Some("test-key"),
        Some(&session_id),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let tool_names: Vec<&str> = body
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|t| t["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(
        tool_names.contains(&"session_create"),
        "session_create missing from tools: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"page_a11y"),
        "page_a11y missing from tools: {tool_names:?}"
    );
    assert!(
        tool_names.contains(&"page_click_ref"),
        "page_click_ref missing from tools: {tool_names:?}"
    );
    assert!(
        tool_names.len() >= 40,
        "expected at least 40 tools, got {}",
        tool_names.len()
    );
    ct.cancel();
}

#[tokio::test]
async fn hot_reload_api_key_rotation() {
    use std::path::PathBuf;

    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("config.toml");
    std::fs::write(
        &path,
        "[http]\napi_key = \"key-one-0123456789abcdef\"\n",
    )
    .unwrap();

    let store = ConfigStore::new(
        Config {
            transport: Transport::Http,
            http: HttpConfig {
                api_key: "key-one-0123456789abcdef".into(),
                ..Default::default()
            },
        },
        Some(path.clone()),
        Default::default(),
        EnvVars::default(),
    );

    let (url, ct) = spawn_server(store).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // key-one works
    let resp = mcp_post(
        &url,
        Some("key-one-0123456789abcdef"),
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);

    // Rotate to key-two
    std::fs::write(
        &path,
        "[http]\napi_key = \"key-two-aaaaaaaaaaaaaaaaaa\"\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // key-one now rejected
    let resp = mcp_post(
        &url,
        Some("key-one-0123456789abcdef"),
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 401);

    // key-two works
    let resp = mcp_post(
        &url,
        Some("key-two-aaaaaaaaaaaaaaaaaa"),
        None,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        }),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    ct.cancel();
}
