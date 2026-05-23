//! Capability profiles.
//!
//! A profile names a set of tool *groups* the agent is allowed to use. Tools
//! are grouped by intent (read-only, window movement, workspace, destructive,
//! process spawn, undo); profiles allow groups, not individual tools, to keep
//! the surface human-reviewable.
//!
//! Profiles live as TOML files in `~/.config/hyprpilot/profiles/<name>.toml`.
//! A built-in `default` profile is used when no profile is specified or the
//! requested profile file is missing.
//!
//! Two layers of gating apply:
//! - `tools/list` returns only allowed tools, so the agent does not learn
//!   about tools it cannot call.
//! - `tools/call` re-checks; an out-of-policy call returns
//!   [`ErrorCode::CapabilityDenied`].
//!
//! Profiles are loaded at startup. Editing a profile while the server runs
//! has no effect; restart the MCP server.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Tool groups. Tools are tagged with exactly one group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGroup {
    /// Read-only queries: clients, workspaces, monitors, version, etc.
    Read,
    /// Focus, move, resize, fullscreen, float, pin — reversible window state.
    Window,
    /// Switch / send to workspace, move workspace to monitor, focus monitor.
    Workspace,
    /// Close / kill — destructive. State may not be recoverable via undo.
    Destructive,
    /// Spawn arbitrary processes via Hyprland's `exec` dispatcher.
    Process,
    /// Undo and inspect the daemon's undo stack.
    Undo,
}

impl ToolGroup {
    pub fn name(self) -> &'static str {
        match self {
            ToolGroup::Read => "read",
            ToolGroup::Window => "window",
            ToolGroup::Workspace => "workspace",
            ToolGroup::Destructive => "destructive",
            ToolGroup::Process => "process",
            ToolGroup::Undo => "undo",
        }
    }
}

/// A loaded capability profile.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub allow: HashSet<ToolGroup>,
    /// Tool names (not groups) explicitly denied. Overrides `allow`.
    pub deny_tools: HashSet<String>,
    /// Tool names explicitly allowed regardless of group membership.
    pub allow_tools: HashSet<String>,
}

impl Profile {
    /// Built-in safe-by-default profile. Read, window, workspace, undo. No
    /// destructive, no process spawn.
    pub fn default_safe() -> Self {
        let mut allow = HashSet::new();
        allow.insert(ToolGroup::Read);
        allow.insert(ToolGroup::Window);
        allow.insert(ToolGroup::Workspace);
        allow.insert(ToolGroup::Undo);
        Self {
            name: "default".into(),
            allow,
            deny_tools: HashSet::new(),
            allow_tools: HashSet::new(),
        }
    }

    /// Allow everything. Use sparingly.
    pub fn unrestricted() -> Self {
        let mut allow = HashSet::new();
        for g in [
            ToolGroup::Read,
            ToolGroup::Window,
            ToolGroup::Workspace,
            ToolGroup::Destructive,
            ToolGroup::Process,
            ToolGroup::Undo,
        ] {
            allow.insert(g);
        }
        Self {
            name: "unrestricted".into(),
            allow,
            deny_tools: HashSet::new(),
            allow_tools: HashSet::new(),
        }
    }

    pub fn allows(&self, tool_name: &str, group: ToolGroup) -> bool {
        if self.deny_tools.contains(tool_name) {
            return false;
        }
        if self.allow_tools.contains(tool_name) {
            return true;
        }
        self.allow.contains(&group)
    }
}

/// Wire form of a profile on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    allow: Vec<ToolGroup>,
    #[serde(default)]
    allow_tools: Vec<String>,
    #[serde(default)]
    deny_tools: Vec<String>,
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("profile file not found: {0}")]
    NotFound(PathBuf),
    #[error("failed to read profile file {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("failed to parse profile file {path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
}

/// Load a named profile from `~/.config/hyprpilot/profiles/<name>.toml`.
pub fn load_named(name: &str) -> Result<Profile, LoadError> {
    let path = profile_path(name);
    load_from(&path, name)
}

/// Load a profile from a specific path.
pub fn load_from(path: &Path, name: &str) -> Result<Profile, LoadError> {
    if !path.exists() {
        return Err(LoadError::NotFound(path.to_path_buf()));
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| LoadError::Io { path: path.to_path_buf(), source: e })?;
    let file: ProfileFile = toml::from_str(&raw)
        .map_err(|e| LoadError::Parse { path: path.to_path_buf(), source: e })?;
    Ok(Profile {
        name: name.to_string(),
        allow: file.allow.into_iter().collect(),
        allow_tools: file.allow_tools.into_iter().collect(),
        deny_tools: file.deny_tools.into_iter().collect(),
    })
}

/// `$XDG_CONFIG_HOME/hyprpilot/profiles/<name>.toml`, or
/// `$HOME/.config/hyprpilot/profiles/<name>.toml`.
pub fn profile_path(name: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("hyprpilot").join("profiles").join(format!("{name}.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allows_safe_groups() {
        let p = Profile::default_safe();
        assert!(p.allows("query_clients", ToolGroup::Read));
        assert!(p.allows("focus_window", ToolGroup::Window));
        assert!(!p.allows("kill_active", ToolGroup::Destructive));
        assert!(!p.allows("exec", ToolGroup::Process));
    }

    #[test]
    fn deny_tools_overrides_group() {
        let mut p = Profile::default_safe();
        p.deny_tools.insert("focus_window".into());
        assert!(!p.allows("focus_window", ToolGroup::Window));
        assert!(p.allows("cycle_next", ToolGroup::Window));
    }

    #[test]
    fn allow_tools_extends_grant() {
        let mut p = Profile::default_safe();
        p.allow_tools.insert("kill_active".into());
        assert!(p.allows("kill_active", ToolGroup::Destructive));
    }

    #[test]
    fn profile_round_trip() {
        let dir = tempdir();
        let path = dir.join("test.toml");
        std::fs::write(
            &path,
            r#"
            allow = ["read", "window"]
            allow_tools = ["kill_active"]
            deny_tools = ["exec"]
            "#,
        )
        .unwrap();
        let p = load_from(&path, "test").unwrap();
        assert!(p.allow.contains(&ToolGroup::Read));
        assert!(p.allow.contains(&ToolGroup::Window));
        assert!(p.allow_tools.contains("kill_active"));
        assert!(p.deny_tools.contains("exec"));
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("hyprpilot-test-{}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
