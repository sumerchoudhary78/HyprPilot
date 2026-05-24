//! HyprPilot daemon — RPC server + undo stack.
//!
//! Other processes (the CLI today, the MCP server later) talk to the daemon
//! over a newline-delimited JSON-RPC protocol on a Unix socket. The daemon
//! records reversible operations so that `undo` can restore prior state.
//!
//! This crate exposes both the binary (`hyprpilot-daemon`) and a lib surface
//! used by clients: [`protocol::Request`], [`protocol::Response`],
//! [`client::DaemonClient`], and [`socket::default_socket_path`].

pub mod client;
pub mod protocol;
pub mod rules;
pub mod server;
pub mod socket;
pub mod undo;

pub use client::DaemonClient;
pub use protocol::{Request, RequestEnvelope, Response, ResponseEnvelope, RpcError};
pub use socket::default_socket_path;
