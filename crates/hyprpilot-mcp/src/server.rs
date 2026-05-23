//! Top-level MCP server: state, request routing, stdio loop.
//!
//! One [`Server`] instance per MCP session. It owns:
//!
//! - the active [`Profile`] (gates `tools/list` and `tools/call`);
//! - a lazy daemon connection (reconnects on transport failure);
//! - the cached tool registry.
//!
//! Concurrency: MCP stdio is sequential by spec, so there is no concurrent
//! tool calls. The daemon client is held behind an `Option` and rebuilt on
//! error.

use std::path::PathBuf;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, warn};

use hyprpilot_core::snapshot::{RestoreAction, SnapshotDiff};
use hyprpilot_daemon::client::{ClientError, DaemonClient};
use hyprpilot_daemon::protocol::Request;

use crate::capability::Profile;
use crate::protocol::{
    CallToolParams, CallToolResult, Content, ErrorCode, InitializeParams, InitializeResult,
    JsonRpcRequest, JsonRpcResponse, ListToolsResult, ServerCapabilities, ServerInfo, Tool,
    PROTOCOL_VERSION,
};
use crate::tools::{self, Dispatch, DispatchError, ToolDef};

pub struct Server {
    profile: Profile,
    daemon_socket: PathBuf,
    daemon: Option<DaemonClient>,
    registry: Vec<ToolDef>,
    server_version: String,
}

