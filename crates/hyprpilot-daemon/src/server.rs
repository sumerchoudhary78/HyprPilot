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
use hyprpilot_core::types::{Client as HyprClient, Monitor};
use hyprpilot_core::{Connection, Error as CoreError};
use hyprpilot_a11y::{A11y, A11yError, WalkOpts};
use hyprpilot_input::keys::{KeyCombo, MouseButton};
use hyprpilot_input::{InputError, InputRunner};

use crate::cache::{self, StateCache};
use crate::protocol::{
    codes, Request, RequestEnvelope, Response, ResponseEnvelope, UndoListEntry,
};
use crate::undo::{
    apply_inverse, default_persist_path, read_proc_cmdline, InverseAction, LoadOutcome, UndoStack,
};

const UNDO_CAPACITY: usize = 64;

#[derive(Clone)]
struct State {
    hypr: Connection,
    /// Event-warmed world-model cache. Read-only queries are served from
    /// here; mutating handlers still read `hypr` live for authoritative
    /// pre-mutation state (undo capture, selector matching).
    cache: StateCache,
    undo: Arc<Mutex<UndoStack>>,
    shutdown: Arc<Notify>,
    /// True when HYPRPILOT_DANGEROUS_INPUT_OK=1 was set at startup.
    /// Input dispatchers refuse on false with `input_disabled`.
    input_enabled: bool,
    input_runner: Arc<InputRunner>,
    /// Lazily-established accessibility-bus connection, shared across clients.
    /// `None` until first use or after a connection-level failure (next a11y
    /// request reconnects). The a11y bus may be absent entirely — requests
    /// then fail cleanly with `a11y_unavailable`.
    a11y: Arc<Mutex<Option<A11y>>>,
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

    // World-model cache, kept warm by its own event-socket subscription.
    let cache = StateCache::new(hypr.clone());

    let mut undo_stack = UndoStack::with_capacity(UNDO_CAPACITY);
    let persist_path = default_persist_path();
    match undo_stack.enable_persistence(persist_path.clone()) {
        LoadOutcome::Loaded(n) => info!(
            entries = n,
            path = %persist_path.display(),
            "restored persisted undo stack"
        ),
        LoadOutcome::Empty => info!(
            path = %persist_path.display(),
            "no persisted undo stack; starting fresh"
        ),
        LoadOutcome::Corrupt(e) => warn!(
            error = %e,
            path = %persist_path.display(),
            "persisted undo stack was malformed; starting fresh (file will be overwritten)"
        ),
    }

    // Input subsystem. Gated by an explicit env var because input
    // synthesis is uniquely dangerous: any focused terminal executes
    // typed shell commands. The gate is checked once at startup; per
    // request the daemon just consults state.input_enabled.
    let input_enabled = std::env::var("HYPRPILOT_DANGEROUS_INPUT_OK")
        .map(|v| v == "1")
        .unwrap_or(false);
    let input_runner = Arc::new(InputRunner::detect());
    if input_enabled {
        info!(
            backend = input_runner.backend_name(),
            mouse = input_runner.mouse_backend_name(),
            wtype = ?input_runner.backends().wtype,
            ydotool = ?input_runner.backends().ydotool,
            "input synthesis ENABLED (HYPRPILOT_DANGEROUS_INPUT_OK=1)"
        );
    } else {
        info!(
            "input synthesis DISABLED \
             (set HYPRPILOT_DANGEROUS_INPUT_OK=1 in the daemon's env to enable)"
        );
    }

    let state = State {
        hypr,
        cache: cache.clone(),
        undo: Arc::new(Mutex::new(undo_stack)),
        shutdown: Arc::new(Notify::new()),
        input_enabled,
        input_runner,
        a11y: Arc::new(Mutex::new(None)),
    };

    // Keep the cache warm for the daemon's lifetime. Read-only; detached
    // because it has no cleanup — the runtime drops it on daemon exit. If the
    // event socket closes (Hyprland gone) it logs and returns; the MAX_AGE
    // backstop keeps queries correct in the meantime.
    tokio::spawn(async move {
        if let Err(e) = cache::run(cache).await {
            error!(error = %e, "state cache task exited with error");
        }
    });

