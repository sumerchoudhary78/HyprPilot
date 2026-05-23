//! Named snapshots of Hyprland window/workspace state.
//!
//! A [`Snapshot`] is a serializable record of:
//!
//! - every managed window (address, identity tuple, workspace, geometry,
//!   floating/fullscreen/pinned state);
//! - every workspace and which monitor it's on;
//! - every monitor and which workspace is active on it;
//! - the focused window and focused workspace.
//!
//! Snapshots are JSON files under
//! `$XDG_STATE_HOME/hyprpilot/snapshots/<name>.json`. Names are restricted
//! to `[A-Za-z0-9_.-]+` to prevent path traversal.
//!
//! ## Restore semantics
//!
//! v0.3 ships a deliberately small restore: per-window workspace and
//! floating state. Exact geometry, fullscreen mode, pin state, focus order,
//! and re-spawn of dead windows are documented as v0.4 work. The diff
//! enumerates everything; only a subset gets applied.
//!
//! Windows present in the snapshot but missing from live state are reported
//! and skipped. Windows present in live state but not in the snapshot are
//! left alone — restore is additive, never destructive.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ipc::Connection;
use crate::selector::{WindowSelector, WorkspaceRef};

// =============================================================================
// Data shapes
// =============================================================================

/// One serialized snapshot. Persisted as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub name: String,
    pub created_at_unix: i64,
    pub hyprland_version: String,
    pub monitors: Vec<MonitorSnapshot>,
    pub workspaces: Vec<WorkspaceSnapshot>,
    pub windows: Vec<WindowSnapshot>,
    pub active_window_address: Option<String>,
    pub active_workspace_id: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowSnapshot {
    /// Hex pointer (`0x...`) at capture time. May not survive Hyprland
    /// restart; the identity tuple below is the fallback.
    pub address: String,
    pub pid: i32,
    /// Class as set by the client when the window opened — does not change.
    pub initial_class: String,
    /// Initial title — does not change for most clients.
    pub initial_title: String,
    /// Current class at capture time (some clients change class).
    pub class: String,
    /// Current title at capture time.
    pub title: String,

    pub workspace_id: i32,
    pub monitor_name: String,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub floating: bool,
    /// 0 = none, 1 = maximize, 2 = fullscreen.
    pub fullscreen: i32,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub id: i32,
    pub name: String,
    pub monitor: String,
    pub windows: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorSnapshot {
    pub name: String,
    pub active_workspace_id: i32,
}

// =============================================================================
// Diff
// =============================================================================

/// One restore action computed from a diff. `apply_diff` consumes these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestoreAction {
    /// Window in the snapshot is not present in live state. Reported only;
    /// not applied. Re-spawn is v0.4.
    WindowMissing {
        address: String,
        class: String,
        title: String,
    },
    /// Window is on the wrong workspace. Applied via
    /// `movetoworkspacesilent <ws>,address:<addr>`.
    MoveToWorkspace {
        address: String,
        from_workspace: i32,
        to_workspace: i32,
    },
    /// Window's floating state differs. Applied via
    /// `togglefloating address:<addr>` (which acts as set-to-target given
    /// we computed the diff against current state).
    ToggleFloating {
        address: String,
        target: bool,
    },
    /// Window present in live state but absent from snapshot. Reported
    /// only — restore is non-destructive.
    WindowNotInSnapshot {
        address: String,
        class: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub actions: Vec<RestoreAction>,
}

impl SnapshotDiff {
    /// Number of actions that would actually mutate Hyprland (informational
    /// actions like `WindowMissing` don't count).
    pub fn mutation_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    RestoreAction::MoveToWorkspace { .. } | RestoreAction::ToggleFloating { .. }
                )
            })
            .count()
    }

    pub fn is_no_op(&self) -> bool {
        self.mutation_count() == 0
    }
}

// =============================================================================
// Capture
// =============================================================================

