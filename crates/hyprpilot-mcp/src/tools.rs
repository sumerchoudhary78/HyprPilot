//! Tool catalog: parameter shapes, descriptions, schemas, and dispatch.
//!
//! Each tool is defined by:
//!
//! - a parameter struct deriving `Serialize`, `Deserialize`, and `JsonSchema`,
//!   from which we generate the MCP `inputSchema`;
//! - a name, group, and LLM-facing description;
//! - a handler in [`dispatch`] that parses args and either forwards to the
//!   daemon or returns a dry-run preview.
//!
//! Conventions for tool descriptions:
//! - State whether the tool reads or mutates.
//! - For mutating tools, name the side effect and whether it is reversible
//!   via the undo stack.
//! - For destructive tools, spell out the irreversible parts.
//!
//! Stringy selectors (`address:0x...`, `class:Foo`, `pid:1234`, `title:Bar`,
//! `active`) and workspace refs (`2`, `+1`, `name:foo`, `special:term`,
//! `empty`, `prev`, `next`) are accepted as-is. They mirror Hyprland's
//! native grammar and what an LLM tends to produce.

use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use hyprpilot_core::dispatch::{Direction, FullscreenMode};
use hyprpilot_core::selector::{WindowSelector, WorkspaceRef};
use hyprpilot_daemon::protocol::Request;
use hyprpilot_input::keys::{KeyCombo, MouseButton};

use crate::capability::ToolGroup;

// =============================================================================
// Tool metadata
// =============================================================================

/// One entry in the registry. Stable across an MCP server's lifetime.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub group: ToolGroup,
    pub description: &'static str,
    pub mutating: bool,
    pub schema: Value,
}

/// Outcome of parsing + handling a tool call.
#[derive(Debug)]
pub enum Dispatch {
    /// Forward this request to the daemon and surface its response.
    Forward(Request),
    /// Return a dry-run preview without touching the daemon.
    Preview(String),
    /// Fetch a diff for the named snapshot from the daemon and render it
    /// into a structured dry-run preview for `snapshot_restore`. Lets the
    /// agent see exactly what would change without applying anything.
    PreviewSnapshotRestore { name: String },
}

/// Parameter-parsing or validation failure.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("invalid arguments for `{tool}`: {message}")]
    InvalidArgs { tool: String, message: String },
}

// =============================================================================
// Shared param fields
// =============================================================================

/// Common `dry_run` field, defaults to `true` for safety. Mutating tools
/// embed this; query tools do not.
fn default_true() -> bool {
    true
}

