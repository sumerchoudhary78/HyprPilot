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
use std::sync::{Mutex, OnceLock};

use hyprpilot_mcp::capability::{Profile, ToolGroup};

/// Tests that mutate `XDG_STATE_HOME` must be serialized — env vars are
/// process-global. Held only while a test's body is dirtying the env.
fn env_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}
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
async fn snapshot_save_list_diff_delete_round_trip() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let _env_guard = env_lock().lock().unwrap();
    // Isolate snapshot storage so this test doesn't see / touch the user's
    // real snapshots.
    let state_dir = std::env::temp_dir().join(format!(
        "hyprpilot-snapshot-test-{}-rt",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&state_dir);
    std::fs::create_dir_all(&state_dir).unwrap();
    std::env::set_var("XDG_STATE_HOME", &state_dir);

    let socket = temp_socket("snapshot");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::default_safe(), socket.clone());

    // Save under a unique name.
    let snap_name = format!("e2e-{}", std::process::id());
    let save_resp = server
        .handle(req(
            10,
            "tools/call",
            json!({
                "name": "snapshot_save",
                "arguments": { "name": snap_name }
            }),
        ))
        .await
        .unwrap();
    assert!(save_resp.error.is_none(), "save error: {:?}", save_resp.error);
    let save_text = save_resp.result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
    assert!(save_text.contains(&snap_name));

    // List shows it.
    let list_resp = server
        .handle(req(
            11,
            "tools/call",
            json!({"name": "snapshot_list", "arguments": {}}),
        ))
        .await
        .unwrap();
    let list_text = list_resp.result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
    assert!(
        list_text.contains(&snap_name),
        "list missing snapshot: {list_text}"
    );

    // Diff against itself is a no-op (no mutations).
    let diff_resp = server
        .handle(req(
            12,
            "tools/call",
            json!({"name": "snapshot_diff", "arguments": {"name": &snap_name}}),
        ))
        .await
        .unwrap();
    assert!(diff_resp.error.is_none());
    let diff_text = diff_resp.result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
    // The JSON in the text should contain an `actions` array; mutation_count
    // is not in the serialization, but no MoveToWorkspace / ToggleFloating
    // actions should appear since live == snapshot at this moment.
    assert!(!diff_text.contains("move_to_workspace"));
    assert!(!diff_text.contains("toggle_floating"));

    // Delete.
    let del_resp = server
        .handle(req(
            13,
            "tools/call",
            json!({"name": "snapshot_delete", "arguments": {"name": &snap_name}}),
        ))
        .await
        .unwrap();
    assert!(del_resp.error.is_none());

    // List no longer shows it.
    let list2 = server
        .handle(req(
            14,
            "tools/call",
            json!({"name": "snapshot_list", "arguments": {}}),
        ))
        .await
        .unwrap();
    let list2_text = list2.result.unwrap()["content"][0]["text"].as_str().unwrap().to_string();
    assert!(!list2_text.contains(&snap_name), "delete didn't take effect: {list2_text}");

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&state_dir);
    std::env::remove_var("XDG_STATE_HOME");
}

#[tokio::test]
async fn snapshot_invalid_name_rejected() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("snap-bad");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::default_safe(), socket.clone());
    let resp = server
        .handle(req(
            20,
            "tools/call",
            json!({"name": "snapshot_save", "arguments": {"name": "../oops"}}),
        ))
        .await
        .unwrap();
    // snapshot_invalid_name is agent-input validation: surfaces as
    // JSON-RPC InvalidParams, same as MCP-layer arg validation.
    let error = resp.error.expect("expected JSON-RPC error");
    assert_eq!(error.code, ErrorCode::InvalidParams.code());
    assert!(error.message.contains("snapshot_invalid_name"));

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn snapshot_not_found_returns_invalid_params() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let socket = temp_socket("not-found");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;
    let mut server = Server::new(Profile::default_safe(), socket.clone());
    let resp = server
        .handle(req(
            30,
            "tools/call",
            json!({"name": "snapshot_diff", "arguments": {"name": "absolutely-does-not-exist"}}),
        ))
        .await
        .unwrap();
    let error = resp.error.expect("expected JSON-RPC error");
    assert_eq!(error.code, ErrorCode::InvalidParams.code());
    assert!(error.message.contains("snapshot_not_found"));
    daemon.abort();
    let _ = std::fs::remove_file(&socket);
}