    // Rules engine. Loads $XDG_CONFIG_HOME/hyprpilot/rules.toml if present;
    // if absent, the engine task still starts but is idle. Config parse
    // errors at startup are logged and the engine is skipped — the rest of
    // the daemon continues to run.
    let engine_task = match crate::rules::load_default() {
        Ok(Some(rule_cfg)) => {
            info!(rules = rule_cfg.len(), "loaded rules config");
            let conn = state.hypr.clone();
            let shutdown = state.shutdown.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = crate::engine::run(conn, rule_cfg, shutdown).await {
                    error!(error = %e, "rules engine exited with error");
                }
            }))
        }
        Ok(None) => {
            info!(
                path = %crate::rules::default_path().display(),
                "no rules file; engine disabled"
            );
            None
        }
        Err(e) => {
            warn!(error = %e, "failed to load rules config; engine disabled");
            None
        }
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

    // The engine task watches the same shutdown Notify; signal_loop above
    // either fired it on SIGINT/SIGTERM or it was fired by an RPC Shutdown
    // request. Wait briefly for the engine to exit cleanly.
    if let Some(handle) = engine_task {
        match tokio::time::timeout(std::time::Duration::from_secs(1), handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!(error = %e, "engine task panicked"),
            Err(_) => warn!("engine task did not exit within 1s; abandoning"),
        }
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

        // ---- queries (cache-served; see `crate::cache`) --------------------
        Request::QueryClients => from_core(state.cache.clients().await),
        Request::QueryWorkspaces => from_core(state.cache.workspaces().await),
        Request::QueryMonitors => from_core(state.cache.monitors().await),
        Request::QueryActiveWindow => from_core(state.cache.active_window().await),
        Request::QueryActiveWorkspace => from_core(state.cache.active_workspace().await),
        Request::QueryVersion => from_core(state.cache.version().await),
        // Cursor moves continuously with no event to invalidate on; query live.
        Request::QueryCursorPos => from_core(state.hypr.cursor_position().await),
        Request::QueryBinds => from_core(state.cache.binds().await),

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

        // ---- snapshots -----------------------------------------------------
        Request::SnapshotSave { name } => snapshot_save(state, name).await,
        Request::SnapshotList => snapshot_list(),
        Request::SnapshotDiff { name } => snapshot_diff(state, name).await,
        Request::SnapshotPreview { name } => snapshot_preview(state, name).await,
        Request::SnapshotRestore { name } => snapshot_restore(state, name).await,
        Request::SnapshotDelete { name } => snapshot_delete(name),

        // ---- rules ---------------------------------------------------------
        Request::RulesList => rules_list(),
        Request::RulesValidate => rules_validate(),
        Request::RulesPath => Response::ok(serde_json::json!({
            "path": crate::rules::default_path(),
        })),

        // ---- input ---------------------------------------------------------
        Request::InputType { text } => input_type(state, text).await,
        Request::InputKeys { combo } => input_keys(state, combo).await,
        Request::InputShortcut { combo, selector } => {
            input_shortcut(state, combo, selector).await
        }
        Request::InputMouseMove { x, y, absolute } => {
            input_mouse_move(state, x, y, absolute).await
        }
        Request::InputMouseClick { button } => input_mouse_click(state, button).await,

        // ---- accessibility (AT-SPI) ----------------------------------------
        Request::A11yTree { pid, max_nodes } => a11y_tree(state, pid, max_nodes).await,
        Request::A11yFind { pid, query, role, max_nodes } => {
            a11y_find(state, pid, query, role, max_nodes).await
        }

        // ---- keymap --------------------------------------------------------
        Request::RunBind { combo, submap, dry_run } => {
            run_bind(state, combo, submap, dry_run).await
        }
    }
}

fn input_disabled_response() -> Response {
    Response::err(
        codes::INPUT_DISABLED,
        "input synthesis is disabled in this daemon. \
         Restart the daemon with HYPRPILOT_DANGEROUS_INPUT_OK=1 to enable.",
    )
}

