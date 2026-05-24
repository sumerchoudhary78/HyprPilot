//! Backend detection — probe `$PATH` for `grim`, `tesseract`, `slurp`.
//!
//! `slurp` is not used by the vision crate itself but is recorded so callers
//! (the CLI, the MCP server) can advertise region-picker support when it's
//! available.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendAvailability {
    pub grim: Option<PathBuf>,
    pub tesseract: Option<PathBuf>,
    pub slurp: Option<PathBuf>,
}

impl BackendAvailability {
    /// Probe `$PATH` once. Cheap — a handful of filesystem lookups. Run at
    /// startup and cache.
    pub fn detect() -> Self {
        Self {
            grim: which("grim"),
            tesseract: which("tesseract"),
            slurp: which("slurp"),
        }
    }

    pub fn has_grim(&self) -> bool {
        self.grim.is_some()
    }

    pub fn has_tesseract(&self) -> bool {
        self.tesseract.is_some()
    }

    pub fn has_slurp(&self) -> bool {
        self.slurp.is_some()
    }
}

/// `which`-style lookup. Walks `$PATH`, returns the first executable match.
/// Returns `None` on any error rather than failing loudly — callers surface
/// "not available" as a typed error.
pub(crate) fn which(name: &str) -> Option<PathBuf> {
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
    fn which_finds_sh() {
        // `sh` is on every Unix box we care about; verifies the PATH walk
        // without depending on grim/tesseract being installed.
        assert!(which("sh").is_some());
    }

    #[test]
    fn which_returns_none_for_nonsense() {
        assert!(which("this_binary_definitely_does_not_exist_zzz").is_none());
    }

    #[test]
    fn detect_does_not_panic() {
        let _ = BackendAvailability::detect();
    }
}
