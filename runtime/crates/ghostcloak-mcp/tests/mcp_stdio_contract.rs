use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};

fn start_server() -> Child {
    let binary = env!("CARGO_BIN_EXE_ghostcloak-mcp");
    Command::new(binary)
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ghostcloak-mcp")
}

fn request(child: &mut Child, request: Value) -> Value {
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
    panic!("did not receive response for request {request}");
}

fn initialize(child: &mut Child) {
    let _ = request(
        child,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "contract-test", "version": "1.0.0"}
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
    stdin.flush().expect("flush initialized notification");
}

#[test]
fn mcp_stdio_contract_lists_and_calls_identity_tools() {
    let mut child = start_server();
    initialize(&mut child);

    let listed = request(
        &mut child,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    );
    let result = listed.get("result").expect("tools/list result");
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");
    let names: Vec<_> = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    for expected in [
        "session_create",
        "page_open",
        "page_a11y",
        "page_snapshot",
        "page_screenshot",
        "page_read_ref",
        "page_wait_for",
        "page_click_ref",
        "page_type_ref",
        "page_click",
        "page_type",
        "page_fill",
        "page_press",
        "page_upload_file",
        "page_eval",
        "identity_generate",
        "identity_audit",
        "session_evidence",
        "page_fill",
        "page_ocr",
        "page_console",
        "page_errors",
        "page_network_start",
        "page_network_read",
        "page_network_body",
        "page_contrast",
        "page_pixels",
        "page_match_image",
        "page_move_to",
        "page_drag",
        "page_dismiss_modal",
        "page_init_script",
        "page_comment",
        "session_me",
        "session_pages",
        "session_close",
        "session_list",
        "page_back",
        "page_forward",
        "page_reload",
        "page_close",
        "page_extract",
        "page_markdown",
        "page_batch",
        "page_wait_for_idle",
        "page_wait_for_timeout",
        "extract_tokens",
        "detect_anti_bot",
        "page_storage",
        "page_http",
        "export_har",
        "page_scroll",
        "page_cookies",
        "page_permissions",
        "page_download",
        "page_dialog",
        "page_emulate",
        "page_search",
        "page_query",
        "page_dropdown",
        "page_scroll_to_text",
        "page_fill_form",
        "page_get_html",
        "page_hit_test",
        "browser_exec",
        "page_wait_for_text",
        "page_wait_rate_limit",
        "page_clipboard",
        "page_video",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    // Guard against accidental tool loss: this is the exact count currently
    // exposed by GhostcloakServer. If a tool is ever removed (or added),
    // update this list deliberately — never silently drop tools.
    // Batch 1 (9) + batch 2 (4) + dialog (1) + emulate (1) + research-4 (4) + fill_form (1) +
    // research-5 (2) + exec (2: browser_exec, page_wait_for_text) +
    // rate-limit (1: page_wait_rate_limit) +
    // clipboard/video (2: page_clipboard, page_video) +
    // sleep (1: page_wait_for_timeout) = 76, MINUS removed captcha/ML solvers
    // (7: captcha_solve, page_geetest_click, page_geetest_slide,
    // page_captcha_rotate, page_captcha_ocr, page_hcaptcha, page_vision)
    // = 69 — captcha solving and on-device model inference were removed from
    // the gateway (screenshot + generic vision/pixel tools remain).
    assert_eq!(names.len(), 69, "tools: {names:?}");

    let generated = request(
        &mut child,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "identity_generate", "arguments": {}}
        }),
    );
    let generated_result = generated.get("result").expect("identity_generate result");
    assert_eq!(generated_result.get("isError"), Some(&json!(false)));
    let text = generated_result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .expect("identity TOML");
    assert!(text.contains("platform ="));

    let audited = request(
        &mut child,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {"name": "identity_audit", "arguments": {"identity_toml": text}}
        }),
    );
    let audit_text = audited
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .expect("audit result");
    assert_eq!(audit_text, "clean: no violations");

    let unknown = request(
        &mut child,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {"name": "not_a_tool", "arguments": {}}
        }),
    );
    assert!(
        unknown.get("error").is_some(),
        "unknown tool response: {unknown}"
    );

    let _ = child.kill();
    let _ = child.wait();
}
