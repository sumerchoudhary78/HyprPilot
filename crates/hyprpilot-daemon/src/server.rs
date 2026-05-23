//! Daemon accept loop and per-request dispatch.
//!
//! The server owns a single [`hyprpilot_core::Connection`] (cheap to clone)
//! and a shared [`UndoStack`]. Each client connection gets its own reader
//! task that streams newline-delimited requests; we await one request at a
//! time on a given connection to keep response ordering with id matching
//! trivial.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result as AnyResult};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use hyprpilot_core::dispatch::FullscreenMode;
use hyprpilot_core::selector::{WindowSelector, WorkspaceRef};
use hyprpilot_core::types::Client as HyprClient;
use hyprpilot_core::{Connection, Error as CoreError};

use crate::protocol::{
    codes, Request, RequestEnvelope, Response, ResponseEnvelope, UndoListEntry,
};
use crate::undo::{apply_inverse, read_proc_cmdline, InverseAction, UndoStack};

const UNDO_CAPACITY: usize = 64;

#[derive(Clone)]
struct State {
    hypr: Connection,
    undo: Arc<Mutex<UndoStack>>,
    shutdown: Arc<Notify>,
}

pub async fn run(socket_path: PathBuf) -> AnyResult<()> {
    if socket_path.exists() {
        // Stale socket left over from a previous run. Refuse to start if a
        // peer answers — that's a live daemon — otherwise unlink and bind.
        match UnixStream::connect(&socket_path).await {
            Ok(_) => {
                anyhow::bail!(
                    "another hyprpilot-daemon appears to be running at {} \
                     (use `hyprpilot daemon stop` to terminate it)",
                    socket_path.display()
                );
            }
            Err(_) => {
                let _ = std::fs::remove_file(&socket_path);
            }
        }
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    info!(path = %socket_path.display(), "daemon listening");

    let hypr = Connection::new().context("connect to Hyprland")?;
    let version = hypr.version().await.context("Hyprland version probe")?;
    info!(version = %version.version, "connected to Hyprland");

    let state = State {
        hypr,
        undo: Arc::new(Mutex::new(UndoStack::with_capacity(UNDO_CAPACITY))),
        shutdown: Arc::new(Notify::new()),
    };

    let shutdown = state.shutdown.clone();
    let accept_loop = {
        let state = state.clone();
        async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_client(stream, state).await {
                                debug!(error = %e, "client task ended with error");
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "accept failed");
                        // Soft-recover; if it keeps failing the operator will notice.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    };

    let signal_loop = async {
        tokio::select! {
            _ = shutdown.notified() => info!("shutdown requested by client"),
            _ = wait_signal() => info!("received termination signal"),
        }
    };

    tokio::select! {
        _ = accept_loop => {},
        _ = signal_loop => {},
    }

    let _ = std::fs::remove_file(&socket_path);
    info!("daemon exiting");
    Ok(())
}

async fn wait_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
    }
}

async fn handle_client(stream: UnixStream, state: State) -> AnyResult<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let response = match serde_json::from_str::<RequestEnvelope>(line.trim_end()) {
            Ok(env) => {
                let id = env.id;
                let resp = handle_request(&state, env.request).await;
                ResponseEnvelope { id, response: resp }
            }
            Err(e) => {
                warn!(error = %e, raw = %line.trim_end(), "parse error");
                ResponseEnvelope {
                    id: 0,
                    response: Response::err(codes::PARSE, e.to_string()),
                }
            }
        };
        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        write.write_all(out.as_bytes()).await?;
        write.flush().await?;
    }
}

