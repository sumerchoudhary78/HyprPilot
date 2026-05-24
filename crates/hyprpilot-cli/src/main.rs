use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use hyprpilot_core::dispatch::{Direction, FullscreenMode};
use hyprpilot_core::selector::{WindowSelector, WorkspaceRef};
use hyprpilot_core::Connection;
use hyprpilot_daemon::{client::DaemonClient, default_socket_path, protocol::Request};
use hyprpilot_input::keys::{KeyCombo, MouseButton};

#[derive(Parser, Debug)]
#[command(
    name = "hyprpilot",
    version,
    about = "Typed control over Hyprland from the shell or via a daemon"
)]
struct Cli {
    /// Force every mutating op through the daemon (required for undo).
    #[arg(long, global = true)]
    use_daemon: bool,

    /// Override daemon socket path.
    #[arg(long, global = true, value_name = "PATH")]
    daemon_socket: Option<PathBuf>,

    /// Emit results as JSON instead of pretty text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Read-only queries against Hyprland.
    #[command(subcommand)]
    Query(QueryCmd),
    /// Window operations.
    #[command(subcommand)]
    Win(WinCmd),
    /// Workspace operations.
    #[command(subcommand)]
    Ws(WsCmd),
    /// Spawn an external command via Hyprland's `exec` dispatcher.
    Exec {
        /// The shell command to spawn.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Daemon control.
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Undo the last reversible operation (requires daemon).
    Undo,
    /// List the daemon's undo stack (requires daemon).
    UndoList,
    /// Named snapshots of window/workspace state.
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
    /// Declarative event-to-action rules.
    #[command(subcommand)]
    Rules(RulesCmd),
    /// Input synthesis: typing, key chords, mouse. Disabled by default
    /// in the daemon; requires HYPRPILOT_DANGEROUS_INPUT_OK=1 in the
    /// daemon's environment.
    #[command(subcommand)]
    Input(InputCmd),
}

#[derive(Subcommand, Debug)]
enum InputCmd {
    /// Type free-form text into the focused window (via wtype).
    Type {
        /// Text to type. Use `--` to pass text that begins with `-`.
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
    /// Press a key chord in the focused window (via wtype).
    Keys {
        /// e.g. `ctrl+shift+t`, `super+space`, `Escape`.
        combo: String,
    },
    /// Send a key chord to a specific window (via Hyprland sendshortcut;
    /// no external backend, no focus change).
    Shortcut {
        /// Key chord, e.g. `ctrl+t`.
        combo: String,
        /// Window selector: `active`, `class:Foo`, `pid:1234`, etc.
        target: String,
    },
    /// Move the mouse cursor (via ydotool).
    MouseMove {
        x: i32,
        y: i32,
        /// Interpret (x, y) as absolute screen coordinates rather than a
        /// delta from the current cursor position.
        #[arg(short, long)]
        absolute: bool,
    },
    /// Click a mouse button (via ydotool).
    MouseClick {
        /// `left`, `right`, `middle`, `x1`/`back`, `x2`/`forward`.
        button: String,
    },
}

#[derive(Subcommand, Debug)]
enum RulesCmd {
    /// Show the rules the daemon would load (re-reads the file each call).
    List,
    /// Parse + compile the rules file. Reports the first error if invalid.
    Validate,
    /// Print the path the daemon reads rules from.
    Path,
}

#[derive(Subcommand, Debug)]
enum SnapshotCmd {
    /// Capture the current state as a named snapshot.
    Save { name: String },
    /// List all on-disk snapshots.
    List,
    /// Show what restoring a snapshot would change, without applying.
    Diff { name: String },
    /// Restore a snapshot. Auto-saves a `_pre-restore-<ts>` snapshot first.
    Restore { name: String },
    /// Delete a snapshot.
    Delete { name: String },
}

#[derive(Subcommand, Debug)]
enum QueryCmd {
    Clients,
    Workspaces,
    Monitors,
    ActiveWindow,
    ActiveWorkspace,
    Version,
    CursorPos,
}

#[derive(Subcommand, Debug)]
enum WinCmd {
    /// Focus a window by selector.
    Focus(SelectorArg),
    /// Close a specific window by selector.
    Close(SelectorArg),
    /// Close the focused window (records undo when going through the daemon).
    Kill,
    /// Cycle focus to the next (or previous) tiled window.
    Cycle {
        #[arg(long)]
        prev: bool,
    },
    /// Toggle floating mode on the focused window.
    Float,
    /// Set fullscreen mode for the focused window.
    Fullscreen {
        #[arg(value_enum, default_value_t = FsMode::Fullscreen)]
        mode: FsMode,
    },
    /// Center the focused window (floating only).
    Center,
    /// Toggle pinned (sticky across workspaces) on the focused window.
    Pin,
    /// Move the focused window in a direction.
    Move {
        #[arg(value_enum)]
        direction: Dir,
    },
    /// Resize the focused window by (dx, dy) pixels.
    Resize { dx: i32, dy: i32 },
    /// Swap the focused window with its neighbour.
    Swap {
        #[arg(value_enum)]
        direction: Dir,
    },
}

#[derive(Subcommand, Debug)]
enum WsCmd {
    /// Switch to a workspace.
    Switch {
        #[arg(value_parser = parse_workspace_ref, allow_hyphen_values = true)]
        target: WorkspaceRef,
    },
    /// Send the focused window to a workspace and follow it.
    Send {
        #[arg(value_parser = parse_workspace_ref, allow_hyphen_values = true)]
        target: WorkspaceRef,
    },
    /// Send the focused window silently (no focus change).
    SendSilent {
        #[arg(value_parser = parse_workspace_ref, allow_hyphen_values = true)]
        target: WorkspaceRef,
    },
    /// Move the focused workspace to a named monitor.
    ToMonitor { monitor: String },
    /// Focus a named monitor (or direction: l/r/u/d).
    FocusMonitor { name_or_direction: String },
}

#[derive(Subcommand, Debug)]
enum DaemonCmd {
    /// Run the daemon in the foreground.
    Run {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Ping a running daemon.
    Ping,
    /// Ask the daemon to shut down.
    Stop,
    /// Show whether a daemon is reachable on the socket.
    Status,
}

#[derive(Args, Debug)]
struct SelectorArg {
    /// Selector: `address:0x...`, `pid:1234`, `class:kitty`, `title:foo`,
    /// `tag:x`, or `active`.
    selector: String,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Dir { L, R, U, D }
impl Dir {
    fn into_core(self) -> Direction {
        match self {
            Dir::L => Direction::Left,
            Dir::R => Direction::Right,
            Dir::U => Direction::Up,
            Dir::D => Direction::Down,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum FsMode { Maximize, Fullscreen }
impl FsMode {
    fn into_core(self) -> FullscreenMode {
        match self {
            FsMode::Maximize => FullscreenMode::Maximize,
            FsMode::Fullscreen => FullscreenMode::Fullscreen,
        }
    }
}

fn parse_workspace_ref(s: &str) -> std::result::Result<WorkspaceRef, String> {
    WorkspaceRef::parse(s)
}

fn parse_selector(s: &str) -> Result<WindowSelector> {
    WindowSelector::parse(s).map_err(|e| anyhow!(e))
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Daemon(c) => run_daemon_cmd(c, cli.daemon_socket, cli.json).await,
        Cmd::Query(q) => run_query(q, cli.use_daemon, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Win(w) => run_win(w, cli.use_daemon, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Ws(w) => run_ws(w, cli.use_daemon, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Exec { command } => {
            let cmd = command.join(" ");
            let conn = Connection::new()?;
            conn.exec(&cmd).await?;
            emit_json_or_text(cli.json, &serde_json::json!({ "spawned": cmd }), "ok")
        }
        Cmd::Undo => {
            let mut client = open_daemon(cli.daemon_socket.as_deref()).await?;
            let v: serde_json::Value = client.call(Request::Undo).await?;
            emit_json_or_text(cli.json, &v, &format!("{v}"))
        }
        Cmd::UndoList => {
            let mut client = open_daemon(cli.daemon_socket.as_deref()).await?;
            let v: serde_json::Value = client.call(Request::UndoList).await?;
            emit_json_or_text(cli.json, &v, &pretty_undo_list(&v))
        }
        Cmd::Snapshot(s) => run_snapshot_cmd(s, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Rules(r) => run_rules_cmd(r, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Input(i) => run_input_cmd(i, cli.daemon_socket.as_deref(), cli.json).await,
    }
}

async fn run_input_cmd(
    i: InputCmd,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    let mut client = open_daemon(socket).await?;
    let req = match i {
        InputCmd::Type { text } => Request::InputType { text: text.join(" ") },
        InputCmd::Keys { combo } => Request::InputKeys {
            combo: KeyCombo::parse(&combo).map_err(|e| anyhow!(e))?,
        },
        InputCmd::Shortcut { combo, target } => Request::InputShortcut {
            combo: KeyCombo::parse(&combo).map_err(|e| anyhow!(e))?,
            selector: hyprpilot_core::WindowSelector::parse(&target).map_err(|e| anyhow!(e))?,
        },
        InputCmd::MouseMove { x, y, absolute } => Request::InputMouseMove { x, y, absolute },
        InputCmd::MouseClick { button } => Request::InputMouseClick {
            button: MouseButton::parse(&button).map_err(|e| anyhow!(e))?,
        },
    };
    let v: serde_json::Value = client.call(req).await?;
    emit_json_or_text(json, &v, &pretty(&v))
}

async fn run_rules_cmd(
    r: RulesCmd,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    let mut client = open_daemon(socket).await?;
    let req = match r {
        RulesCmd::List => Request::RulesList,
        RulesCmd::Validate => Request::RulesValidate,
        RulesCmd::Path => Request::RulesPath,
    };
    let v: serde_json::Value = client.call(req).await?;
    emit_json_or_text(json, &v, &pretty(&v))
}

async fn run_snapshot_cmd(
    s: SnapshotCmd,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    let mut client = open_daemon(socket).await?;
    let (req, label) = match s {
        SnapshotCmd::Save { name } => (Request::SnapshotSave { name: name.clone() }, name),
        SnapshotCmd::List => (Request::SnapshotList, "list".to_string()),
        SnapshotCmd::Diff { name } => (Request::SnapshotDiff { name: name.clone() }, name),
        SnapshotCmd::Restore { name } => (Request::SnapshotRestore { name: name.clone() }, name),
        SnapshotCmd::Delete { name } => (Request::SnapshotDelete { name: name.clone() }, name),
    };
    let v: serde_json::Value = client.call(req).await?;
    let text = match &v {
        serde_json::Value::Array(arr) if arr.iter().all(|x| x.is_string()) => {
            // snapshot list — render as a plain list
            arr.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => pretty(&v),
    };
    let _ = label;
    emit_json_or_text(json, &v, &text)
}

fn pretty_undo_list(v: &serde_json::Value) -> String {
    let Some(arr) = v.as_array() else { return v.to_string(); };
    if arr.is_empty() {
        return "(empty)".into();
    }
    let mut out = String::new();
    for entry in arr {
        let idx = entry.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
        let desc = entry.get("description").and_then(|x| x.as_str()).unwrap_or("");
        let ts = entry.get("timestamp_unix").and_then(|x| x.as_i64()).unwrap_or(0);
        out.push_str(&format!("{idx:>3}  t={ts}  {desc}\n"));
    }
    out
}

async fn run_daemon_cmd(c: DaemonCmd, socket_override: Option<PathBuf>, json: bool) -> Result<()> {
    match c {
        DaemonCmd::Run { socket } => {
            let path = socket.or(socket_override).unwrap_or_else(default_socket_path);
            hyprpilot_daemon::server::run(path).await
        }
        DaemonCmd::Ping => {
            let mut client = open_daemon(socket_override.as_deref()).await?;
            let v: serde_json::Value = client.call(Request::Ping).await?;
            emit_json_or_text(json, &v, &v.to_string())
        }
        DaemonCmd::Stop => {
            let mut client = open_daemon(socket_override.as_deref()).await?;
            let v: serde_json::Value = client.call(Request::Shutdown).await?;
            emit_json_or_text(json, &v, &v.to_string())
        }
        DaemonCmd::Status => {
            let path = socket_override.unwrap_or_else(default_socket_path);
            match DaemonClient::connect(&path).await {
                Ok(_) => emit_json_or_text(
                    json,
                    &serde_json::json!({ "reachable": true, "socket": path }),
                    &format!("reachable: {}", path.display()),
                ),
                Err(e) => emit_json_or_text(
                    json,
                    &serde_json::json!({
                        "reachable": false, "socket": path, "error": e.to_string()
                    }),
                    &format!("not reachable: {} ({e})", path.display()),
                ),
            }
        }
    }
}

async fn open_daemon(socket: Option<&std::path::Path>) -> Result<DaemonClient> {
    let path = socket.map(|p| p.to_path_buf()).unwrap_or_else(default_socket_path);
    DaemonClient::connect(&path)
        .await
        .with_context(|| format!("connect to daemon at {}", path.display()))
}

async fn run_query(
    q: QueryCmd,
    use_daemon: bool,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    if use_daemon {
        let mut client = open_daemon(socket).await?;
        let req = match q {
            QueryCmd::Clients => Request::QueryClients,
            QueryCmd::Workspaces => Request::QueryWorkspaces,
            QueryCmd::Monitors => Request::QueryMonitors,
            QueryCmd::ActiveWindow => Request::QueryActiveWindow,
            QueryCmd::ActiveWorkspace => Request::QueryActiveWorkspace,
            QueryCmd::Version => Request::QueryVersion,
            QueryCmd::CursorPos => Request::QueryCursorPos,
        };
        let v: serde_json::Value = client.call(req).await?;
        return emit_json_or_text(json, &v, &pretty(&v));
    }
    let conn = Connection::new()?;
    match q {
        QueryCmd::Clients => emit(json, &conn.clients().await?),
        QueryCmd::Workspaces => emit(json, &conn.workspaces().await?),
        QueryCmd::Monitors => emit(json, &conn.monitors().await?),
        QueryCmd::ActiveWindow => emit(json, &conn.active_window().await?),
        QueryCmd::ActiveWorkspace => emit(json, &conn.active_workspace().await?),
        QueryCmd::Version => emit(json, &conn.version().await?),
        QueryCmd::CursorPos => emit(json, &conn.cursor_position().await?),
    }
}

async fn run_win(
    w: WinCmd,
    use_daemon: bool,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    if use_daemon {
        let mut client = open_daemon(socket).await?;
        let req = match w {
            WinCmd::Focus(s) => Request::DispatchFocusWindow { selector: parse_selector(&s.selector)? },
            WinCmd::Close(s) => Request::DispatchCloseWindow { selector: parse_selector(&s.selector)? },
            WinCmd::Kill => Request::DispatchKillActive,
            WinCmd::Cycle { prev } => {
                if prev { Request::DispatchCyclePrev } else { Request::DispatchCycleNext }
            }
            WinCmd::Float => Request::DispatchToggleFloating,
            WinCmd::Fullscreen { mode } => Request::DispatchFullscreen { mode: mode.into_core() },
            WinCmd::Center => Request::DispatchCenterActive,
            WinCmd::Pin => Request::DispatchPinActive,
            WinCmd::Move { direction } => Request::DispatchMoveActive { direction: direction.into_core() },
            WinCmd::Resize { dx, dy } => Request::DispatchResizeActive { dx, dy },
            WinCmd::Swap { direction } => Request::DispatchSwapActive { direction: direction.into_core() },
        };
        let v: serde_json::Value = client.call(req).await?;
        return emit_json_or_text(json, &v, &v.to_string());
    }
    let conn = Connection::new()?;
    match w {
        WinCmd::Focus(s) => conn.focus_window(&parse_selector(&s.selector)?).await?,
        WinCmd::Close(s) => conn.close_window(&parse_selector(&s.selector)?).await?,
        WinCmd::Kill => conn.kill_active().await?,
        WinCmd::Cycle { prev } => if prev { conn.cycle_prev().await? } else { conn.cycle_next().await? },
        WinCmd::Float => conn.toggle_floating().await?,
        WinCmd::Fullscreen { mode } => conn.set_fullscreen(mode.into_core()).await?,
        WinCmd::Center => conn.center_active().await?,
        WinCmd::Pin => conn.pin_active().await?,
        WinCmd::Move { direction } => conn.move_active(direction.into_core()).await?,
        WinCmd::Resize { dx, dy } => conn.resize_active(dx, dy).await?,
        WinCmd::Swap { direction } => conn.swap_active(direction.into_core()).await?,
    }
    emit_json_or_text(json, &serde_json::json!({ "ok": true }), "ok")
}

async fn run_ws(
    w: WsCmd,
    use_daemon: bool,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    if use_daemon {
        let mut client = open_daemon(socket).await?;
        let req = match w {
            WsCmd::Switch { target } => Request::DispatchSwitchWorkspace { workspace: target },
            WsCmd::Send { target } => Request::DispatchMoveToWorkspace { workspace: target },
            WsCmd::SendSilent { target } => Request::DispatchMoveToWorkspaceSilent { workspace: target },
            WsCmd::ToMonitor { monitor } => Request::DispatchMoveWorkspaceToMonitor { monitor },
            WsCmd::FocusMonitor { name_or_direction } => Request::DispatchFocusMonitor { name: name_or_direction },
        };
        let v: serde_json::Value = client.call(req).await?;
        return emit_json_or_text(json, &v, &v.to_string());
    }
    let conn = Connection::new()?;
    match w {
        WsCmd::Switch { target } => conn.switch_workspace(&target).await?,
        WsCmd::Send { target } => conn.move_active_to_workspace(&target).await?,
        WsCmd::SendSilent { target } => conn.move_active_to_workspace_silent(&target).await?,
        WsCmd::ToMonitor { monitor } => conn.move_workspace_to_monitor(&monitor).await?,
        WsCmd::FocusMonitor { name_or_direction } => conn.focus_monitor(&name_or_direction).await?,
    }
    emit_json_or_text(json, &serde_json::json!({ "ok": true }), "ok")
}

fn emit<T: Serialize>(_json: bool, v: &T) -> Result<()> {
    // v0.1: pretty JSON for both modes. A non-JSON pretty printer per type is
    // future work; for now, pretty JSON is already human-readable.
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn emit_json_or_text(json: bool, v: &serde_json::Value, text: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
    } else {
        println!("{text}");
    }
    Ok(())
}

fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_relative_workspace_parses_as_value_not_flag() {
        // Regression for issue #11: `ws switch -1` used to fail clap parsing
        // because clap treated `-1` as a short flag.
        let cli = Cli::parse_from(["hyprpilot", "ws", "switch", "-1"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::Switch { target }) => {
                assert_eq!(target, WorkspaceRef::Relative(-1));
            }
            other => panic!("expected Ws Switch, got {other:?}"),
        }
    }

    #[test]
    fn negative_relative_workspace_parses_for_send() {
        let cli = Cli::parse_from(["hyprpilot", "ws", "send", "-1"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::Send { target }) => {
                assert_eq!(target, WorkspaceRef::Relative(-1));
            }
            other => panic!("expected Ws Send, got {other:?}"),
        }
    }

    #[test]
    fn negative_relative_workspace_parses_for_send_silent() {
        let cli = Cli::parse_from(["hyprpilot", "ws", "send-silent", "-1"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::SendSilent { target }) => {
                assert_eq!(target, WorkspaceRef::Relative(-1));
            }
            other => panic!("expected Ws SendSilent, got {other:?}"),
        }
    }

    #[test]
    fn positive_relative_workspace_still_parses() {
        let cli = Cli::parse_from(["hyprpilot", "ws", "switch", "+1"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::Switch { target }) => {
                assert_eq!(target, WorkspaceRef::Relative(1));
            }
            other => panic!("expected Ws Switch, got {other:?}"),
        }
    }

    #[test]
    fn absolute_workspace_id_still_parses() {
        let cli = Cli::parse_from(["hyprpilot", "ws", "switch", "2"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::Switch { target }) => {
                assert_eq!(target, WorkspaceRef::Id(2));
            }
            other => panic!("expected Ws Switch, got {other:?}"),
        }
    }
}
