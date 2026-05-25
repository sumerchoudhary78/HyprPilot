//! libei backend — v0.7 scaffold, no real implementation yet.
//!
//! Compiled only under `--features libei`. See `docs/libei-design.md`
//! for the rationale (wtype/virtual-keyboard global-bind filter) and
//! the v0.8 milestone plan. Every method on this backend currently
//! returns [`InputError::NotImplemented`]; the dispatch wiring lives
//! in [`crate::runner`].
//!
//! The constructor [`LibeiBackend::detect`] is the connection point for
//! v0.8 work: it should perform the `org.freedesktop.portal.RemoteDesktop`
//! handshake here and stash the EIS connection on the backend.

use crate::error::Result;

/// Holder for a future libei/EIS connection. Empty in the v0.7 scaffold
/// — real fields land with v0.8 M1.
#[allow(dead_code)] // populated in v0.8
pub struct LibeiBackend {
    // Placeholder. v0.8 will store the portal-session handle and the
    // EIS context (e.g. `reis::tokio::EiEventStream` plus an
    // `ei::Connection` producer). Keeping the struct non-empty signals
    // intent without committing to a concrete shape today.
    _portal_session_placeholder: (),
}

impl LibeiBackend {
    /// Construct the stub. v0.8 M1 will turn this into an
    /// `org.freedesktop.portal.RemoteDesktop` handshake that opens an
    /// EIS file descriptor and returns the live session. Today it just
    /// returns the empty struct so `HYPRPILOT_INPUT_BACKEND=libei`
    /// selects the backend cleanly; the method calls themselves error
    /// out with [`InputError::NotImplemented`].
    pub fn detect() -> Result<Self> {
        Ok(Self {
            _portal_session_placeholder: (),
        })
    }
}
