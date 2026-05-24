use thiserror::Error;

#[derive(Debug, Error)]
pub enum InputError {
    #[error("input backend `{0}` is not installed (or not on PATH)")]
    BackendMissing(&'static str),

    #[error(
        "input daemon `{0}` is not reachable: socket missing. \
         Start ydotoold (e.g. `systemctl --user start ydotoold` or `sudo ydotoold &`); \
         set $YDOTOOL_SOCKET if your socket lives at a non-default path (default `/tmp/.ydotool_socket`)."
    )]
    DaemonNotReachable(&'static str),

    #[error("invalid key combo: {0}")]
    InvalidCombo(String),

    #[error("invalid mouse button `{0}`; expected left/right/middle/x1/x2")]
    InvalidButton(String),

    #[error("input backend `{backend}` exited with status {status}: {stderr}")]
    BackendFailed {
        backend: &'static str,
        status: String,
        stderr: String,
    },

    #[error("I/O error invoking input backend: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, InputError>;
