//! Error type for the a11y layer.

/// Failures talking to the AT-SPI accessibility bus.
#[derive(Debug, thiserror::Error)]
pub enum A11yError {
    /// Could not connect to the a11y bus (`org.a11y.Bus`). Usually means the
    /// bus launcher isn't running or accessibility is disabled in the session.
    #[error("accessibility bus unavailable: {0}")]
    Unavailable(String),

    /// A D-Bus call to an accessible object failed.
    #[error("atspi call failed: {0}")]
    Bus(String),

    /// No accessible matched the requested application / window.
    #[error("no accessible application matched (pid {0:?})")]
    NoApplication(Option<i32>),
}

pub type Result<T> = std::result::Result<T, A11yError>;
