//! In-memory undo stack for mutating Hyprland operations.
//!
//! Hyprland exposes no transaction primitive, so "undo" is always best-effort
//! reversal of the recorded operation. For `kill`, we re-spawn the captured
//! command line — state and PID are gone, but the binary comes back. For
//! `move-to-workspace`, we move the same window back to its prior workspace.
//!
//! v0.1: in-memory only, fixed-size ring buffer. Persistence is v0.2.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use hyprpilot_core::selector::{WindowSelector, WorkspaceRef};
use hyprpilot_core::{Connection, Result as CoreResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InverseAction {
    /// Re-spawn a process from its captured command line.
    Spawn { command: String },
    /// Move a window back to a workspace, optionally re-focusing it.
    MoveToWorkspace {
        selector: WindowSelector,
        workspace: WorkspaceRef,
        silent: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    pub timestamp_unix: i64,
    pub description: String,
    pub inverse: InverseAction,
}

#[derive(Debug, Default)]
pub struct UndoStack {
    entries: VecDeque<UndoEntry>,
    capacity: usize,
    /// Optional disk path. When set, every mutating operation writes a
    /// best-effort persistence on a temp file + rename.
    persist_path: Option<PathBuf>,
}

impl UndoStack {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            persist_path: None,
        }
    }

    /// Enable persistence. If the file exists and parses cleanly, the
    /// existing entries are loaded; older entries beyond `capacity` are
    /// dropped. If the file is missing, the stack starts empty. If the
    /// file exists but is malformed, [`LoadOutcome::Corrupt`] is returned —
    /// the caller decides whether to start fresh or abort.
    pub fn enable_persistence(&mut self, path: PathBuf) -> LoadOutcome {
        let outcome = if path.exists() {
            match Self::read(&path) {
                Ok(entries) => {
                    self.entries = entries.into_iter().collect();
                    while self.entries.len() > self.capacity {
                        self.entries.pop_front();
                    }
                    LoadOutcome::Loaded(self.entries.len())
                }
                Err(e) => LoadOutcome::Corrupt(e),
            }
        } else {
            LoadOutcome::Empty
        };
        self.persist_path = Some(path);
        outcome
    }

    pub fn push(&mut self, description: impl Into<String>, inverse: InverseAction) {
        let entry = UndoEntry {
            timestamp_unix: now_unix(),
            description: description.into(),
            inverse,
        };
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        self.persist();
    }

    pub fn pop(&mut self) -> Option<UndoEntry> {
        let out = self.entries.pop_back();
        if out.is_some() {
            self.persist();
        }
        out
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &UndoEntry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn persist_path(&self) -> Option<&Path> {
        self.persist_path.as_deref()
    }

    /// Best-effort: log + swallow errors. Persistence is non-essential to
    /// the daemon's correctness, so we don't let disk I/O fail an undo op.
    fn persist(&self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        if let Err(e) = Self::write(path, &self.entries) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "failed to persist undo stack; continuing in memory"
            );
        }
    }

    fn read(path: &Path) -> std::io::Result<Vec<UndoEntry>> {
        let raw = std::fs::read(path)?;
        serde_json::from_slice(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    fn write(path: &Path, entries: &VecDeque<UndoEntry>) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let buf = serde_json::to_vec_pretty(&entries.iter().collect::<Vec<_>>())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, buf)?;
        std::fs::rename(&tmp, path)
    }
}

/// Result of `enable_persistence`. Surfaced to the daemon startup path so
/// the operator sees what happened.
#[derive(Debug)]
pub enum LoadOutcome {
    /// File loaded; usize = number of entries restored.
    Loaded(usize),
    /// File did not exist; stack starts empty.
    Empty,
    /// File existed but could not be parsed. Stack is left empty; the
    /// caller decides whether to overwrite or abort.
    Corrupt(std::io::Error),
}

/// Default on-disk location for the undo stack.
pub fn default_persist_path() -> PathBuf {
    let base = if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(state).join("hyprpilot")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/state/hyprpilot")
    } else {
        PathBuf::from(".hyprpilot")
    };
    base.join("undo.json")
}

