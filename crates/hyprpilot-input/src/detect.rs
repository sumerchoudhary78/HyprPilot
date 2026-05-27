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
    /// Filesystem path to `ydotoold`'s Unix socket. Existence is
    /// re-checked before each ydotool invocation so the runner can fail
    /// with an actionable hint instead of ydotool's silent exit-2.
    pub ydotoold_socket: PathBuf,
}

/// Legacy ydotoold socket location, used only when neither
/// `$YDOTOOL_SOCKET` nor `$XDG_RUNTIME_DIR` resolves a socket.
pub const DEFAULT_YDOTOOLD_SOCKET: &str = "/tmp/.ydotool_socket";

impl BackendAvailability {
    /// Probe `$PATH` once. Cheap — a handful of file-system lookups.
    /// Run at daemon startup and cache.
    pub fn detect() -> Self {
        Self {
            wtype: which("wtype"),
            ydotool: which("ydotool"),
            ydotoold_socket: ydotoold_socket_path(),
        }
    }

    pub fn has_wtype(&self) -> bool {
        self.wtype.is_some()
    }

    pub fn has_ydotool(&self) -> bool {
        self.ydotool.is_some()
    }

    /// True if `ydotoold`'s socket appears to exist on disk. Cheap
    /// `stat` — re-run each call so a user starting ydotoold after the
    /// daemon doesn't have to restart anything.
    pub fn ydotoold_reachable(&self) -> bool {
        self.ydotoold_socket.exists()
    }
}

/// Resolve ydotoold's socket path. Resolution order, matching how modern
/// ydotoold picks its socket:
///
/// 1. `$YDOTOOL_SOCKET` — explicit override, always wins.
/// 2. `$XDG_RUNTIME_DIR/.ydotool_socket` — where ydotoold's `systemd
///    --user` unit and recent versions put it (e.g.
///    `/run/user/1000/.ydotool_socket`).
/// 3. [`DEFAULT_YDOTOOLD_SOCKET`] (`/tmp/.ydotool_socket`) — legacy
///    fallback for older ydotoold.
///
/// Existence is checked separately ([`BackendAvailability::ydotoold_reachable`]);
/// this only decides *where* to look.
pub fn ydotoold_socket_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("YDOTOOL_SOCKET") {
        return PathBuf::from(explicit);
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join(".ydotool_socket");
    }
    PathBuf::from(DEFAULT_YDOTOOLD_SOCKET)
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