fn input_error_response(e: InputError) -> Response {
    let code = match &e {
        InputError::BackendMissing(_) => codes::INPUT_BACKEND_MISSING,
        InputError::DaemonNotReachable(_) => codes::INPUT_DAEMON_MISSING,
        InputError::InvalidCombo(_) | InputError::InvalidButton(_) => codes::INPUT_INVALID,
        InputError::BackendFailed { .. } => codes::INPUT_FAILED,
        InputError::NotImplemented { .. } => codes::INPUT_BACKEND_MISSING,
        InputError::InvalidBackendSelection(_) => codes::INPUT_BACKEND_MISSING,
        InputError::Io(_) => codes::INPUT_FAILED,
        InputError::VirtualPointer(_) => codes::INPUT_FAILED,
    };
    Response::err(code, e.to_string())
}

async fn input_type(state: &State, text: String) -> Response {
    if !state.input_enabled {
        return input_disabled_response();
    }
    match state.input_runner.type_text(&text).await {
        Ok(()) => Response::ok(serde_json::json!({ "typed_bytes": text.len() })),
        Err(e) => input_error_response(e),
    }
}

async fn input_keys(state: &State, combo: KeyCombo) -> Response {
    if !state.input_enabled {
        return input_disabled_response();
    }
    let label = combo.to_string();
    match state.input_runner.press_keys(&combo).await {
        Ok(()) => Response::ok(serde_json::json!({ "pressed": label })),
        Err(e) => input_error_response(e),
    }
}

async fn input_shortcut(
    state: &State,
    combo: KeyCombo,
    selector: WindowSelector,
) -> Response {
    if !state.input_enabled {
        return input_disabled_response();
    }
    // No external backend — Hyprland's sendshortcut takes a selector
    // directly. Build: `sendshortcut MODS,KEY,SELECTOR`.
    let mods = combo.encode_hyprctl_mods();
    let payload = format!(
        "sendshortcut {mods},{key},{sel}",
        key = combo.key,
        sel = selector.encode()
    );
    match state.hypr.dispatch(&payload).await {
        Ok(()) => Response::ok(serde_json::json!({
            "sent": combo.to_string(),
            "target": selector.encode(),
        })),
        Err(e) => core_error(e),
    }
}

async fn input_mouse_move(state: &State, x: i32, y: i32, absolute: bool) -> Response {
    if !state.input_enabled {
        return input_disabled_response();
    }
    // The wlr-virtual-pointer maps absolute coordinates against each output's
    // DEVICE resolution (its `wl_output` mode), but the rest of HyprPilot —
    // hyprctl geometry, a11y extents, OCR-derived points — is in LOGICAL
    // compositor coordinates. On a scaled output those differ by the monitor
    // scale, so a logical point would land at `logical/scale` (e.g. half, at
    // scale 2). Convert logical → device here so callers always speak logical.
    let (mx, my) = if absolute {
        logical_to_device(state, x, y).await
    } else {
        (x, y)
    };
    match state.input_runner.mouse_move(mx, my, absolute).await {
        // Report the logical point the caller asked for, not the device one.
        Ok(()) => Response::ok(serde_json::json!({
            "moved_to": [x, y],
            "absolute": absolute,
        })),
        Err(e) => input_error_response(e),
    }
}

/// Convert a LOGICAL compositor coordinate to the DEVICE-pixel coordinate the
/// virtual pointer expects, using the scale of the monitor containing the
/// point. Exact for a single output (or uniform-scale setups); for mixed-scale
/// multi-monitor layouts it uses the containing monitor's scale, which is a
/// best-effort approximation of the virtual pointer's union coordinate space.
async fn logical_to_device(state: &State, x: i32, y: i32) -> (i32, i32) {
    let scale = match state.cache.monitors().await {
        Ok(monitors) => scale_at(&monitors, x, y),
        Err(_) => 1.0,
    };
    (
        ((x as f64) * scale).round() as i32,
        ((y as f64) * scale).round() as i32,
    )
}

/// Scale of the monitor whose LOGICAL bounds contain `(x, y)`, falling back to
/// the focused monitor, then the first, then 1.0. A monitor's logical size is
/// its device mode divided by its scale.
fn scale_at(monitors: &[Monitor], x: i32, y: i32) -> f64 {
    monitors
        .iter()
        .find(|m| {
            let lw = (m.width as f64 / m.scale).round() as i32;
            let lh = (m.height as f64 / m.scale).round() as i32;
            x >= m.x && x < m.x + lw && y >= m.y && y < m.y + lh
        })
        .or_else(|| monitors.iter().find(|m| m.focused))
        .or_else(|| monitors.first())
        .map(|m| m.scale)
        .unwrap_or(1.0)
}