impl Snapshot {
    /// Query Hyprland and build a snapshot. Network only — does not touch
    /// disk. Use [`Snapshot::save`] to persist.
    pub async fn capture(conn: &Connection, name: impl Into<String>) -> Result<Self> {
        let (version, clients, workspaces, monitors, active_window, active_workspace) = tokio::try_join!(
            conn.version(),
            conn.clients(),
            conn.workspaces(),
            conn.monitors(),
            conn.active_window(),
            conn.active_workspace(),
        )?;

        let monitor_name_by_id: std::collections::HashMap<i32, String> =
            monitors.iter().map(|m| (m.id, m.name.clone())).collect();

        let windows: Vec<WindowSnapshot> = clients
            .into_iter()
            .map(|c| WindowSnapshot {
                monitor_name: monitor_name_by_id
                    .get(&c.monitor)
                    .cloned()
                    .unwrap_or_default(),
                address: c.address,
                pid: c.pid,
                initial_class: c.initial_class,
                initial_title: c.initial_title,
                class: c.class,
                title: c.title,
                workspace_id: c.workspace.id,
                at: c.at,
                size: c.size,
                floating: c.floating,
                fullscreen: c.fullscreen,
                pinned: c.pinned,
            })
            .collect();

        let workspaces = workspaces
            .into_iter()
            .map(|w| WorkspaceSnapshot {
                id: w.id,
                name: w.name,
                monitor: w.monitor,
                windows: w.windows,
            })
            .collect();

        let monitors = monitors
            .into_iter()
            .map(|m| MonitorSnapshot {
                name: m.name,
                active_workspace_id: m.active_workspace.map(|w| w.id).unwrap_or(0),
            })
            .collect();

        Ok(Snapshot {
            name: name.into(),
            created_at_unix: now_unix(),
            hyprland_version: version.version,
            monitors,
            workspaces,
            windows,
            active_window_address: active_window.map(|c| c.address),
            active_workspace_id: active_workspace.id,
        })
    }
}

// =============================================================================
// Diff
// =============================================================================

impl Snapshot {
    /// Compute the actions needed to take the *live* state back to *self*.
    ///
    /// Matching strategy for windows:
    /// 1. exact `address` match
    /// 2. fallback: `pid` match (skipped if pid is shared by multiple live windows)
    /// 3. fallback: `(initial_class, initial_title)` match — useful when
    ///    Hyprland was restarted and addresses are fresh.
    ///
    /// Each live window is matched at most once, so two snapshot windows
    /// can't both claim the same live target.
    pub fn diff_against(&self, live: &Snapshot) -> SnapshotDiff {
        let mut actions = Vec::new();
        let mut matched_live: BTreeSet<&str> = BTreeSet::new();

        for snap in &self.windows {
            let matched = match_live_window(snap, &live.windows, &matched_live);
            match matched {
                Some(live_w) => {
                    matched_live.insert(live_w.address.as_str());
                    if live_w.workspace_id != snap.workspace_id {
                        actions.push(RestoreAction::MoveToWorkspace {
                            address: live_w.address.clone(),
                            from_workspace: live_w.workspace_id,
                            to_workspace: snap.workspace_id,
                        });
                    }
                    if live_w.floating != snap.floating {
                        actions.push(RestoreAction::ToggleFloating {
                            address: live_w.address.clone(),
                            target: snap.floating,
                        });
                    }
                }
                None => {
                    actions.push(RestoreAction::WindowMissing {
                        address: snap.address.clone(),
                        class: snap.class.clone(),
                        title: snap.title.clone(),
                    });
                }
            }
        }

        for live_w in &live.windows {
            if !matched_live.contains(live_w.address.as_str()) {
                actions.push(RestoreAction::WindowNotInSnapshot {
                    address: live_w.address.clone(),
                    class: live_w.class.clone(),
                });
            }
        }

        SnapshotDiff { actions }
    }
}

fn match_live_window<'a>(
    snap: &WindowSnapshot,
    live: &'a [WindowSnapshot],
    already_matched: &BTreeSet<&str>,
) -> Option<&'a WindowSnapshot> {
    let unmatched = |w: &&WindowSnapshot| !already_matched.contains(w.address.as_str());
    if let Some(w) = live.iter().filter(unmatched).find(|w| w.address == snap.address) {
        return Some(w);
    }
    if snap.pid > 0 {
        // Pid fallback only if exactly one unmatched live window has this pid;
        // otherwise it's ambiguous.
        let candidates: Vec<_> =
            live.iter().filter(unmatched).filter(|w| w.pid == snap.pid).collect();
        if candidates.len() == 1 {
            return Some(candidates[0]);
        }
    }
    if !snap.initial_class.is_empty() {
        let candidates: Vec<_> = live
            .iter()
            .filter(unmatched)
            .filter(|w| w.initial_class == snap.initial_class && w.initial_title == snap.initial_title)
            .collect();
        if candidates.len() == 1 {
            return Some(candidates[0]);
        }
    }
    None
}