#[tokio::test]
async fn snapshot_restore_dry_run_includes_diff_summary() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let _env_guard = env_lock().lock().unwrap();
    let state_dir = std::env::temp_dir().join(format!(
        "hyprpilot-snapshot-test-{}-restore-dry",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&state_dir);
    std::fs::create_dir_all(&state_dir).unwrap();
    std::env::set_var("XDG_STATE_HOME", &state_dir);

    let socket = temp_socket("snap-restore-dry");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::default_safe(), socket.clone());

    let snap_name = format!("e2e-restore-dry-{}", std::process::id());
    let save_resp = server
        .handle(req(
            40,
            "tools/call",
            json!({"name": "snapshot_save", "arguments": {"name": &snap_name}}),
        ))
        .await
        .unwrap();
    assert!(save_resp.error.is_none(), "save error: {:?}", save_resp.error);

    // Dry-run restore against the just-saved snapshot. Since live ==
    // snapshot, mutation_count should be 0 and the preview should say so.
    let restore_resp = server
        .handle(req(
            41,
            "tools/call",
            json!({"name": "snapshot_restore", "arguments": {"name": &snap_name}}),
        ))
        .await
        .unwrap();
    assert!(
        restore_resp.error.is_none(),
        "dry-run restore should not raise a JSON-RPC error: {:?}",
        restore_resp.error
    );
    let result = restore_resp.result.expect("ok");
    assert_eq!(result["isError"].as_bool().unwrap_or(false), false);
    let text = result["content"][0]["text"].as_str().expect("text content");
    assert!(text.starts_with("[dry_run]"), "expected dry_run preview, got: {text}");
    assert!(
        text.contains("would restore snapshot"),
        "preview missing action phrase: {text}"
    );
    assert!(text.contains(&snap_name), "preview missing snapshot name: {text}");
    assert!(
        text.contains("(no changes"),
        "self-restore should report no changes, got: {text}"
    );
    assert!(
        text.contains("Pass `dry_run: false` to apply."),
        "preview missing apply hint: {text}"
    );

    // Clean up.
    let del_resp = server
        .handle(req(
            42,
            "tools/call",
            json!({"name": "snapshot_delete", "arguments": {"name": &snap_name}}),
        ))
        .await
        .unwrap();
    assert!(del_resp.error.is_none());

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&state_dir);
    std::env::remove_var("XDG_STATE_HOME");
}

#[tokio::test]
async fn snapshot_restore_dry_run_missing_snapshot_returns_invalid_params() {
    if !has_hyprland() {
        eprintln!("skip: no Hyprland");
        return;
    }
    let _env_guard = env_lock().lock().unwrap();
    let state_dir = std::env::temp_dir().join(format!(
        "hyprpilot-snapshot-test-{}-restore-missing",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&state_dir);
    std::fs::create_dir_all(&state_dir).unwrap();
    std::env::set_var("XDG_STATE_HOME", &state_dir);

    let socket = temp_socket("snap-restore-missing");
    let _ = std::fs::remove_file(&socket);
    let daemon = start_daemon(socket.clone()).await;

    let mut server = Server::new(Profile::default_safe(), socket.clone());
    let resp = server
        .handle(req(
            50,
            "tools/call",
            json!({
                "name": "snapshot_restore",
                "arguments": {"name": "definitely-not-a-real-snapshot"}
            }),
        ))
        .await
        .unwrap();
    let error = resp.error.expect("expected JSON-RPC error");
    assert_eq!(error.code, ErrorCode::InvalidParams.code());
    assert!(
        error.message.contains("snapshot_not_found"),
        "expected snapshot_not_found in: {}",
        error.message
    );

    daemon.abort();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_dir_all(&state_dir);
    std::env::remove_var("XDG_STATE_HOME");
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