impl Server {
    pub fn new(profile: Profile, daemon_socket: PathBuf) -> Self {
        Self {
            profile,
            daemon_socket,
            daemon: None,
            registry: tools::registry(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Visible-to-LLM tool list, filtered by the active profile.
    fn visible_tools(&self) -> Vec<Tool> {
        self.registry
            .iter()
            .filter(|t| self.profile.allows(t.name, t.group))
            .map(|t| Tool {
                name: t.name.to_string(),
                description: t.description.to_string(),
                input_schema: t.schema.clone(),
            })
            .collect()
    }

    fn find_tool(&self, name: &str) -> Option<&ToolDef> {
        self.registry.iter().find(|t| t.name == name)
    }

    /// Get a daemon client, reconnecting if the previous one is dead.
    async fn daemon(&mut self) -> Result<&mut DaemonClient, ClientError> {
        if self.daemon.is_none() {
            debug!(socket = %self.daemon_socket.display(), "connecting to daemon");
            self.daemon = Some(DaemonClient::connect(&self.daemon_socket).await?);
        }
        Ok(self.daemon.as_mut().expect("just set"))
    }

    /// Handle a single JSON-RPC message. Returns `None` for valid
    /// notifications (no response expected) or `Some(response)` otherwise.
    pub async fn handle(&mut self, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
        if req.jsonrpc != "2.0" {
            // For requests with an id, respond with InvalidRequest; for
            // notifications, drop.
            return req.id.map(|id| {
                JsonRpcResponse::err(id, ErrorCode::InvalidRequest, "jsonrpc must be \"2.0\"")
            });
        }

        match req.method.as_str() {
            "initialize" => Some(self.handle_initialize(req)),
            "notifications/initialized" => None,
            "ping" => req.id.map(|id| JsonRpcResponse::ok(id, Value::Object(Default::default()))),
            "tools/list" => Some(self.handle_list_tools(req)),
            "tools/call" => Some(self.handle_call_tool(req).await),
            method => req.id.map(|id| {
                JsonRpcResponse::err(
                    id,
                    ErrorCode::MethodNotFound,
                    format!("method `{method}` is not supported"),
                )
            }),
        }
    }

    fn handle_initialize(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        let _params: InitializeParams = req
            .params
            .clone()
            .and_then(|p| serde_json::from_value(p).ok())
            .unwrap_or_else(|| InitializeParams {
                protocol_version: String::new(),
                capabilities: Value::Null,
                client_info: Value::Null,
            });

        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities { tools: Value::Object(Default::default()) },
            server_info: ServerInfo {
                name: "hyprpilot-mcp".to_string(),
                version: self.server_version.clone(),
            },
            instructions: Some(
                "HyprPilot exposes Hyprland window/workspace control as MCP tools. \
                 Mutating tools default to dry_run=true; pass dry_run=false to \
                 actually apply. Selectors use Hyprland's grammar: `active`, \
                 `class:Name`, `pid:N`, `title:Foo`, `address:0x<hex>`. \
                 Workspace targets: `2`, `+1`, `prev`, `next`, `empty`, \
                 `name:foo`, `special:term`."
                    .to_string(),
            ),
        };
        JsonRpcResponse::ok(id, result)
    }

    fn handle_list_tools(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        JsonRpcResponse::ok(id, ListToolsResult { tools: self.visible_tools() })
    }

    async fn handle_call_tool(&mut self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        let params: CallToolParams = match req
            .params
            .clone()
            .ok_or_else(|| "missing params".to_string())
            .and_then(|p| serde_json::from_value(p).map_err(|e| e.to_string()))
        {
            Ok(p) => p,
            Err(e) => {
                return JsonRpcResponse::err(id, ErrorCode::InvalidParams, e);
            }
        };

        // Capability check.
        let tool = match self.find_tool(&params.name) {
            Some(t) => t.clone(),
            None => {
                return tool_error(
                    id,
                    ErrorCode::MethodNotFound,
                    format!("unknown tool `{}`", params.name),
                );
            }
        };
        if !self.profile.allows(tool.name, tool.group) {
            return tool_error(
                id,
                ErrorCode::CapabilityDenied,
                format!(
                    "tool `{}` is not permitted by the active capability profile `{}` \
                     (group: `{}`)",
                    tool.name,
                    self.profile.name,
                    tool.group.name()
                ),
            );
        }

        // Parse + dispatch.
        let dispatch = match tools::dispatch(&params.name, params.arguments) {
            Ok(d) => d,
            Err(DispatchError::UnknownTool(name)) => {
                return tool_error(
                    id,
                    ErrorCode::MethodNotFound,
                    format!("unknown tool `{name}`"),
                );
            }
            Err(DispatchError::InvalidArgs { tool, message }) => {
                return tool_error(
                    id,
                    ErrorCode::InvalidParams,
                    format!("invalid arguments for `{tool}`: {message}"),
                );
            }
        };

        match dispatch {
            Dispatch::Preview(text) => {
                let body = format!(
                    "[dry_run] {text}\n\nPass `dry_run: false` to apply."
                );
                JsonRpcResponse::ok(
                    id,
                    CallToolResult { content: vec![Content::text(body)], is_error: false },
                )
            }
            Dispatch::Forward(request) => self.forward(id, request).await,
            Dispatch::PreviewSnapshotRestore { name } => {
                self.preview_snapshot_restore(id, name).await
            }
        }
    }

    /// Dry-run preview for `snapshot_restore`: fetch the diff from the daemon
    /// and render the action list inline. On any daemon error (notably
    /// `snapshot_not_found`), propagate via the normal error path so the
    /// agent sees the same JSON-RPC codes it would for a non-dry-run call.
    async fn preview_snapshot_restore(&mut self, id: Value, name: String) -> JsonRpcResponse {
        let value = match self.call_daemon(Request::SnapshotDiff { name: name.clone() }).await {
            Ok(v) => v,
            Err(e) => {
                self.daemon = None;
                let (code, message) = classify_daemon_error(&e);
                return tool_error(id, code, message);
            }
        };
        let diff: SnapshotDiff = match serde_json::from_value(value) {
            Ok(d) => d,
            Err(e) => {
                return tool_error(
                    id,
                    ErrorCode::ToolExecution,
                    format!("daemon returned malformed snapshot diff: {e}"),
                );
            }
        };
        let body = render_restore_preview(&name, &diff);
        JsonRpcResponse::ok(
            id,
            CallToolResult { content: vec![Content::text(body)], is_error: false },
        )
    }

    async fn forward(&mut self, id: Value, request: Request) -> JsonRpcResponse {
        match self.call_daemon(request).await {
            Ok(value) => {
                let body = if value.is_null() {
                    "ok".to_string()
                } else {
                    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
                };
                JsonRpcResponse::ok(
                    id,
                    CallToolResult { content: vec![Content::text(body)], is_error: false },
                )
            }
            Err(e) => {
                // Drop the daemon client so the next call attempts a reconnect.
                self.daemon = None;
                let (code, message) = classify_daemon_error(&e);
                tool_error(id, code, message)
            }
        }
    }

    async fn call_daemon(&mut self, request: Request) -> Result<Value, ClientError> {
        let daemon = self.daemon().await?;
        daemon.call::<Value>(request).await
    }

    /// Run the stdio loop until EOF. Reads newline-delimited JSON-RPC from
    /// `stdin`, writes responses to `stdout`. Anything we log goes to
    /// `stderr` — never stdout.
    pub async fn run_stdio(mut self) -> std::io::Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                debug!("stdin closed; exiting");
                return Ok(());
            }
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                continue;
            }
            let response = match serde_json::from_str::<JsonRpcRequest>(trimmed) {
                Ok(req) => self.handle(req).await,
                Err(e) => {
                    warn!(error = %e, raw = trimmed, "parse error");
                    Some(JsonRpcResponse::err(
                        Value::Null,
                        ErrorCode::ParseError,
                        e.to_string(),
                    ))
                }
            };
            if let Some(resp) = response {
                let mut out = match serde_json::to_string(&resp) {
                    Ok(s) => s,
                    Err(e) => {
                        error!(error = %e, "failed to encode response");
                        continue;
                    }
                };
                out.push('\n');
                stdout.write_all(out.as_bytes()).await?;
                stdout.flush().await?;
            }
        }
    }
}

