use thiserror::Error;

#[derive(Debug, Error)]
pub enum VisionError {
    /// Required binary not on PATH. Distinguish from a found-but-broken
    /// backend; the user fix is to install the package.
    #[error(
        "backend `{0}` is not installed (or not on $PATH); install it and try again"
    )]
    BackendMissing(&'static str),

    /// Backend was found but exited non-zero.
    #[error("backend `{backend}` exited with status {status}: {stderr}")]
    BackendFailed {
        backend: &'static str,
        status: String,
        stderr: String,
    },

    /// Caller supplied a degenerate region.
    #[error("invalid region: width and height must be > 0, got {w}x{h}")]
    InvalidRegion { w: i32, h: i32 },

    /// Backend exited 0 but wrote no bytes. Usually means it captured an
    /// empty/invisible monitor — actionable as a config issue.
    #[error("backend returned no output bytes")]
    EmptyOutput,

    #[error("I/O error invoking vision backend: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, VisionError>;
