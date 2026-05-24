//! Wire protocol for the daemon RPC.
//!
//! Newline-delimited JSON. One [`RequestEnvelope`] per line in, one
//! [`ResponseEnvelope`] per line out, matched by integer id. Stable so long
//! as the daemon and client are the same build; not a public stability
//! commitment.

use serde::{Deserialize, Serialize};

use hyprpilot_core::dispatch::{Direction, FullscreenMode};
use hyprpilot_core::selector::{WindowSelector, WorkspaceRef};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub response: Response,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Shutdown,

    QueryClients,
    QueryWorkspaces,
    QueryMonitors,
    QueryActiveWindow,
    QueryActiveWorkspace,
    QueryVersion,
    QueryCursorPos,

    DispatchKillActive,
    DispatchCloseWindow { selector: WindowSelector },
    DispatchFocusWindow { selector: WindowSelector },
    DispatchCycleNext,
    DispatchCyclePrev,
    DispatchSwitchWorkspace { workspace: WorkspaceRef },
    DispatchMoveToWorkspace { workspace: WorkspaceRef },
    DispatchMoveToWorkspaceSilent { workspace: WorkspaceRef },
    DispatchToggleFloating,
    DispatchFullscreen { mode: FullscreenMode },
    DispatchCenterActive,
    DispatchPinActive,
    DispatchMoveActive { direction: Direction },
    DispatchResizeActive { dx: i32, dy: i32 },
    DispatchSwapActive { direction: Direction },
    DispatchExec { command: String },
    DispatchFocusMonitor { name: String },
    DispatchMoveWorkspaceToMonitor { monitor: String },

    Undo,
    UndoList,

    /// Capture live state into a snapshot file under
    /// `$XDG_STATE_HOME/hyprpilot/snapshots/<name>.json`.
    SnapshotSave { name: String },
    /// List names of all on-disk snapshots.
    SnapshotList,
    /// Show what a restore would change without applying.
    SnapshotDiff { name: String },
    /// Restore a snapshot. Captures an implicit `_pre-restore-<unix_ts>`
    /// snapshot first so the operation is itself reversible.
    SnapshotRestore { name: String },
    /// Delete a snapshot file. Idempotent.
    SnapshotDelete { name: String },

    /// List currently loaded rules (re-reads the file on each call).
    /// Returns the deserialized RuleConfig as JSON. Parse errors are
    /// surfaced as `rules_load_failed`.
    RulesList,
    /// Validate the rules file: parses TOML, then compiles every rule's
    /// action list. Returns either `{ ok: true, rules: N }` or details
    /// of the first parse / compile error.
    RulesValidate,
    /// Returns the on-disk path the daemon would load from.
    RulesPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Ok { result: serde_json::Value },
    Err { error: RpcError },
}

impl Response {
    pub fn ok<T: Serialize>(value: T) -> Self {
        Response::Ok {
            result: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        }
    }
    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        Response::Err { error: RpcError { code: code.into(), message: message.into() } }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
}

/// Stable error codes used in RPC responses.
pub mod codes {
    pub const PARSE: &str = "parse_error";
    pub const NO_INSTANCE: &str = "no_instance";
    pub const HYPRLAND_REJECTED: &str = "hyprland_rejected";
    pub const HYPRLAND_UNKNOWN: &str = "hyprland_unknown_dispatcher";
    pub const HYPRLAND_IO: &str = "hyprland_io";
    pub const UNDO_EMPTY: &str = "undo_empty";
    pub const UNDO_FAILED: &str = "undo_failed";
    pub const SNAPSHOT_NOT_FOUND: &str = "snapshot_not_found";
    pub const SNAPSHOT_INVALID_NAME: &str = "snapshot_invalid_name";
    pub const SNAPSHOT_IO: &str = "snapshot_io";
    pub const RULES_LOAD_FAILED: &str = "rules_load_failed";
    pub const INTERNAL: &str = "internal";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoListEntry {
    pub index: usize,
    pub timestamp_unix: i64,
    pub description: String,
}