fn tool_error(id: Value, code: ErrorCode, message: String) -> JsonRpcResponse {
    // MCP convention: tool-execution failures are returned as a *successful*
    // tools/call response with `isError: true`, not a JSON-RPC error. We use
    // a JSON-RPC error only for protocol-level problems (parse, capability,
    // unknown method). Tool-internal failures are reported as content.
    match code {
        ErrorCode::ToolExecution => JsonRpcResponse::ok(
            id,
            CallToolResult { content: vec![Content::text(message)], is_error: true },
        ),
        _ => JsonRpcResponse::err(id, code, message),
    }
}

/// Map a daemon-side [`ClientError`] to an MCP error code + message.
///
/// Validation-class daemon codes (bad agent input that happened to be caught
/// at the daemon boundary, not in the MCP arg parser) are surfaced as
/// JSON-RPC `InvalidParams` so they look the same to the agent as MCP-layer
/// arg validation. Runtime-class codes (Hyprland rejected the dispatch, undo
/// stack empty, snapshot I/O, etc.) stay as `ToolExecution` and surface as
/// `isError` content, since they're not the agent's fault to fix by tweaking
/// its arguments.
///
/// The daemon's structured code is preserved as a `[code]` prefix in the
/// message so the agent and humans can still see exactly what failed.
/// Format a `SnapshotDiff` into a human-readable preview body for
/// `snapshot_restore --dry-run`. Caps the action list at 10 lines; an
/// agent that wants the full list can still call `snapshot_diff` directly.
fn render_restore_preview(name: &str, diff: &SnapshotDiff) -> String {
    let mut moves = 0usize;
    let mut floats = 0usize;
    let mut missing = 0usize;
    let mut extra = 0usize;
    for a in &diff.actions {
        match a {
            RestoreAction::MoveToWorkspace { .. } => moves += 1,
            RestoreAction::ToggleFloating { .. } => floats += 1,
            RestoreAction::WindowMissing { .. } => missing += 1,
            RestoreAction::WindowNotInSnapshot { .. } => extra += 1,
        }
    }

    let mut out = String::new();
    out.push_str(&format!("[dry_run] would restore snapshot `{name}`:\n"));
    out.push_str(&format!(
        "  {moves} window move{}, {floats} float toggle{}, {missing} missing, \
         {extra} not in snapshot\n",
        if moves == 1 { "" } else { "s" },
        if floats == 1 { "" } else { "s" },
    ));

    if diff.mutation_count() == 0 {
        out.push_str("  (no changes — current state already matches the snapshot)\n");
    } else {
        let mut shown = 0usize;
        const LIMIT: usize = 10;
        let mutating: Vec<&RestoreAction> = diff
            .actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    RestoreAction::MoveToWorkspace { .. } | RestoreAction::ToggleFloating { .. }
                )
            })
            .collect();
        for a in mutating.iter().take(LIMIT) {
            match a {
                RestoreAction::MoveToWorkspace { address, to_workspace, .. } => {
                    out.push_str(&format!(
                        "  move address:{address} to workspace {to_workspace}\n"
                    ));
                }
                RestoreAction::ToggleFloating { address, target } => {
                    out.push_str(&format!(
                        "  toggle floating address:{address} → {target}\n"
                    ));
                }
                _ => {}
            }
            shown += 1;
        }
        let total = mutating.len();
        if total > shown {
            out.push_str(&format!("  ... and {} more\n", total - shown));
        }
    }
    out.push_str("\nPass `dry_run: false` to apply.");
    out
}

fn classify_daemon_error(e: &ClientError) -> (ErrorCode, String) {
    match e {
        ClientError::Rpc { code, message } => {
            // Daemon error codes are defined as string constants in
            // `hyprpilot_daemon::protocol::codes` — see that module for the
            // canonical list.
            let mcp_code = match code.as_str() {
                "snapshot_invalid_name" | "snapshot_not_found" => ErrorCode::InvalidParams,
                // undo_empty, undo_failed, hyprland_rejected,
                // hyprland_unknown_dispatcher, hyprland_io, no_instance,
                // snapshot_io, parse_error, internal — all runtime / state
                // problems, not bad input. ToolExecution → isError content.
                _ => ErrorCode::ToolExecution,
            };
            (mcp_code, format!("[{code}] {message}"))
        }
        _ => (ErrorCode::ToolExecution, e.to_string()),
    }
}