async fn handle_request(state: &State, req: Request) -> Response {
    match req {
        Request::Ping => Response::ok("pong"),
        Request::Shutdown => {
            state.shutdown.notify_one();
            Response::ok("shutting_down")
        }

        // ---- queries -------------------------------------------------------
        Request::QueryClients => from_core(state.hypr.clients().await),
        Request::QueryWorkspaces => from_core(state.hypr.workspaces().await),
        Request::QueryMonitors => from_core(state.hypr.monitors().await),
        Request::QueryActiveWindow => from_core(state.hypr.active_window().await),
        Request::QueryActiveWorkspace => from_core(state.hypr.active_workspace().await),
        Request::QueryVersion => from_core(state.hypr.version().await),
        Request::QueryCursorPos => from_core(state.hypr.cursor_position().await),

        // ---- mutating, recorded for undo -----------------------------------
        Request::DispatchKillActive => kill_active_recorded(state).await,
        Request::DispatchCloseWindow { selector } => close_window_recorded(state, selector).await,
        Request::DispatchMoveToWorkspace { workspace } => {
            move_to_workspace_recorded(state, workspace, false).await
        }
        Request::DispatchMoveToWorkspaceSilent { workspace } => {
            move_to_workspace_recorded(state, workspace, true).await
        }

        // ---- mutating, no undo ---------------------------------------------
        Request::DispatchFocusWindow { selector } => {
            void_from_core(state.hypr.focus_window(&selector).await)
        }
        Request::DispatchCycleNext => void_from_core(state.hypr.cycle_next().await),
        Request::DispatchCyclePrev => void_from_core(state.hypr.cycle_prev().await),
        Request::DispatchSwitchWorkspace { workspace } => {
            void_from_core(state.hypr.switch_workspace(&workspace).await)
        }
        Request::DispatchToggleFloating => void_from_core(state.hypr.toggle_floating().await),
        Request::DispatchFullscreen { mode } => {
            void_from_core(state.hypr.set_fullscreen(unwrap_mode(mode)).await)
        }
        Request::DispatchCenterActive => void_from_core(state.hypr.center_active().await),
        Request::DispatchPinActive => void_from_core(state.hypr.pin_active().await),
        Request::DispatchMoveActive { direction } => {
            void_from_core(state.hypr.move_active(direction).await)
        }
        Request::DispatchResizeActive { dx, dy } => {
            void_from_core(state.hypr.resize_active(dx, dy).await)
        }
        Request::DispatchSwapActive { direction } => {
            void_from_core(state.hypr.swap_active(direction).await)
        }
        Request::DispatchExec { command } => void_from_core(state.hypr.exec(&command).await),
        Request::DispatchFocusMonitor { name } => {
            void_from_core(state.hypr.focus_monitor(&name).await)
        }
        Request::DispatchMoveWorkspaceToMonitor { monitor } => {
            void_from_core(state.hypr.move_workspace_to_monitor(&monitor).await)
        }

        // ---- undo ----------------------------------------------------------
        Request::Undo => undo(state).await,
        Request::UndoList => undo_list(state).await,
    }
}

fn unwrap_mode(m: FullscreenMode) -> FullscreenMode {
    m
}

async fn kill_active_recorded(state: &State) -> Response {
    let active = match state.hypr.active_window().await {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Response::err(codes::HYPRLAND_REJECTED, "no active window to kill");
        }
        Err(e) => return core_error(e),
    };
    let inverse = inverse_for_killed(&active);
    if let Err(e) = state.hypr.kill_active().await {
        return core_error(e);
    }
    if let Some(action) = inverse {
        let mut undo = state.undo.lock().await;
        undo.push(format!("kill {} ({})", active.class, active.title), action);
    } else {
        warn!(
            pid = active.pid,
            class = %active.class,
            "killed window without capturing inverse (no /proc/<pid>/cmdline)"
        );
    }
    Response::ok(serde_json::json!({ "killed": active.address }))
}

async fn close_window_recorded(state: &State, selector: WindowSelector) -> Response {
    // Best-effort: find a matching window so we can capture its cmdline.
    let matched = match state.hypr.clients().await {
        Ok(cs) => cs.into_iter().find(|c| selector_matches(&selector, c)),
        Err(e) => return core_error(e),
    };
    let inverse = matched.as_ref().and_then(inverse_for_killed);
    if let Err(e) = state.hypr.close_window(&selector).await {
        return core_error(e);
    }
    if let (Some(c), Some(action)) = (matched.as_ref(), inverse) {
        let mut undo = state.undo.lock().await;
        undo.push(format!("close {} ({})", c.class, c.title), action);
    }
    Response::ok(serde_json::json!({ "closed": matched.map(|c| c.address) }))
}