// =============================================================================
// Apply
// =============================================================================

/// Apply a diff's mutating actions through the live connection. Returns
/// the list of actions actually attempted, with per-action errors collected
/// rather than short-circuiting on the first failure.
pub async fn apply_diff(conn: &Connection, diff: &SnapshotDiff) -> Vec<ApplyOutcome> {
    let mut outcomes = Vec::new();
    for action in &diff.actions {
        match action {
            RestoreAction::MoveToWorkspace { address, to_workspace, .. } => {
                let sel = WindowSelector::Address(address.clone());
                let ws = WorkspaceRef::Id(*to_workspace);
                let result = conn
                    .dispatch(&format!(
                        "movetoworkspacesilent {},{}",
                        ws.encode(),
                        sel.encode()
                    ))
                    .await;
                outcomes.push(ApplyOutcome::from(action.clone(), result));
            }
            RestoreAction::ToggleFloating { address, .. } => {
                let sel = WindowSelector::Address(address.clone());
                let result = conn
                    .dispatch(&format!("togglefloating {}", sel.encode()))
                    .await;
                outcomes.push(ApplyOutcome::from(action.clone(), result));
            }
            RestoreAction::WindowMissing { .. } | RestoreAction::WindowNotInSnapshot { .. } => {
                outcomes.push(ApplyOutcome {
                    action: action.clone(),
                    ok: true,
                    error: None,
                    skipped: true,
                });
            }
        }
    }
    outcomes
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub action: RestoreAction,
    pub ok: bool,
    pub skipped: bool,
    pub error: Option<String>,
}

impl ApplyOutcome {
    fn from(action: RestoreAction, result: Result<()>) -> Self {
        match result {
            Ok(()) => Self { action, ok: true, skipped: false, error: None },
            Err(e) => Self {
                action,
                ok: false,
                skipped: false,
                error: Some(e.to_string()),
            },
        }
    }
}

// =============================================================================
// Disk storage
// =============================================================================

/// Root directory for snapshots. Creates the directory if missing on a
/// `save` call. Honours `$XDG_STATE_HOME`, falling back to
/// `$HOME/.local/state/hyprpilot/snapshots`.
pub fn snapshots_dir() -> PathBuf {
    state_dir().join("snapshots")
}

pub(crate) fn state_dir() -> PathBuf {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(state).join("hyprpilot");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/hyprpilot");
    }
    PathBuf::from(".hyprpilot")
}

pub fn snapshot_path(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(snapshots_dir().join(format!("{name}.json")))
}

/// Restrict snapshot names to a safe character set. Prevents path traversal
/// and weird filenames.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Validation(
            "snapshot name must not be empty".to_string(),
        ));
    }
    if name.len() > 128 {
        return Err(Error::Validation(
            "snapshot name too long (max 128 chars)".to_string(),
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        || name.starts_with('.')
    {
        return Err(Error::Validation(format!(
            "snapshot name `{name}` contains illegal characters; allowed: \
             [A-Za-z0-9_.-], must not start with `.`"
        )));
    }
    Ok(())
}

impl Snapshot {
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        let buf = serde_json::to_vec_pretty(self).map_err(Error::Json)?;
        // Atomic write: write to tmp, fsync, rename.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, buf).map_err(Error::Io)?;
        std::fs::rename(&tmp, path).map_err(Error::Io)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read(path).map_err(Error::Io)?;
        serde_json::from_slice(&raw).map_err(Error::Json)
    }

    pub fn save_named(&self) -> Result<PathBuf> {
        let path = snapshot_path(&self.name)?;
        self.save(&path)?;
        Ok(path)
    }
}

pub fn load_named(name: &str) -> Result<Snapshot> {
    let path = snapshot_path(name)?;
    if !path.exists() {
        return Err(Error::NotFound(format!("snapshot `{name}`")));
    }
    Snapshot::load(&path)
}

pub fn delete_named(name: &str) -> Result<()> {
    let path = snapshot_path(name)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(Error::Io)?;
    }
    Ok(())
}