async fn input_mouse_click(state: &State, button: MouseButton) -> Response {
    if !state.input_enabled {
        return input_disabled_response();
    }
    match state.input_runner.mouse_click(button).await {
        Ok(()) => Response::ok(serde_json::json!({ "clicked": button })),
        Err(e) => input_error_response(e),
    }
}

// ---- accessibility helpers ------------------------------------------------

/// Lazily connect to the a11y bus, caching the connection in `State`.
async fn a11y_conn(state: &State) -> Result<A11y, A11yError> {
    let mut guard = state.a11y.lock().await;
    if guard.is_none() {
        *guard = Some(A11y::connect().await?);
    }
    Ok(guard.as_ref().expect("just connected").clone())
}

/// Map an [`A11yError`] to an RPC response, dropping the cached connection on
/// a bus-availability failure so the next request reconnects.
async fn a11y_error_response(state: &State, e: A11yError) -> Response {
    let code = match &e {
        A11yError::Unavailable(_) => {
            *state.a11y.lock().await = None;
            codes::A11Y_UNAVAILABLE
        }
        A11yError::NoApplication(_) => codes::A11Y_NO_APP,
        A11yError::Bus(_) => codes::A11Y_FAILED,
    };
    Response::err(code, e.to_string())
}

fn walk_opts(max_nodes: Option<usize>) -> WalkOpts {
    let mut opts = WalkOpts::default();
    if let Some(n) = max_nodes.filter(|n| *n > 0) {
        opts.max_nodes = n;
    }
    opts
}

/// Resolve the pid to introspect: the caller's `pid`, or the focused window's
/// pid from the world-model cache.
async fn resolve_a11y_pid(state: &State, pid: Option<i32>) -> Result<i32, Response> {
    if let Some(p) = pid {
        return Ok(p);
    }
    match state.cache.active_window().await {
        Ok(Some(c)) => Ok(c.pid),
        Ok(None) => Err(Response::err(
            codes::A11Y_NO_APP,
            "no focused window to resolve a pid from; pass `pid` explicitly",
        )),
        Err(e) => Err(core_error(e)),
    }
}

async fn a11y_tree(state: &State, pid: Option<i32>, max_nodes: Option<usize>) -> Response {
    let pid = match resolve_a11y_pid(state, pid).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let a11y = match a11y_conn(state).await {
        Ok(a) => a,
        Err(e) => return a11y_error_response(state, e).await,
    };
    match a11y.snapshot_app(pid, walk_opts(max_nodes)).await {
        Ok(elements) => Response::ok(serde_json::json!({
            "pid": pid,
            "count": elements.len(),
            "elements": elements,
        })),
        Err(e) => a11y_error_response(state, e).await,
    }
}