pub async fn apply_inverse(conn: &Connection, action: &InverseAction) -> CoreResult<()> {
    match action {
        InverseAction::Spawn { command } => conn.exec(command).await,
        InverseAction::MoveToWorkspace { selector, workspace, silent } => {
            conn.focus_window(selector).await?;
            if *silent {
                conn.move_active_to_workspace_silent(workspace).await
            } else {
                conn.move_active_to_workspace(workspace).await
            }
        }
    }
}

/// Read `/proc/<pid>/cmdline`, the null-separated argv. Returns a shell-safe
/// reconstruction. Used to capture an inverse for `killactive`.
pub fn read_proc_cmdline(pid: i32) -> Option<String> {
    let path = format!("/proc/{pid}/cmdline");
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() {
        return None;
    }
    let mut parts = bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned());
    let first = parts.next()?;
    let rest: Vec<String> = parts.map(shell_quote).collect();
    let quoted_first = shell_quote(first);
    if rest.is_empty() {
        Some(quoted_first)
    } else {
        Some(format!("{quoted_first} {}", rest.join(" ")))
    }
}

fn shell_quote(s: String) -> String {
    if !s.is_empty() && s.bytes().all(|b| {
        matches!(b,
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' |
            b'_' | b'-' | b'.' | b'/' | b'=' | b'+' | b':' | b',')
    }) {
        return s;
    }
    // Wrap in single quotes; escape embedded single quotes the POSIX way.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_evicts_oldest() {
        let mut s = UndoStack::with_capacity(2);
        s.push("a", InverseAction::Spawn { command: "x".into() });
        s.push("b", InverseAction::Spawn { command: "y".into() });
        s.push("c", InverseAction::Spawn { command: "z".into() });
        assert_eq!(s.len(), 2);
        assert_eq!(s.pop().unwrap().description, "c");
        assert_eq!(s.pop().unwrap().description, "b");
        assert!(s.pop().is_none());
    }

    #[test]
    fn persistence_round_trip() {
        let path = std::env::temp_dir()
            .join(format!("hyprpilot-undo-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // First lifetime: push two entries.
        {
            let mut s = UndoStack::with_capacity(8);
            s.enable_persistence(path.clone());
            s.push("first", InverseAction::Spawn { command: "echo 1".into() });
            s.push("second", InverseAction::Spawn { command: "echo 2".into() });
            assert_eq!(s.len(), 2);
        }

        // Second lifetime: same path. Entries restored.
        {
            let mut s = UndoStack::with_capacity(8);
            let outcome = s.enable_persistence(path.clone());
            assert!(matches!(outcome, LoadOutcome::Loaded(2)));
            assert_eq!(s.len(), 2);
            let popped = s.pop().unwrap();
            assert_eq!(popped.description, "second");
        }

        // Third lifetime: pop persisted, only `first` remains.
        {
            let mut s = UndoStack::with_capacity(8);
            let outcome = s.enable_persistence(path.clone());
            assert!(matches!(outcome, LoadOutcome::Loaded(1)));
            let popped = s.pop().unwrap();
            assert_eq!(popped.description, "first");
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_file_is_surfaced() {
        let path = std::env::temp_dir()
            .join(format!("hyprpilot-undo-corrupt-{}.json", std::process::id()));
        std::fs::write(&path, b"not valid json").unwrap();
        let mut s = UndoStack::with_capacity(8);
        let outcome = s.enable_persistence(path.clone());
        assert!(matches!(outcome, LoadOutcome::Corrupt(_)));
        assert!(s.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn shell_quote_safe() {
        assert_eq!(shell_quote("kitty".into()), "kitty");
        assert_eq!(shell_quote("/usr/bin/kitty".into()), "/usr/bin/kitty");
        assert_eq!(shell_quote("a b".into()), "'a b'");
        assert_eq!(shell_quote("can't".into()), "'can'\\''t'");
        assert_eq!(shell_quote(String::new()), "''");
    }
}
