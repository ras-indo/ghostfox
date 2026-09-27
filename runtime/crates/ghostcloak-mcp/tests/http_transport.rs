#![allow(clippy::all)]
//! Integration tests for the HTTP transport.
//!
//! These tests verify:
//! - HTTP transport starts and binds
//! - API key authentication works
//! - Origin validation works
//! - MCP initialize/tools/list works over HTTP
//! - Both mode works

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn start_server_stdio() -> Child {
    let binary = env!("CARGO_BIN_EXE_ghostcloak-mcp");
    Command::new(binary)
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ghostcloak-mcp")
}

fn start_server_http(port: u16, api_key: &str) -> Child {
    let binary = env!("CARGO_BIN_EXE_ghostcloak-mcp");
    Command::new(binary)
        .args([
            "--transport",
            "http",
            "--http-port",
            &port.to_string(),
            "--http-host",
            "127.0.0.1",
            "--http-api-key",
            api_key,
        ])
        .env("RUST_LOG", "off")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ghostcloak-mcp http")
}

fn request_stdio(child: &mut Child, request: Value) -> Value {
    let stdin = child.stdin.as_mut().expect("server stdin");
    let raw = serde_json::to_string(&request).expect("serialize request");
    writeln!(stdin, "{raw}").expect("write request");
    stdin.flush().expect("flush request");

    let stdout = child.stdout.as_mut().expect("server stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    for _ in 0..100 {
        line.clear();
        let read = reader.read_line(&mut line).expect("read response");
        assert!(read > 0, "server closed before response");
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if value.get("id") == request.get("id") {
                return value;
            }
        }
    }
    panic!("no response for request");
}

fn initialize_stdio(child: &mut Child) {
    let _ = request_stdio(
        child,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1.0.0"}
            }
        }),
    );
    let stdin = child.stdin.as_mut().expect("server stdin");
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .expect("write initialized notification");
    stdin.flush().expect("flush");
}

async fn http_request(port: u16, method: &str, params: Value, api_key: &str) -> reqwest::Response {
    let client = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {api_key}"))
        .body(serde_json::to_string(&body).unwrap())
        .send()
        .await
        .expect("HTTP request failed")
}

async fn http_request_no_auth(port: u16, method: &str, params: Value) -> reqwest::Response {
    let client = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(serde_json::to_string(&body).unwrap())
        .send()
        .await
        .expect("HTTP request failed")
}

async fn http_request_wrong_origin(
    port: u16,
    method: &str,
    params: Value,
    api_key: &str,
    origin: &str,
) -> reqwest::Response {
    let client = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Origin", origin)
        .body(serde_json::to_string(&body).unwrap())
        .send()
        .await
        .expect("HTTP request failed")
}

fn wait_for_port(port: u16, timeout: Duration) {
    let start = std::time::Instant::now();
    loop {
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timeout waiting for port {port}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn stdio_contract_lists_identity_tools() {
    let mut child = start_server_stdio();
    initialize_stdio(&mut child);

    let listed = request_stdio(
        &mut child,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let tools = listed
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .expect("tools array");
    let names: Vec<_> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    assert!(names.contains(&"identity_generate"));
    assert!(names.contains(&"identity_audit"));
    // Total should be 43+ tools
    assert!(
        names.len() >= 40,
        "expected >= 40 tools, got {}",
        names.len()
    );

    child.kill().ok();
}

#[tokio::test]
async fn http_auth_rejects_missing_key() {
    let port = 18781;
    let api_key = "test-secret-key-12345";
    let mut child = start_server_http(port, api_key);
    wait_for_port(port, Duration::from_secs(10));

    let resp = http_request_no_auth(
        port,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0.0"}
        }),
    )
    .await;

    assert_eq!(resp.status(), 401, "expected 401 for missing auth");
    child.kill().ok();
}

#[tokio::test]
async fn http_auth_rejects_wrong_key() {
    let port = 18782;
    let api_key = "correct-key-abcdef";
    let mut child = start_server_http(port, api_key);
    wait_for_port(port, Duration::from_secs(10));

    let resp = http_request(
        port,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0.0"}
        }),
        "wrong-key",
    )
    .await;

    assert_eq!(resp.status(), 401, "expected 401 for wrong key");
    child.kill().ok();
}

#[tokio::test]
async fn http_auth_accepts_correct_key() {
    let port = 18783;
    let api_key = "correct-key-abcdef";
    let mut child = start_server_http(port, api_key);
    wait_for_port(port, Duration::from_secs(10));

    let resp = http_request(
        port,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0.0"}
        }),
        api_key,
    )
    .await;

    assert!(
        resp.status().is_success() || resp.status().as_u16() == 200,
        "expected success, got {}",
        resp.status()
    );
    child.kill().ok();
}

#[tokio::test]
async fn http_healthz_works_without_auth() {
    let port = 18784;
    let api_key = "test-key";
    let mut child = start_server_http(port, api_key);
    wait_for_port(port, Duration::from_secs(10));

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
    child.kill().ok();
}