async fn a11y_find(
    state: &State,
    pid: Option<i32>,
    query: String,
    role: Option<String>,
    max_nodes: Option<usize>,
) -> Response {
    let pid = match resolve_a11y_pid(state, pid).await {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let a11y = match a11y_conn(state).await {
        Ok(a) => a,
        Err(e) => return a11y_error_response(state, e).await,
    };
    match a11y.find(pid, &query, role.as_deref(), walk_opts(max_nodes)).await {
        Ok(elements) => Response::ok(serde_json::json!({
            "pid": pid,
            "query": query,
            "count": elements.len(),
            "elements": elements,
        })),
        Err(e) => a11y_error_response(state, e).await,
    }
}

/// `run_bind`: resolve the user's keybind for `combo` and dispatch its action
/// directly (no key synthesis). Reads binds from the world-model cache.
async fn run_bind(
    state: &State,
    combo: KeyCombo,
    submap: Option<String>,
    dry_run: bool,
) -> Response {
    let binds = match state.cache.binds().await {
        Ok(b) => b,
        Err(e) => return core_error(e),
    };
    let Some(bind) = crate::binds::resolve(&binds, &combo, submap.as_deref()) else {
        let scope = match &submap {
            Some(s) => format!(" in submap `{s}`"),
            None => String::new(),
        };
        return Response::err(
            codes::BIND_NOT_FOUND,
            format!("no keybind matches `{combo}`{scope}"),
        );
    };
    let action = crate::binds::bind_action(bind);
    if dry_run {
        return Response::ok(serde_json::json!({
            "dry_run": true,
            "combo": combo.to_string(),
            "dispatcher": bind.dispatcher.clone(),
            "arg": bind.arg.clone(),
            "would_dispatch": action,
        }));
    }
    match state.hypr.dispatch(&action).await {
        Ok(()) => Response::ok(serde_json::json!({
            "ran": combo.to_string(),
            "dispatched": action,
        })),
        Err(e) => core_error(e),
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

async fn snapshot_save(state: &State, name: String) -> Response {
    use hyprpilot_core::snapshot::{validate_name, Snapshot};
    if let Err(e) = validate_name(&name) {
        return Response::err(codes::SNAPSHOT_INVALID_NAME, e.to_string());
    }
    let snap = match Snapshot::capture(&state.hypr, name.clone()).await {
        Ok(s) => s,
        Err(e) => return core_error(e),
    };
    match snap.save_named() {
        Ok(path) => Response::ok(serde_json::json!({
            "name": name,
            "path": path,
            "windows": snap.windows.len(),
            "workspaces": snap.workspaces.len(),
            "monitors": snap.monitors.len(),
        })),
        Err(e) => Response::err(codes::SNAPSHOT_IO, e.to_string()),
    }
}

fn snapshot_list() -> Response {
    use hyprpilot_core::snapshot::list_names;
    match list_names() {
        Ok(names) => Response::ok(names),
        Err(e) => Response::err(codes::SNAPSHOT_IO, e.to_string()),
    }
}

async fn snapshot_diff(state: &State, name: String) -> Response {
    use hyprpilot_core::snapshot::{load_named, Snapshot};
    let target = match load_named(&name) {
        Ok(s) => s,
        Err(e) => return snapshot_load_error(e),
    };
    let live = match Snapshot::capture(&state.hypr, "_live_").await {
        Ok(s) => s,
        Err(e) => return core_error(e),
    };
    let diff = target.diff_against(&live);
    Response::ok(diff)
}

async fn snapshot_preview(state: &State, name: String) -> Response {
    use hyprpilot_core::snapshot::{load_named, Snapshot, SnapshotRestorePreview};
    let target = match load_named(&name) {
        Ok(s) => s,
        Err(e) => return snapshot_load_error(e),
    };
    let live = match Snapshot::capture(&state.hypr, "_live_").await {
        Ok(s) => s,
        Err(e) => return core_error(e),
    };
    let diff = target.diff_against(&live);
    let preview = SnapshotRestorePreview::from_snapshot_and_diff(&target, diff);
    Response::ok(preview)
}

async fn snapshot_restore(state: &State, name: String) -> Response {
    use hyprpilot_core::snapshot::{apply_diff, load_named, Snapshot, SnapshotRestorePreview};
    let target = match load_named(&name) {
        Ok(s) => s,
        Err(e) => return snapshot_load_error(e),
    };
    // Auto-save a pre-restore snapshot so the operation is reversible.
    let pre_name = format!("_pre-restore-{}", now_unix());
    match Snapshot::capture(&state.hypr, pre_name.clone()).await {
        Ok(s) => {
            if let Err(e) = s.save_named() {
                warn!(error = %e, "failed to save pre-restore snapshot; continuing");
            }
        }
        Err(e) => {
            return core_error(e);
        }
    }
    let live = match Snapshot::capture(&state.hypr, "_live_").await {
        Ok(s) => s,
        Err(e) => return core_error(e),
    };
    let diff = target.diff_against(&live);
    // Build the preview from the pre-apply diff so the response carries both
    // the plan and the per-action outcomes. Cloning the diff is cheap.
    let preview = SnapshotRestorePreview::from_snapshot_and_diff(&target, diff.clone());
    let outcomes = apply_diff(&state.hypr, &diff).await;
    Response::ok(serde_json::json!({
        "restored": name,
        "pre_restore_snapshot": pre_name,
        "mutations_attempted": diff.mutation_count(),
        "outcomes": outcomes,
        "preview": preview,
    }))
}

fn snapshot_delete(name: String) -> Response {
    use hyprpilot_core::snapshot::{delete_named, validate_name};
    if let Err(e) = validate_name(&name) {
        return Response::err(codes::SNAPSHOT_INVALID_NAME, e.to_string());
    }
    match delete_named(&name) {
        Ok(()) => Response::ok(serde_json::json!({ "deleted": name })),
        Err(e) => Response::err(codes::SNAPSHOT_IO, e.to_string()),
    }
}

fn rules_list() -> Response {
    match crate::rules::load_default() {
        Ok(Some(cfg)) => Response::ok(serde_json::json!({
            "path": crate::rules::default_path(),
            "count": cfg.len(),
            "rules": cfg.rules,
        })),
        Ok(None) => Response::ok(serde_json::json!({
            "path": crate::rules::default_path(),
            "count": 0,
            "rules": [],
        })),
        Err(e) => Response::err(codes::RULES_LOAD_FAILED, e.to_string()),
    }
}

fn rules_validate() -> Response {
    let path = crate::rules::default_path();
    let cfg = match crate::rules::load_default() {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Response::ok(serde_json::json!({
                "ok": true,
                "path": path,
                "rules": 0,
                "note": "no rules file present",
            }));
        }
        Err(e) => {
            return Response::err(codes::RULES_LOAD_FAILED, e.to_string());
        }
    };
    match crate::engine::compile(&cfg) {
        Ok(compiled) => Response::ok(serde_json::json!({
            "ok": true,
            "path": path,
            "rules": compiled.len(),
        })),
        Err(msg) => Response::err(
            codes::RULES_LOAD_FAILED,
            format!("rules compile error: {msg}"),
        ),
    }
}

fn snapshot_load_error(e: CoreError) -> Response {
    match &e {
        CoreError::NotFound(_) => Response::err(codes::SNAPSHOT_NOT_FOUND, e.to_string()),
        CoreError::Validation(_) => Response::err(codes::SNAPSHOT_INVALID_NAME, e.to_string()),
        CoreError::Io(_) | CoreError::Json(_) => Response::err(codes::SNAPSHOT_IO, e.to_string()),
        _ => core_error(e),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: i32, y: i32, w: i32, h: i32, scale: f64, focused: bool) -> Monitor {
        serde_json::from_value(serde_json::json!({
            "id": 0, "name": "eDP-1", "width": w, "height": h,
            "x": x, "y": y, "scale": scale, "transform": 0,
            "focused": focused, "disabled": false,
        }))
        .expect("valid monitor fixture")
    }

    #[test]
    fn scale_at_single_monitor_returns_its_scale() {
        // 1920x1080 @2.0 → logical 960x540; a point inside maps at scale 2.0.
        let mons = vec![monitor(0, 0, 1920, 1080, 2.0, true)];
        assert_eq!(scale_at(&mons, 246, 483), 2.0);
    }

    #[test]
    fn scale_at_handles_fractional() {
        let mons = vec![monitor(0, 0, 1920, 1080, 1.5, true)];
        assert_eq!(scale_at(&mons, 100, 100), 1.5);
    }

    #[test]
    fn scale_at_picks_the_containing_monitor() {
        // m0: logical x 0..960 @2.0 ; m1: logical x 960..2240 @1.0.
        let mons = vec![
            monitor(0, 0, 1920, 1080, 2.0, true),
            monitor(960, 0, 1280, 720, 1.0, false),
        ];
        assert_eq!(scale_at(&mons, 100, 100), 2.0, "point in m0");
        assert_eq!(scale_at(&mons, 1500, 100), 1.0, "point in m1");
    }

    #[test]
    fn scale_at_outside_all_falls_back_to_focused() {
        let mons = vec![
            monitor(0, 0, 1920, 1080, 2.0, false),
            monitor(960, 0, 1280, 720, 1.5, true),
        ];
        assert_eq!(scale_at(&mons, 5000, 5000), 1.5, "no containing monitor → focused");
    }

    #[test]
    fn scale_at_empty_is_identity() {
        assert_eq!(scale_at(&[], 100, 100), 1.0);
    }
}
