//! In-memory undo stack for mutating Hyprland operations.
//!
//! Hyprland exposes no transaction primitive, so "undo" is always best-effort
//! reversal of the recorded operation. For `kill`, we re-spawn the captured
//! command line — state and PID are gone, but the binary comes back. For
//! `move-to-workspace`, we move the same window back to its prior workspace.
//!
//! v0.1: in-memory only, fixed-size ring buffer. Persistence is v0.2.

use std::collections::VecDeque;
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
}

impl UndoStack {
    pub fn with_capacity(capacity: usize) -> Self {
        Self { entries: VecDeque::with_capacity(capacity), capacity }
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
    }

    pub fn pop(&mut self) -> Option<UndoEntry> {
        self.entries.pop_back()
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
    fn shell_quote_safe() {
        assert_eq!(shell_quote("kitty".into()), "kitty");
        assert_eq!(shell_quote("/usr/bin/kitty".into()), "/usr/bin/kitty");
        assert_eq!(shell_quote("a b".into()), "'a b'");
        assert_eq!(shell_quote("can't".into()), "'can'\\''t'");
        assert_eq!(shell_quote(String::new()), "''");
    }
}
