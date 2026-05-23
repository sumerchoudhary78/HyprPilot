//! Where the daemon listens.
//!
//! Default path is `$XDG_RUNTIME_DIR/hyprpilot.sock`. Falls back to
//! `/tmp/hyprpilot-<uid>.sock` if `XDG_RUNTIME_DIR` is unset.

use std::path::PathBuf;

pub fn default_socket_path() -> PathBuf {
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(rt).join("hyprpilot.sock");
    }
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/tmp/hyprpilot-{uid}.sock"))
}

// Avoid pulling in the `libc` crate for one syscall. `getuid` is a libc symbol
// that's always linked on Unix.
#[allow(non_snake_case)]
unsafe fn libc_getuid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}
