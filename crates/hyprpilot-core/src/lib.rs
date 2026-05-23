//! Async Rust client for Hyprland's IPC.
//!
//! The compositor is the source of truth. This crate is a thin, typed surface
//! over Hyprland's two Unix sockets — the control socket and the event socket.
//! It does not cache, does not run a daemon, and does not interpret state on
//! the caller's behalf beyond parsing JSON.
//!
//! ## Surface
//!
//! - [`Connection`] — entry point. Discovers the running Hyprland instance and
//!   exposes typed queries ([`Connection::clients`], [`Connection::workspaces`],
//!   etc.) and dispatchers ([`Connection::focus_window`],
//!   [`Connection::move_active_to_workspace`], …).
//! - [`EventStream`] — push events from `.socket2.sock`.
//! - [`types`] — strongly typed representations of Hyprland's JSON outputs.
//! - [`selector`] — typed window/workspace selectors.
//!
//! ## Versioning
//!
//! Dispatcher names and JSON fields drift across Hyprland releases. v0.1 is
//! verified against Hyprland 0.55.x. Older versions may work for read-only
//! queries; mutating dispatchers may have renamed.

pub mod dispatch;
pub mod error;
pub mod events;
pub mod ipc;
pub mod query;
pub mod selector;
pub mod types;

pub use dispatch::Direction;
pub use error::{Error, Result};
pub use events::{EventStream, RawEvent};
pub use ipc::{Connection, Instance};
pub use selector::{WindowSelector, WorkspaceRef};
