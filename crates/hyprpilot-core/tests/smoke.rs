//! Integration smoke test: read-only operations against the live Hyprland.
//!
//! Skipped when no Hyprland instance is reachable, so this does not break
//! cargo test for non-Hyprland CI hosts.

use hyprpilot_core::{Connection, Instance};

fn try_conn() -> Option<Connection> {
    Instance::discover().ok().map(Connection::with_instance)
}

#[tokio::test]
async fn version_query_roundtrips() {
    let Some(conn) = try_conn() else {
        eprintln!("skip: no live Hyprland instance");
        return;
    };
    let v = conn.version().await.expect("version query failed");
    assert!(!v.version.is_empty(), "version string was empty");
    eprintln!("Hyprland version: {}", v.version);
}

#[tokio::test]
async fn list_clients_workspaces_monitors() {
    let Some(conn) = try_conn() else {
        eprintln!("skip: no live Hyprland instance");
        return;
    };
    let clients = conn.clients().await.expect("clients query failed");
    let workspaces = conn.workspaces().await.expect("workspaces query failed");
    let monitors = conn.monitors().await.expect("monitors query failed");
    eprintln!(
        "clients={} workspaces={} monitors={}",
        clients.len(),
        workspaces.len(),
        monitors.len()
    );
    assert!(!monitors.is_empty(), "at least one monitor expected");
    assert!(!workspaces.is_empty(), "at least one workspace expected");
}

#[tokio::test]
async fn active_window_is_either_present_or_none() {
    let Some(conn) = try_conn() else {
        eprintln!("skip: no live Hyprland instance");
        return;
    };
    let _ = conn.active_window().await.expect("active_window query failed");
}

#[tokio::test]
async fn cursor_pos_is_readable() {
    let Some(conn) = try_conn() else {
        eprintln!("skip: no live Hyprland instance");
        return;
    };
    let p = conn.cursor_position().await.expect("cursor_position query failed");
    eprintln!("cursor: ({}, {})", p.x, p.y);
}

#[tokio::test]
async fn initial_class_populated_for_at_least_one_client() {
    let Some(conn) = try_conn() else {
        eprintln!("skip: no live Hyprland instance");
        return;
    };
    let clients = conn.clients().await.expect("clients query failed");
    if clients.is_empty() {
        eprintln!("warning: no clients to inspect; skipping initial_class check");
        return;
    }
    assert!(
        clients.iter().any(|c| !c.initial_class.is_empty()),
        "expected at least one client to have a non-empty initial_class; \
         this likely means the serde alias for `initialClass` regressed. \
         Clients seen: {}",
        clients.len()
    );
}

#[tokio::test]
async fn unknown_dispatcher_is_typed_error() {
    let Some(conn) = try_conn() else {
        eprintln!("skip: no live Hyprland instance");
        return;
    };
    let err = conn
        .dispatch("absolutelynotarealverb")
        .await
        .expect_err("expected an error for bogus dispatcher");
    match err {
        hyprpilot_core::Error::UnknownDispatcher(v) => {
            assert_eq!(v, "absolutelynotarealverb");
        }
        other => panic!("expected UnknownDispatcher, got {other:?}"),
    }
}
