//! End-to-end MCP server tests.
//!
//! Boots an in-process [`hyprpilot_daemon`] on a temp socket, then drives a
//! [`hyprpilot_mcp::Server`] against it. Skipped when no live Hyprland is
//! reachable, so cargo test still works on non-Hyprland hosts.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::sleep;

use hyprpilot_core::Instance;
use hyprpilot_mcp::capability::{Profile, ToolGroup};
use hyprpilot_mcp::protocol::{ErrorCode, JsonRpcRequest};
use hyprpilot_mcp::Server;

fn has_hyprland() -> bool {
    Instance::discover().is_ok()
}

fn temp_socket(suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "hyprpilot-mcp-test-{}-{}.sock",
        std::process::id(),
        suffix
    ))
}

/// Boots an in-process daemon on `socket` and returns once it's listening.
async fn start_daemon(socket: PathBuf) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    let socket_clone = socket.clone();
    let handle = tokio::spawn(async move { hyprpilot_daemon::server::run(socket_clone).await });
    // Wait up to 2s for the socket to appear.
    for _ in 0..40 {
        if socket.exists() {
            return handle;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon failed to start listening on {}", socket.display());
}

fn req(id: u64, method: &str, params: Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(id)),
        method: method.to_string(),
        params: Some(params),
    }
}

#[tokio::test]
async fn handshake_returns_protocol_version_and_tool_capability() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("handshake");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::unrestricted(), socket.clone());
    let resp = server
        .handle(req(
            1,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        ))
        .await
        .expect("initialize must respond");
    let result = resp.result.expect("ok response");
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(result["serverInfo"]["name"], "hyprpilot-mcp");

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn tools_list_is_filtered_by_default_profile() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("list-default");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::default_safe(), socket.clone());
    let resp = server
        .handle(req(2, "tools/list", json!({})))
        .await
        .expect("response");
    let tools = resp.result.unwrap()["tools"]
        .as_array()
        .expect("tools array")
        .clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"query_clients"));
    assert!(names.contains(&"focus_window"));
    assert!(!names.contains(&"kill_active"), "default profile must hide kill_active");
    assert!(!names.contains(&"close_window"), "default profile must hide close_window");
    assert!(!names.contains(&"exec"), "default profile must hide exec");

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn tools_list_with_unrestricted_profile_shows_all() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("list-all");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::unrestricted(), socket.clone());
    let resp = server.handle(req(3, "tools/list", json!({}))).await.unwrap();
    let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"kill_active"));
    assert!(names.contains(&"exec"));
    assert!(names.contains(&"undo"));
    // Sanity: every tool has a non-empty description and an object schema.
    for t in &tools {
        assert!(t["description"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(t["inputSchema"].is_object(), "schema must be an object");
    }

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn query_version_through_full_stack() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("query");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::default_safe(), socket.clone());
    let resp = server
        .handle(req(
            4,
            "tools/call",
            json!({"name": "query_version", "arguments": {}}),
        ))
        .await
        .unwrap();
    assert!(resp.error.is_none(), "error: {:?}", resp.error);
    let result = resp.result.expect("ok");
    let text = result["content"][0]["text"]
        .as_str()
        .expect("text content");
    assert!(text.contains("\"version\""), "expected version field in: {text}");

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn kill_active_default_returns_dry_run_preview() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("dryrun");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::unrestricted(), socket.clone());
    let resp = server
        .handle(req(
            5,
            "tools/call",
            json!({"name": "kill_active", "arguments": {}}),
        ))
        .await
        .unwrap();
    assert!(resp.error.is_none(), "error: {:?}", resp.error);
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("[dry_run]"), "expected dry_run preview, got: {text}");
    assert_eq!(result["isError"].as_bool().unwrap_or(false), false);

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn capability_denied_on_destructive_with_default_profile() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("denied");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::default_safe(), socket.clone());
    let resp = server
        .handle(req(
            6,
            "tools/call",
            json!({"name": "kill_active", "arguments": {"dry_run": false}}),
        ))
        .await
        .unwrap();
    let error = resp.error.expect("expected JSON-RPC error");
    assert_eq!(error.code, ErrorCode::CapabilityDenied.code());
    assert!(error.message.contains("kill_active"));

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn invalid_args_returns_invalid_params() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("badargs");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::unrestricted(), socket.clone());
    let resp = server
        .handle(req(
            7,
            "tools/call",
            json!({"name": "move_to_workspace", "arguments": {"target": "not_a_ws"}}),
        ))
        .await
        .unwrap();
    let error = resp.error.expect("expected JSON-RPC error");
    assert_eq!(error.code, ErrorCode::InvalidParams.code());

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn group_assignment_is_consistent() {
    // Doesn't need Hyprland; pure registry check.
    let reg = hyprpilot_mcp::tools::registry();
    for t in &reg {
        // Group-name mapping must be valid (i.e. not stale enum drift).
        let _ = t.group.name();
    }
    // Spot-check group assignments that matter for safety.
    let by_name = |n: &str| reg.iter().find(|t| t.name == n).unwrap();
    assert_eq!(by_name("query_clients").group, ToolGroup::Read);
    assert_eq!(by_name("kill_active").group, ToolGroup::Destructive);
    assert_eq!(by_name("exec").group, ToolGroup::Process);
    assert_eq!(by_name("undo").group, ToolGroup::Undo);
}