// =============================================================================
// Param structs — one per tool
// =============================================================================

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MutNoArgs {
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}
impl Default for MutNoArgs {
    fn default() -> Self {
        Self { dry_run: true }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SelectorArgs {
    /// Window selector. One of: `active`, `address:0x<hex>`, `pid:<int>`,
    /// `class:<regex>`, `title:<regex>`, `tag:<name>`.
    pub selector: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceArgs {
    /// Workspace target. Accepts: `<id>` (e.g. `2`), `+N` / `-N` (relative),
    /// `prev`, `next`, `empty`, `name:<n>`, `special[:<n>]`.
    pub target: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DirArg {
    L,
    R,
    U,
    D,
}
impl DirArg {
    fn to_core(&self) -> Direction {
        match self {
            DirArg::L => Direction::Left,
            DirArg::R => Direction::Right,
            DirArg::U => Direction::Up,
            DirArg::D => Direction::Down,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DirectionArgs {
    /// Direction: `l` (left), `r` (right), `u` (up), `d` (down).
    pub direction: DirArg,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ResizeArgs {
    /// Horizontal delta in pixels.
    pub dx: i32,
    /// Vertical delta in pixels.
    pub dy: i32,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsModeArg {
    Maximize,
    Fullscreen,
}
impl FsModeArg {
    fn to_core(&self) -> FullscreenMode {
        match self {
            FsModeArg::Maximize => FullscreenMode::Maximize,
            FsModeArg::Fullscreen => FullscreenMode::Fullscreen,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FullscreenArgs {
    /// `maximize` respects gaps/bars; `fullscreen` is true exclusive.
    pub mode: FsModeArg,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FocusMonitorArgs {
    /// Monitor name (e.g. `eDP-1`) or direction (`l`, `r`, `u`, `d`).
    pub name_or_direction: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MonitorArgs {
    /// Monitor name (e.g. `eDP-1`, `HDMI-A-1`).
    pub monitor: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// Shell command to spawn. Forked-and-detached by Hyprland.
    pub command: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotNameArgs {
    /// Snapshot name. Allowed chars: `[A-Za-z0-9_.-]+`, must not start with
    /// `.`, max 128 chars.
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputTextArgs {
    /// Free-form text to type into the focused window. Whatever is
    /// currently focused will receive this — verify with
    /// `query_active_window` first if you're not sure.
    pub text: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputKeysArgs {
    /// Key chord, e.g. `ctrl+shift+t`, `super+space`, `Escape`.
    /// Modifiers: ctrl, shift, alt, super (aliases: control, meta, cmd,
    /// win, mod4). Last token is the keysym (passed to wtype as-is).
    pub combo: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputShortcutArgs {
    /// Key chord, e.g. `ctrl+t`.
    pub combo: String,
    /// Window selector: `active`, `class:Foo`, `pid:1234`, `title:Foo`,
    /// `address:0x<hex>`, or `tag:<name>`.
    pub selector: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputMouseMoveArgs {
    /// X coordinate (pixels). Absolute screen-space if `absolute=true`,
    /// otherwise a delta from the current cursor position.
    pub x: i32,
    /// Y coordinate (pixels). Same semantics as x.
    pub y: i32,
    /// When true, (x, y) are absolute screen coordinates.
    /// When false (default), they are deltas from the current cursor.
    #[serde(default)]
    pub absolute: bool,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputMouseClickArgs {
    /// `left`, `right`, `middle`, `x1`/`back`, `x2`/`forward`.
    pub button: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotMutArgs {
    /// Snapshot name. Allowed chars: `[A-Za-z0-9_.-]+`, must not start with `.`,
    /// max 128 chars.
    pub name: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

// =============================================================================
// Registry
// =============================================================================

/// All tools known to this server, in stable order. The registry is the
/// single source of truth for `tools/list`.
pub fn registry() -> Vec<ToolDef> {
    use ToolGroup::*;
    vec![
        // ---- Queries (group: read) -------------------------------------------
        def::<NoArgs>(
            "query_clients",
            Read,
            false,
            "List all open windows managed by Hyprland. Returns a JSON array of \
             clients with address, class, title, workspace, monitor, geometry, \
             pid, and floating/fullscreen state. Read-only.",
        ),
        def::<NoArgs>(
            "query_workspaces",
            Read,
            false,
            "List all workspaces with id, name, monitor, window count, and \
             whether any window is currently fullscreen on them. Read-only.",
        ),
        def::<NoArgs>(
            "query_monitors",
            Read,
            false,
            "List physical monitors with name, geometry, scale, refresh rate, \
             and which workspace is currently active on each. Read-only.",
        ),
        def::<NoArgs>(
            "query_active_window",
            Read,
            false,
            "Return the currently focused window, or `null` if no window has \
             focus. Read-only.",
        ),
        def::<NoArgs>(
            "query_active_workspace",
            Read,
            false,
            "Return the currently focused workspace. Read-only.",
        ),
        def::<NoArgs>(
            "query_version",
            Read,
            false,
            "Return the live Hyprland version, commit, and build tag. Use this \
             to detect compositor-version-specific behaviour. Read-only.",
        ),
        def::<NoArgs>(
            "query_cursor_pos",
            Read,
            false,
            "Return the cursor's screen-space (x, y) position in pixels. Read-only.",
        ),
        // ---- Window state (group: window) ------------------------------------
        def::<SelectorArgs>(
            "focus_window",
            Window,
            true,
            "Move keyboard focus to a window. Reversible by focusing the \
             previously-focused window. Not recorded in the undo stack (focus \
             changes are too frequent to be useful there).",
        ),
        def::<MutNoArgs>(
            "cycle_next",
            Window,
            true,
            "Cycle focus to the next tiled window in the focused workspace. \
             No state mutation beyond focus.",
        ),
        def::<MutNoArgs>(
            "cycle_prev",
            Window,
            true,
            "Cycle focus to the previous tiled window in the focused workspace.",
        ),
        def::<MutNoArgs>(
            "toggle_floating",
            Window,
            true,
            "Toggle floating mode on the focused window. Reversible by calling \
             again. Not in undo stack.",
        ),
        def::<FullscreenArgs>(
            "set_fullscreen",
            Window,
            true,
            "Set fullscreen mode on the focused window. `maximize` covers the \
             monitor while respecting gaps and bars; `fullscreen` is true \
             exclusive. Reversible by calling with the same mode again.",
        ),
        def::<MutNoArgs>(
            "center_active",
            Window,
            true,
            "Center the focused window on its monitor. Floating windows only \
             — has no effect on tiled windows.",
        ),
        def::<MutNoArgs>(
            "pin_active",
            Window,
            true,
            "Toggle the pinned (sticky-across-workspaces) state of the focused \
             window. Floating windows only.",
        ),
        def::<DirectionArgs>(
            "move_active",
            Window,
            true,
            "Move the focused window in a direction (`l`, `r`, `u`, `d`). For \
             tiled windows this swaps with the neighbour; for floating, it \
             nudges by a fixed amount configured in Hyprland.",
        ),
        def::<ResizeArgs>(
            "resize_active",
            Window,
            true,
            "Resize the focused window by (dx, dy) pixels. Negative values \
             shrink. For tiled windows this adjusts the split ratio.",
        ),
        def::<DirectionArgs>(
            "swap_active",
            Window,
            true,
            "Swap the focused window with its neighbour in the given direction. \
             Tiled windows only.",
        ),
        // ---- Workspace (group: workspace) ------------------------------------
        def::<WorkspaceArgs>(
            "switch_workspace",
            Workspace,
            true,
            "Switch the active workspace on the focused monitor. Does not move \
             any windows.",
        ),
        def::<WorkspaceArgs>(
            "move_to_workspace",
            Workspace,
            true,
            "Move the focused window to the target workspace and follow it. \
             Recorded in the undo stack: undo will move the window back to its \
             prior workspace and re-focus it.",
        ),
        def::<WorkspaceArgs>(
            "move_to_workspace_silent",
            Workspace,
            true,
            "Move the focused window to the target workspace without changing \
             focus or visible workspace. Recorded in the undo stack.",
        ),
        def::<FocusMonitorArgs>(
            "focus_monitor",
            Workspace,
            true,
            "Focus a monitor by name (e.g. `eDP-1`) or direction (`l`, `r`, \
             `u`, `d`).",
        ),
        def::<MonitorArgs>(
            "move_workspace_to_monitor",
            Workspace,
            true,
            "Move the focused workspace to a named monitor. Windows go with it.",
        ),
        // ---- Destructive (group: destructive) --------------------------------
        def::<MutNoArgs>(
            "kill_active",
            Destructive,
            true,
            "Close the currently focused window. DESTRUCTIVE: the application \
             receives a close signal and loses unsaved state. The daemon \
             captures the killed process's command line; undo will re-spawn \
             a fresh instance (state is NOT restored). Requires `dry_run=false` \
             to actually execute.",
        ),
        def::<SelectorArgs>(
            "close_window",
            Destructive,
            true,
            "Close a specific window matched by selector. DESTRUCTIVE — same \
             caveats as `kill_active`. Recorded in the undo stack when a \
             matching window is found at call time.",
        ),
        // ---- Process spawn (group: process) ----------------------------------
        def::<ExecArgs>(
            "exec",
            Process,
            true,
            "Spawn an external process via Hyprland's exec dispatcher. \
             POTENTIALLY DANGEROUS: this runs an arbitrary shell command with \
             the user's full privileges. Not in the undo stack — spawned \
             processes are not tracked. Requires `dry_run=false`.",
        ),
        // ---- Undo (group: undo) ---------------------------------------------
        def::<MutNoArgs>(
            "undo",
            Undo,
            true,
            "Reverse the most recent reversible operation recorded by the \
             daemon. May fail if the inverse cannot be applied (e.g. a window \
             that was moved no longer exists). `dry_run=true` reports which \
             entry would be undone without applying it.",
        ),
        def::<NoArgs>(
            "undo_list",
            Undo,
            false,
            "List the daemon's undo stack: index, timestamp, and description \
             of each reversible entry. The most recent entry has index 0. \
             Read-only.",
        ),
        // ---- Snapshots (group: snapshot) -------------------------------------
        def::<SnapshotNameArgs>(
            "snapshot_save",
            Snapshot,
            true,
            "Capture the current Hyprland state (windows + workspaces + \
             monitors) as a named snapshot on disk. Use to mark a known-good \
             layout before experimenting. The snapshot can be restored later \
             via `snapshot_restore`.",
        ),
        def::<NoArgs>(
            "snapshot_list",
            Snapshot,
            false,
            "List all on-disk snapshot names. Read-only.",
        ),
        def::<SnapshotNameArgs>(
            "snapshot_diff",
            Snapshot,
            false,
            "Show what a `snapshot_restore` of this snapshot would change: \
             which windows would move, which would toggle floating, which \
             are missing, and which exist now but weren't in the snapshot. \
             Read-only. Use this to preview a restore before applying.",
        ),
        def::<SnapshotMutArgs>(
            "snapshot_restore",
            Snapshot,
            true,
            "Restore a named snapshot. Best-effort: matches live windows to \
             snapshot entries by address, then pid, then (initial_class, \
             initial_title). For each match, restores workspace and floating \
             state. Exact geometry, fullscreen mode, focus order, and \
             re-spawn of missing windows are NOT restored in v0.3. Before \
             restoring, the current state is auto-saved as \
             `_pre-restore-<unix_ts>` so this op is itself reversible.",
        ),
        def::<SnapshotNameArgs>(
            "snapshot_delete",
            Snapshot,
            true,
            "Delete a snapshot file from disk. Idempotent. Does not affect \
             live Hyprland state.",
        ),
        // ---- Rules (group: rules) -------------------------------------------
        def::<NoArgs>(
            "rules_list",
            Rules,
            false,
            "List the rules from the daemon's configured rules.toml. \
             Returns the rules array as declared in the file. The daemon \
             reads the file fresh on each call, so this reflects current \
             on-disk state; if you edit rules.toml after the daemon \
             started, the engine still uses the version it loaded at \
             startup — restart the daemon to reload. Read-only.",
        ),
        def::<NoArgs>(
            "rules_validate",
            Rules,
            false,
            "Parse and compile the rules.toml file. Returns \
             `{ ok: true, rules: N }` on success, or a structured error \
             with the offending rule label and action index. Use this to \
             check a config change before restarting the daemon. Read-only.",
        ),
        def::<NoArgs>(
            "rules_path",
            Rules,
            false,
            "Return the on-disk path the daemon reads rules from. \
             Useful for opening the file in an editor without guessing \
             the XDG resolution. Read-only.",
        ),
        // ---- Input (group: input) — NOT in default profile ------------------
        def::<InputTextArgs>(
            "input_type",
            Input,
            true,
            "Type free-form text into the focused window via wtype. \
             VERY DANGEROUS: any focused terminal will execute the typed \
             text as shell commands. Verify the focused window with \
             `query_active_window` before applying. Daemon requires \
             HYPRPILOT_DANGEROUS_INPUT_OK=1; this tool also requires the \
             `input` capability group, which is NOT in the default profile.",
        ),
        def::<InputKeysArgs>(
            "input_keys",
            Input,
            true,
            "Press a key chord in the focused window (e.g. `ctrl+shift+t`). \
             Uses wtype, so the chord goes wherever focus is. Same safety \
             gates as `input_type`.",
        ),
        def::<InputShortcutArgs>(
            "input_shortcut",
            Input,
            true,
            "Send a key chord to a specific window via Hyprland's \
             sendshortcut dispatcher. Does not require wtype and does not \
             change focus — the target window receives the chord even if \
             it is hidden on another workspace. Safer than `input_keys` \
             because it cannot accidentally fire in a terminal you didn't \
             mean, but still subject to the same capability + env-var gates.",
        ),
        def::<InputMouseMoveArgs>(
            "input_mouse_move",
            Input,
            true,
            "Move the mouse cursor via ydotool. With `absolute=true`, moves \
             to screen coordinates (x, y); otherwise moves by delta. \
             Requires ydotoold running with input-device access. Same \
             capability + env-var gates as the rest of the input surface.",
        ),
        def::<InputMouseClickArgs>(
            "input_mouse_click",
            Input,
            true,
            "Click a mouse button at the current cursor position via \
             ydotool. Button names: left, right, middle, x1/back, \
             x2/forward. Same gates as input_mouse_move.",
        ),
    ]
}

fn def<T: JsonSchema>(
    name: &'static str,
    group: ToolGroup,
    mutating: bool,
    description: &'static str,
) -> ToolDef {
    let schema_obj = schema_for!(T);
    let mut schema = serde_json::to_value(schema_obj.schema)
        .unwrap_or(Value::Object(Default::default()));
    // schemars sets `title`; strip it — MCP doesn't need it and it adds noise
    // to the LLM-visible schema.
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("title");
    }
    ToolDef { name, group, description, mutating, schema }
}

// =============================================================================
// Dispatch
// =============================================================================

/// Parse the arguments for the named tool and decide whether to forward to
/// the daemon or return a dry-run preview.
pub fn dispatch(name: &str, args: Value) -> Result<Dispatch, DispatchError> {
    match name {
        // ---- queries (always forwarded) --------------------------------------
        "query_clients" => parse_no_args(name, args, Request::QueryClients),
        "query_workspaces" => parse_no_args(name, args, Request::QueryWorkspaces),
        "query_monitors" => parse_no_args(name, args, Request::QueryMonitors),
        "query_active_window" => parse_no_args(name, args, Request::QueryActiveWindow),
        "query_active_workspace" => parse_no_args(name, args, Request::QueryActiveWorkspace),
        "query_version" => parse_no_args(name, args, Request::QueryVersion),
        "query_cursor_pos" => parse_no_args(name, args, Request::QueryCursorPos),
        "undo_list" => parse_no_args(name, args, Request::UndoList),

        // ---- mutating with selector ----------------------------------------
        "focus_window" => parse_selector(name, args, |sel| Request::DispatchFocusWindow {
            selector: sel,
        }),
        "close_window" => parse_selector(name, args, |sel| Request::DispatchCloseWindow {
            selector: sel,
        }),

        // ---- mutating with workspace ref -----------------------------------
        "switch_workspace" => parse_workspace(name, args, |ws| {
            Request::DispatchSwitchWorkspace { workspace: ws }
        }),
        "move_to_workspace" => parse_workspace(name, args, |ws| {
            Request::DispatchMoveToWorkspace { workspace: ws }
        }),
        "move_to_workspace_silent" => parse_workspace(name, args, |ws| {
            Request::DispatchMoveToWorkspaceSilent { workspace: ws }
        }),

        // ---- mutating with no/simple args ----------------------------------
        "cycle_next" => parse_mut_no_args(name, args, Request::DispatchCycleNext),
        "cycle_prev" => parse_mut_no_args(name, args, Request::DispatchCyclePrev),
        "toggle_floating" => parse_mut_no_args(name, args, Request::DispatchToggleFloating),
        "center_active" => parse_mut_no_args(name, args, Request::DispatchCenterActive),
        "pin_active" => parse_mut_no_args(name, args, Request::DispatchPinActive),
        "kill_active" => parse_mut_no_args(name, args, Request::DispatchKillActive),
        "undo" => parse_mut_no_args(name, args, Request::Undo),

        "set_fullscreen" => {
            let p: FullscreenArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::DispatchFullscreen { mode: p.mode.to_core() },
                || format!("would set fullscreen mode={}", mode_name(&p.mode)),
            )
        }

        "move_active" => {
            let p: DirectionArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::DispatchMoveActive { direction: p.direction.to_core() },
                || format!("would move active window {:?}", p.direction),
            )
        }
        "resize_active" => {
            let p: ResizeArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::DispatchResizeActive { dx: p.dx, dy: p.dy },
                || format!("would resize active window by ({}, {}) px", p.dx, p.dy),
            )
        }
        "swap_active" => {
            let p: DirectionArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::DispatchSwapActive { direction: p.direction.to_core() },
                || format!("would swap active window {:?}", p.direction),
            )
        }

        "focus_monitor" => {
            let p: FocusMonitorArgs = parse_args(name, args)?;
            let name_or_dir = p.name_or_direction.clone();
            forward_or_preview(
                p.dry_run,
                Request::DispatchFocusMonitor { name: p.name_or_direction },
                || format!("would focus monitor `{name_or_dir}`"),
            )
        }
        "move_workspace_to_monitor" => {
            let p: MonitorArgs = parse_args(name, args)?;
            let mon = p.monitor.clone();
            forward_or_preview(
                p.dry_run,
                Request::DispatchMoveWorkspaceToMonitor { monitor: p.monitor },
                || format!("would move focused workspace to monitor `{mon}`"),
            )
        }

        "exec" => {
            let p: ExecArgs = parse_args(name, args)?;
            let cmd = p.command.clone();
            forward_or_preview(
                p.dry_run,
                Request::DispatchExec { command: p.command },
                || format!("would spawn: {cmd}"),
            )
        }

        // ---- snapshots -----------------------------------------------------
        "snapshot_save" => {
            let p: SnapshotNameArgs = parse_args(name, args)?;
            Ok(Dispatch::Forward(Request::SnapshotSave { name: p.name }))
        }
        "snapshot_list" => parse_no_args(name, args, Request::SnapshotList),
        "snapshot_diff" => {
            let p: SnapshotNameArgs = parse_args(name, args)?;
            Ok(Dispatch::Forward(Request::SnapshotDiff { name: p.name }))
        }
        "snapshot_restore" => {
            let p: SnapshotMutArgs = parse_args(name, args)?;
            Ok(if p.dry_run {
                // Defer formatting to the server, which needs to call the
                // daemon to fetch the diff. Generic Preview text is too vague
                // for a restore — the operation can be dozens of mutations.
                Dispatch::PreviewSnapshotRestore { name: p.name }
            } else {
                Dispatch::Forward(Request::SnapshotRestore { name: p.name })
            })
        }
        "snapshot_delete" => {
            let p: SnapshotNameArgs = parse_args(name, args)?;
            Ok(Dispatch::Forward(Request::SnapshotDelete { name: p.name }))
        }

        // ---- rules ---------------------------------------------------------
        "rules_list" => parse_no_args(name, args, Request::RulesList),
        "rules_validate" => parse_no_args(name, args, Request::RulesValidate),
        "rules_path" => parse_no_args(name, args, Request::RulesPath),

        // ---- input ---------------------------------------------------------
        "input_type" => {
            let p: InputTextArgs = parse_args(name, args)?;
            let preview_text = p.text.clone();
            forward_or_preview(
                p.dry_run,
                Request::InputType { text: p.text },
                || {
                    format!(
                        "would type {} bytes into the focused window: {:?}",
                        preview_text.len(),
                        truncate(&preview_text, 80),
                    )
                },
            )
        }
        "input_keys" => {
            let p: InputKeysArgs = parse_args(name, args)?;
            let combo = parse_combo(name, &p.combo)?;
            let combo_label = combo.to_string();
            forward_or_preview(
                p.dry_run,
                Request::InputKeys { combo },
                || format!("would press `{combo_label}` in the focused window"),
            )
        }
        "input_shortcut" => {
            let p: InputShortcutArgs = parse_args(name, args)?;
            let combo = parse_combo(name, &p.combo)?;
            let selector = WindowSelector::parse(&p.selector).map_err(|e| {
                DispatchError::InvalidArgs { tool: name.to_string(), message: e }
            })?;
            let combo_label = combo.to_string();
            let sel_label = p.selector.clone();
            forward_or_preview(
                p.dry_run,
                Request::InputShortcut { combo, selector },
                || format!("would send `{combo_label}` to `{sel_label}`"),
            )
        }
        "input_mouse_move" => {
            let p: InputMouseMoveArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::InputMouseMove { x: p.x, y: p.y, absolute: p.absolute },
                || {
                    let mode = if p.absolute { "to absolute" } else { "by delta" };
                    format!("would move mouse {mode} ({}, {})", p.x, p.y)
                },
            )
        }
        "input_mouse_click" => {
            let p: InputMouseClickArgs = parse_args(name, args)?;
            let button = MouseButton::parse(&p.button).map_err(|e| {
                DispatchError::InvalidArgs { tool: name.to_string(), message: e.to_string() }
            })?;
            let label = format!("{button:?}");
            forward_or_preview(
                p.dry_run,
                Request::InputMouseClick { button },
                || format!("would click {label} at current cursor position"),
            )
        }

        other => Err(DispatchError::UnknownTool(other.to_string())),
    }
}

fn parse_combo(tool: &str, s: &str) -> Result<KeyCombo, DispatchError> {
    KeyCombo::parse(s).map_err(|e| DispatchError::InvalidArgs {
        tool: tool.to_string(),
        message: e.to_string(),
    })
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find a char boundary at or before `max`.
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

// ---- helpers ----------------------------------------------------------------

fn parse_args<T: for<'de> Deserialize<'de>>(
    tool: &str,
    args: Value,
) -> Result<T, DispatchError> {
    // serde rejects `null` for structs with required fields; treat `null`
    // and missing args the same as `{}` for ergonomics.
    let args = if args.is_null() { Value::Object(Default::default()) } else { args };
    serde_json::from_value(args).map_err(|e| DispatchError::InvalidArgs {
        tool: tool.to_string(),
        message: e.to_string(),
    })
}

fn parse_no_args(
    name: &str,
    args: Value,
    request: Request,
) -> Result<Dispatch, DispatchError> {
    let _: NoArgs = parse_args(name, args)?;
    Ok(Dispatch::Forward(request))
}

fn parse_mut_no_args(
    name: &str,
    args: Value,
    request: Request,
) -> Result<Dispatch, DispatchError> {
    let p: MutNoArgs = parse_args(name, args)?;
    Ok(forward_or_preview_static(p.dry_run, request, name))
}

fn parse_selector(
    name: &str,
    args: Value,
    build: impl FnOnce(WindowSelector) -> Request,
) -> Result<Dispatch, DispatchError> {
    let p: SelectorArgs = parse_args(name, args)?;
    let sel = WindowSelector::parse(&p.selector).map_err(|e| DispatchError::InvalidArgs {
        tool: name.to_string(),
        message: e,
    })?;
    let sel_str = p.selector.clone();
    forward_or_preview(p.dry_run, build(sel), || {
        format!("would {} on selector `{}`", name, sel_str)
    })
}

fn parse_workspace(
    name: &str,
    args: Value,
    build: impl FnOnce(WorkspaceRef) -> Request,
) -> Result<Dispatch, DispatchError> {
    let p: WorkspaceArgs = parse_args(name, args)?;
    let ws = WorkspaceRef::parse(&p.target).map_err(|e| DispatchError::InvalidArgs {
        tool: name.to_string(),
        message: e,
    })?;
    let ws_str = p.target.clone();
    forward_or_preview(p.dry_run, build(ws), || {
        format!("would {} target=`{}`", name, ws_str)
    })
}

fn forward_or_preview(
    dry_run: bool,
    request: Request,
    preview: impl FnOnce() -> String,
) -> Result<Dispatch, DispatchError> {
    Ok(if dry_run {
        Dispatch::Preview(preview())
    } else {
        Dispatch::Forward(request)
    })
}

fn forward_or_preview_static(dry_run: bool, request: Request, name: &str) -> Dispatch {
    if dry_run {
        Dispatch::Preview(format!("would invoke {name}"))
    } else {
        Dispatch::Forward(request)
    }
}

fn mode_name(m: &FsModeArg) -> &'static str {
    match m {
        FsModeArg::Maximize => "maximize",
        FsModeArg::Fullscreen => "fullscreen",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_no_duplicate_names() {
        let reg = registry();
        let mut names: Vec<_> = reg.iter().map(|t| t.name).collect();
        names.sort();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len, "duplicate tool name in registry");
    }

    #[test]
    fn registry_count() {
        // Lock the surface so additions are deliberate. Update this number
        // intentionally when adding/removing tools.
        assert_eq!(registry().len(), 40);
    }

    #[test]
    fn input_tools_are_in_input_group() {
        let reg = registry();
        for name in [
            "input_type",
            "input_keys",
            "input_shortcut",
            "input_mouse_move",
            "input_mouse_click",
        ] {
            let t = reg
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("missing tool `{name}` in registry"));
            assert_eq!(t.group, crate::capability::ToolGroup::Input);
            assert!(t.mutating, "input tool `{name}` must be flagged mutating");
        }
    }

    #[test]
    fn default_profile_does_not_include_input_tools() {
        let p = crate::capability::Profile::default_safe();
        for name in [
            "input_type",
            "input_keys",
            "input_shortcut",
            "input_mouse_move",
            "input_mouse_click",
        ] {
            assert!(
                !p.allows(name, crate::capability::ToolGroup::Input),
                "default profile must NOT permit `{name}`"
            );
        }
    }

    #[test]
    fn dispatch_unknown_tool() {
        let err = dispatch("does_not_exist", Value::Null).unwrap_err();
        match err {
            DispatchError::UnknownTool(s) => assert_eq!(s, "does_not_exist"),
            other => panic!("expected UnknownTool, got {other:?}"),
        }
    }

    #[test]
    fn query_clients_no_args_forwards() {
        let d = dispatch("query_clients", Value::Null).unwrap();
        assert!(matches!(d, Dispatch::Forward(Request::QueryClients)));
    }

    #[test]
    fn kill_active_dry_run_default_previews() {
        let d = dispatch("kill_active", serde_json::json!({})).unwrap();
        assert!(matches!(d, Dispatch::Preview(_)));
    }

    #[test]
    fn kill_active_explicit_apply_forwards() {
        let d = dispatch("kill_active", serde_json::json!({"dry_run": false})).unwrap();
        assert!(matches!(d, Dispatch::Forward(Request::DispatchKillActive)));
    }

    #[test]
    fn focus_window_parses_stringy_selector() {
        let d = dispatch(
            "focus_window",
            serde_json::json!({"selector": "class:Slack", "dry_run": false}),
        )
        .unwrap();
        match d {
            Dispatch::Forward(Request::DispatchFocusWindow { selector }) => {
                assert_eq!(selector, WindowSelector::Class("Slack".into()));
            }
            other => panic!("expected Forward(DispatchFocusWindow), got {other:?}"),
        }
    }

    #[test]
    fn focus_window_dry_run_schema_has_description() {
        let reg = registry();
        let tool = reg
            .iter()
            .find(|t| t.name == "focus_window")
            .expect("focus_window in registry");
        let desc = tool.schema["properties"]["dry_run"]["description"]
            .as_str()
            .expect("dry_run description should be a string");
        assert!(!desc.is_empty(), "dry_run description should be non-empty");
    }

    #[test]
    fn move_to_workspace_rejects_bad_target() {
        let err = dispatch(
            "move_to_workspace",
            serde_json::json!({"target": "not_a_workspace", "dry_run": false}),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::InvalidArgs { .. }));
    }
}