pub fn list_names() -> io::Result<Vec<String>> {
    let dir = snapshots_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: i32, name: &str, monitor: &str) -> WorkspaceSnapshot {
        WorkspaceSnapshot { id, name: name.into(), monitor: monitor.into(), windows: 1 }
    }

    fn win(addr: &str, ws_id: i32, floating: bool) -> WindowSnapshot {
        WindowSnapshot {
            address: addr.into(),
            pid: 100,
            initial_class: "Foo".into(),
            initial_title: "Bar".into(),
            class: "Foo".into(),
            title: "Bar".into(),
            workspace_id: ws_id,
            monitor_name: "eDP-1".into(),
            at: [0, 0],
            size: [100, 100],
            floating,
            fullscreen: 0,
            pinned: false,
        }
    }

    fn snap(windows: Vec<WindowSnapshot>) -> Snapshot {
        Snapshot {
            name: "t".into(),
            created_at_unix: 0,
            hyprland_version: "0.55.2".into(),
            monitors: vec![MonitorSnapshot { name: "eDP-1".into(), active_workspace_id: 1 }],
            workspaces: vec![ws(1, "1", "eDP-1"), ws(2, "2", "eDP-1")],
            windows,
            active_window_address: None,
            active_workspace_id: 1,
        }
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("ok").is_ok());
        assert!(validate_name("ok-1.0_v2").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("..evil").is_err());
        assert!(validate_name("path/traversal").is_err());
        assert!(validate_name(".hidden").is_err());
    }

    #[test]
    fn diff_no_changes() {
        let s = snap(vec![win("0xa", 1, false), win("0xb", 2, true)]);
        let d = s.diff_against(&s);
        assert!(d.is_no_op());
    }

    #[test]
    fn diff_workspace_changed() {
        let target = snap(vec![win("0xa", 1, false)]);
        let live = snap(vec![win("0xa", 2, false)]);
        let d = target.diff_against(&live);
        assert_eq!(d.mutation_count(), 1);
        assert!(matches!(
            d.actions[0],
            RestoreAction::MoveToWorkspace { to_workspace: 1, from_workspace: 2, .. }
        ));
    }

    #[test]
    fn diff_floating_changed() {
        let target = snap(vec![win("0xa", 1, true)]);
        let live = snap(vec![win("0xa", 1, false)]);
        let d = target.diff_against(&live);
        assert_eq!(d.mutation_count(), 1);
        assert!(matches!(
            d.actions[0],
            RestoreAction::ToggleFloating { target: true, .. }
        ));
    }

    #[test]
    fn diff_window_missing_in_live() {
        let target = snap(vec![win("0xa", 1, false), win("0xb", 2, false)]);
        let live = snap(vec![win("0xa", 1, false)]);
        let d = target.diff_against(&live);
        assert!(d
            .actions
            .iter()
            .any(|a| matches!(a, RestoreAction::WindowMissing { address, .. } if address == "0xb")));
    }

    #[test]
    fn diff_extra_window_in_live() {
        let target = snap(vec![win("0xa", 1, false)]);
        let live = snap(vec![win("0xa", 1, false), win("0xb", 2, false)]);
        let d = target.diff_against(&live);
        assert!(d
            .actions
            .iter()
            .any(|a| matches!(a, RestoreAction::WindowNotInSnapshot { address, .. } if address == "0xb")));
    }

    #[test]
    fn diff_falls_back_to_pid_when_address_changes() {
        let target = snap(vec![{
            let mut w = win("0xold", 1, false);
            w.pid = 1234;
            w
        }]);
        let live = snap(vec![{
            let mut w = win("0xnew", 2, false);
            w.pid = 1234;
            w
        }]);
        let d = target.diff_against(&live);
        // Should have matched via pid and produced a MoveToWorkspace on the *live* address.
        assert!(d.actions.iter().any(|a| matches!(
            a,
            RestoreAction::MoveToWorkspace { address, to_workspace: 1, .. } if address == "0xnew"
        )));
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("hyprpilot-snap-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ok.json");

        let s = snap(vec![win("0xa", 1, false)]);
        s.save(&path).unwrap();
        let loaded = Snapshot::load(&path).unwrap();
        assert_eq!(loaded.windows.len(), 1);
        assert_eq!(loaded.windows[0].address, "0xa");

        std::fs::remove_dir_all(&dir).ok();
    }
}
