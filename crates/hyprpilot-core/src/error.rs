use std::path::PathBuf;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("XDG_RUNTIME_DIR is not set; cannot locate Hyprland sockets")]
    NoRuntimeDir,

    #[error(
        "no live Hyprland instance found (HYPRLAND_INSTANCE_SIGNATURE unset and \
         no instance directory with a .socket.sock under {0})"
    )]
    NoInstance(PathBuf),

    #[error("Hyprland control socket not found at {0}")]
    SocketMissing(PathBuf),

    #[error("I/O failure talking to Hyprland: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse Hyprland JSON response: {0}")]
    Json(#[from] serde_json::Error),

    /// Dispatcher succeeded as a transport call but Hyprland reported failure.
    #[error("Hyprland rejected dispatcher `{verb}`: {message}")]
    Rejected { verb: String, message: String },

    /// Dispatcher name not known to this Hyprland build.
    #[error("Hyprland does not recognise dispatcher `{0}`")]
    UnknownDispatcher(String),

    /// Response was syntactically valid but semantically not what we asked for.
    #[error("unexpected Hyprland response: {0}")]
    Protocol(String),

    /// Client-side input was rejected before reaching Hyprland.
    #[error("validation error: {0}")]
    Validation(String),

    /// Lookup (e.g. on-disk snapshot) did not find the requested item.
    #[error("not found: {0}")]
    NotFound(String),
}
