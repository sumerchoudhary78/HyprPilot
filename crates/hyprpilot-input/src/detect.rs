//! Backend detection — probe `$PATH` for wtype and ydotool.
//!
//! `sendshortcut` is provided by Hyprland itself, so it's always
//! considered available; presence checks belong in the daemon (which
//! holds the Hyprland connection).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendAvailability {
    /// Path to `wtype` if installed.
    pub wtype: Option<PathBuf>,
    /// Path to `ydotool` if installed.
    pub ydotool: Option<PathBuf>,
}

impl BackendAvailability {
    /// Probe `$PATH` once. Cheap — two file-system lookups. Run at
    /// daemon startup and cache.
    pub fn detect() -> Self {
        Self {
            wtype: which("wtype"),
            ydotool: which("ydotool"),
        }
    }

    pub fn has_wtype(&self) -> bool {
        self.wtype.is_some()
    }

    pub fn has_ydotool(&self) -> bool {
        self.ydotool.is_some()
    }
}

/// `which`-style lookup. Walks `$PATH`, returns the first executable
/// match. Returns None on any error rather than failing loudly — the
/// caller surfaces "not available" as a typed error.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_a_common_unix_binary() {
        // `sh` is on every Unix box we care about; this verifies the
        // PATH-walking logic does what it says without depending on
        // wtype/ydotool being installed.
        let r = which("sh");
        assert!(r.is_some(), "expected sh on PATH");
    }

    #[test]
    fn which_returns_none_for_nonexistent() {
        let r = which("this_binary_definitely_does_not_exist_zzz");
        assert!(r.is_none());
    }

    #[test]
    fn detect_does_not_panic() {
        let _ = BackendAvailability::detect();
    }
}