async fn move_to_workspace_recorded(
    state: &State,
    workspace: WorkspaceRef,
    silent: bool,
) -> Response {
    let active = match state.hypr.active_window().await {
        Ok(Some(c)) => c,
        Ok(None) => return Response::err(codes::HYPRLAND_REJECTED, "no active window to move"),
        Err(e) => return core_error(e),
    };
    let prev_ws = WorkspaceRef::Id(active.workspace.id);
    let inverse = InverseAction::MoveToWorkspace {
        selector: WindowSelector::Address(active.address.clone()),
        workspace: prev_ws,
        silent,
    };
    let result = if silent {
        state.hypr.move_active_to_workspace_silent(&workspace).await
    } else {
        state.hypr.move_active_to_workspace(&workspace).await
    };
    if let Err(e) = result {
        return core_error(e);
    }
    let mut undo = state.undo.lock().await;
    undo.push(
        format!(
            "move {} from ws {} to ws {}",
            active.class, active.workspace.id, workspace
        ),
        inverse,
    );
    Response::ok(serde_json::json!({ "moved": active.address }))
}

fn selector_matches(sel: &WindowSelector, c: &HyprClient) -> bool {
    match sel {
        WindowSelector::Address(a) => normalize_addr(a) == normalize_addr(&c.address),
        WindowSelector::Pid(p) => c.pid == *p,
        WindowSelector::Class(s) => c.class == *s,
        WindowSelector::Title(s) => c.title == *s,
        WindowSelector::Tag(t) => c.tags.iter().any(|x| x == t),
        WindowSelector::Active => false, // resolved by Hyprland; we don't second-guess
    }
}

fn normalize_addr(s: &str) -> &str {
    s.strip_prefix("0x").unwrap_or(s)
}

fn inverse_for_killed(c: &HyprClient) -> Option<InverseAction> {
    let cmd = read_proc_cmdline(c.pid)?;
    Some(InverseAction::Spawn { command: cmd })
}

async fn undo(state: &State) -> Response {
    let entry = {
        let mut undo = state.undo.lock().await;
        undo.pop()
    };
    let Some(entry) = entry else {
        return Response::err(codes::UNDO_EMPTY, "undo stack is empty");
    };
    match apply_inverse(&state.hypr, &entry.inverse).await {
        Ok(()) => Response::ok(serde_json::json!({ "undone": entry.description })),
        Err(e) => {
            // Inverse failed — don't silently lose context.
            error!(error = %e, description = %entry.description, "undo failed");
            Response::err(
                codes::UNDO_FAILED,
                format!("inverse `{}` failed: {e}", entry.description),
            )
        }
    }
}

async fn undo_list(state: &State) -> Response {
    let undo = state.undo.lock().await;
    let entries: Vec<UndoListEntry> = undo
        .iter()
        .rev()
        .enumerate()
        .map(|(i, e)| UndoListEntry {
            index: i,
            timestamp_unix: e.timestamp_unix,
            description: e.description.clone(),
        })
        .collect();
    Response::ok(entries)
}

fn from_core<T: serde::Serialize>(r: Result<T, CoreError>) -> Response {
    match r {
        Ok(v) => Response::ok(v),
        Err(e) => core_error(e),
    }
}

fn void_from_core(r: Result<(), CoreError>) -> Response {
    match r {
        Ok(()) => Response::ok(serde_json::Value::Null),
        Err(e) => core_error(e),
    }
}

fn core_error(e: CoreError) -> Response {
    let (code, msg) = match &e {
        CoreError::Rejected { .. } => (codes::HYPRLAND_REJECTED, e.to_string()),
        CoreError::UnknownDispatcher(_) => (codes::HYPRLAND_UNKNOWN, e.to_string()),
        CoreError::Io(_) => (codes::HYPRLAND_IO, e.to_string()),
        CoreError::NoInstance(_) | CoreError::NoRuntimeDir | CoreError::SocketMissing(_) => {
            (codes::NO_INSTANCE, e.to_string())
        }
        _ => (codes::INTERNAL, e.to_string()),
    };
    Response::err(code, msg)
}

#[allow(dead_code)]
pub(crate) fn _socket_path_dummy(_: &Path) {}
