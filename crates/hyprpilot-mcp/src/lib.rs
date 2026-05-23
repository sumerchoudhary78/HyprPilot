//! HyprPilot MCP server.
//!
//! Exposes Hyprland control as Model Context Protocol tools. Designed to be
//! spawned by an MCP host (Claude Desktop, Claude Code, etc.) over stdio.
//!
//! ## Architecture
//!
//! The MCP server is a thin RPC client to [`hyprpilot-daemon`]. It does not
//! talk to Hyprland directly. Reasons:
//!
//! - The undo stack lives in the daemon and must outlive the MCP host's
//!   spawn lifecycle.
//! - Capability and dry-run checks happen in the MCP layer so a misconfigured
//!   profile can never bypass the daemon's authoritative state.
//!
//! ## Safety model
//!
//! Two layers of restraint apply to every mutating tool:
//!
//! 1. **Capability profiles**. A profile (TOML file or built-in `default`)
//!    declares which tool *groups* are allowed. Tools not allowed by the
//!    active profile are absent from `tools/list` and rejected on
//!    `tools/call`. See [`capability`].
//!
//! 2. **Dry-run default**. Every mutating tool exposes a `dry_run` boolean,
//!    default `true`. With `dry_run=true` the server returns a preview of
//!    what would happen and does not forward to the daemon. Agents must
//!    explicitly pass `dry_run=false` to mutate. See [`tools`].

pub mod capability;
pub mod protocol;
pub mod server;
pub mod tools;

pub use capability::{Profile, ToolGroup};
pub use protocol::{
    Content, ErrorCode, JsonRpcError, JsonRpcRequest, JsonRpcResponse, ServerInfo, Tool,
};
pub use server::Server;
