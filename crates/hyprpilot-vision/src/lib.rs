//! Screen capture and OCR for HyprPilot.
//!
//! Shell-outs to two well-known tools:
//!
//! - [`GrimCapture`] (capture.rs) wraps `grim` (wlr-screencopy client).
//! - [`TesseractOcr`] (ocr.rs) wraps the `tesseract` CLI.
//!
//! Both detect their binary at construction time via [`BackendAvailability`]
//! and surface missing dependencies as [`VisionError::BackendMissing`]. The
//! intent mirrors `hyprpilot-input`: prefer mature external tools, fail
//! loudly + actionably when they're not present.
//!
//! This crate is daemon-agnostic. Screen capture is stateless and the
//! output is bulky (PNG bytes); routing it through the daemon's
//! newline-JSON RPC would be wasteful, so the CLI and MCP server call
//! into this crate directly.

pub mod capture;
pub mod detect;
pub mod error;
pub mod ocr;

pub use capture::{BBox, GrimCapture, ImageFormat, Region};
pub use detect::BackendAvailability;
pub use error::{Result, VisionError};
pub use ocr::{find_word_runs, Psm, TesseractOcr, TextMatch, Word};
