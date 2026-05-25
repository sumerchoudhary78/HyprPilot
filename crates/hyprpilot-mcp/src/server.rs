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

use hyprpilot_core::snapshot::SnapshotRestorePreview;
use hyprpilot_core::snapshot::{RestoreAction, SnapshotDiff};
use hyprpilot_core::types::Monitor;
use hyprpilot_daemon::client::{ClientError, DaemonClient};
use hyprpilot_daemon::protocol::Request;
use hyprpilot_vision::{
    find_word_runs, BBox, GrimCapture, ImageFormat as VisionImageFormat, Region, TesseractOcr,
    TextMatch,
};

use crate::capability::Profile;
use crate::protocol::{
    CallToolParams, CallToolResult, Content, ErrorCode, InitializeParams, InitializeResult,
    JsonRpcRequest, JsonRpcResponse, ListToolsResult, ServerCapabilities, ServerInfo, Tool,
    PROTOCOL_VERSION,
};
use crate::tools::{self, CaptureOp, ClickTextOp, Dispatch, DispatchError, FindTextOp, OcrOp, ToolDef};

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
            Dispatch::Capture(op) => handle_capture(id, op).await,
            Dispatch::Ocr(op) => handle_ocr(id, op).await,
            Dispatch::FindText(op) => handle_find_text(id, op).await,
            Dispatch::ClickText(op) => self.handle_click_text(id, op).await,
        }
    }

    async fn handle_click_text(&mut self, id: Value, op: ClickTextOp) -> JsonRpcResponse {
        // Stage 1: vision (daemon-bypass — capture + OCR run locally).
        let matches = match scan_for_matches(
            op.region.as_ref().copied(),
            &op.query,
            op.case_sensitive,
            op.min_confidence,
            op.psm,
            &op.lang,
        )
        .await
        {
            Ok(m) => m,
            Err(e) => return tool_error(id, ErrorCode::ToolExecution, e),
        };

        if matches.is_empty() {
            return tool_error(
                id,
                ErrorCode::ToolExecution,
                format!("no on-screen text matched query `{}`", op.query),
            );
        }
        let total = matches.len();
        if op.require_unique && total > 1 && op.match_index == 0 {
            // Match index defaulted but the result is ambiguous — bail.
            let body = render_ambiguity(&op.query, &matches);
            return tool_error(id, ErrorCode::ToolExecution, body);
        }
        if op.match_index >= total {
            return tool_error(
                id,
                ErrorCode::InvalidParams,
                format!(
                    "match_index {} out of bounds: only {} match(es) for `{}`",
                    op.match_index, total, op.query
                ),
            );
        }
        let chosen = &matches[op.match_index];

        // OCR runs on grim's PHYSICAL-pixel image, but mouse_move takes
        // LOGICAL compositor coords. On a 2x display they differ by the
        // monitor's scale. Query monitors so we can map back.
        let scale = match self.call_daemon(Request::QueryMonitors).await {
            Ok(v) => match serde_json::from_value::<Vec<Monitor>>(v) {
                Ok(ms) => pick_scale(&ms, op.region.as_ref()),
                Err(e) => {
                    return tool_error(
                        id,
                        ErrorCode::ToolExecution,
                        format!("daemon returned malformed monitors: {e}"),
                    );
                }
            },
            Err(e) => {
                self.daemon = None;
                let (code, message) = classify_daemon_error(&e);
                return tool_error(id, code, message);
            }
        };

        // image-px → logical-px, then add region origin (already logical).
        let (cx_img, cy_img) = chosen.bbox.center();
        let cx_logical = ((cx_img as f64) / scale).round() as i32;
        let cy_logical = ((cy_img as f64) / scale).round() as i32;
        let (cx, cy) = match op.region {
            Some(r) => (cx_logical + r.x, cy_logical + r.y),
            None => (cx_logical, cy_logical),
        };

        if op.dry_run {
            let body = render_click_preview(&op, chosen, total, cx, cy);
            return JsonRpcResponse::ok(
                id,
                CallToolResult { content: vec![Content::text(body)], is_error: false },
            );
        }

        // Stage 2: drive input via daemon. mouse_move first (absolute), then
        // mouse_click. Either request can surface BackendMissing or
        // DaemonNotReachable — the daemon's RPC error codes already
        // distinguish them; we propagate via the standard `classify_daemon_error`.
        if let Err(resp) =
            self.run_input(id.clone(), Request::InputMouseMove { x: cx, y: cy, absolute: true }).await
        {
            return resp;
        }
        if let Err(resp) =
            self.run_input(id.clone(), Request::InputMouseClick { button: op.button }).await
        {
            return resp;
        }

        let body = render_click_applied(chosen, total, cx, cy);
        JsonRpcResponse::ok(
            id,
            CallToolResult { content: vec![Content::text(body)], is_error: false },
        )
    }

    /// Forward an input request to the daemon; on failure synthesise the
    /// outgoing JSON-RPC response and return it as `Err` so the caller
    /// short-circuits the click chain.
    async fn run_input(&mut self, id: Value, request: Request) -> Result<(), JsonRpcResponse> {
        match self.call_daemon(request).await {
            Ok(_) => Ok(()),
            Err(e) => {
                self.daemon = None;
                let (code, message) = classify_daemon_error(&e);
                Err(tool_error(id, code, message))
            }
        }
    }

    /// Dry-run preview for `snapshot_restore`: fetch the rich preview from
    /// the daemon and emit it as a single JSON content block. The agent
    /// gets the human-readable summary, the per-category counts, and the
    /// full action list in one tool call — no separate `snapshot_diff`
    /// round-trip needed. On any daemon error (notably `snapshot_not_found`),
    /// propagate via the normal error path so the agent sees the same
    /// JSON-RPC codes it would for a non-dry-run call.
    async fn preview_snapshot_restore(&mut self, id: Value, name: String) -> JsonRpcResponse {
        let value = match self.call_daemon(Request::SnapshotPreview { name }).await {
            Ok(v) => v,
            Err(e) => {
                self.daemon = None;
                let (code, message) = classify_daemon_error(&e);
                return tool_error(id, code, message);
            }
        };
        let preview: SnapshotRestorePreview = match serde_json::from_value(value) {
            Ok(p) => p,
            Err(e) => {
                return tool_error(
                    id,
                    ErrorCode::ToolExecution,
                    format!("daemon returned malformed snapshot preview: {e}"),
                );
            }
        };
        let body = render_dry_run_body(&preview);
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

/// Capture (region or full screen), OCR for words, filter to query matches.
/// Returns matches in *compositor* coordinates (translated from image space
/// when a region was supplied). Errors are pre-formatted strings ready for
/// `ToolExecution`.
async fn scan_for_matches(
    region: Option<Region>,
    query: &str,
    case_sensitive: bool,
    min_confidence: i32,
    psm: hyprpilot_vision::Psm,
    lang: &str,
) -> Result<Vec<TextMatch>, String> {
    let cap = GrimCapture::detect().map_err(|e| e.to_string())?;
    let bytes = match region {
        Some(r) => cap.region(r, VisionImageFormat::Png).await,
        None => cap.full(None, VisionImageFormat::Png).await,
    }
    .map_err(|e| e.to_string())?;

    let ocr = TesseractOcr::detect()
        .map_err(|e| e.to_string())?
        .with_lang(lang)
        .with_psm(psm)
        .with_min_confidence(min_confidence);
    let words = ocr.extract_words(&bytes).await.map_err(|e| e.to_string())?;

    let mut matches = find_word_runs(&words, query, case_sensitive);
    if let Some(r) = region {
        for m in &mut matches {
            m.bbox = BBox { x: m.bbox.x + r.x, y: m.bbox.y + r.y, w: m.bbox.w, h: m.bbox.h };
        }
    }
    Ok(matches)
}

async fn handle_find_text(id: Value, op: FindTextOp) -> JsonRpcResponse {
    match scan_for_matches(
        op.region,
        &op.query,
        op.case_sensitive,
        op.min_confidence,
        op.psm,
        &op.lang,
    )
    .await
    {
        Ok(matches) => {
            let body = serde_json::json!({
                "query": op.query,
                "match_count": matches.len(),
                "matches": matches,
            });
            JsonRpcResponse::ok(
                id,
                CallToolResult {
                    content: vec![Content::text(
                        serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()),
                    )],
                    is_error: false,
                },
            )
        }
        Err(e) => tool_error(id, ErrorCode::ToolExecution, e),
    }
}

fn render_click_preview(
    op: &ClickTextOp,
    chosen: &TextMatch,
    total: usize,
    cx: i32,
    cy: i32,
) -> String {
    format!(
        "[dry_run] click_text would {button:?}-click `{text}` at compositor ({cx}, {cy}) \
         (bbox {x},{y} {w}x{h}, conf {conf}, match {idx}/{total}).\n\n\
         Pass `dry_run: false` to apply.",
        button = op.button,
        text = chosen.text,
        cx = cx,
        cy = cy,
        x = chosen.bbox.x,
        y = chosen.bbox.y,
        w = chosen.bbox.w,
        h = chosen.bbox.h,
        conf = chosen.confidence,
        idx = op.match_index,
        total = total,
    )
}

fn render_click_applied(chosen: &TextMatch, total: usize, cx: i32, cy: i32) -> String {
    let body = serde_json::json!({
        "clicked": chosen,
        "click_at": { "x": cx, "y": cy },
        "total_matches": total,
    });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
}

fn render_ambiguity(query: &str, matches: &[TextMatch]) -> String {
    let mut out = format!(
        "ambiguous: `{}` matched {} regions on screen. Pass `match_index` \
         (0..{}) to pick one, or `require_unique: false` to take the first:\n",
        query,
        matches.len(),
        matches.len().saturating_sub(1),
    );
    for (i, m) in matches.iter().enumerate().take(8) {
        out.push_str(&format!(
            "  [{i}] `{text}` at ({x},{y}) {w}x{h} conf={conf}\n",
            text = m.text,
            x = m.bbox.x,
            y = m.bbox.y,
            w = m.bbox.w,
            h = m.bbox.h,
            conf = m.confidence,
        ));
    }
    if matches.len() > 8 {
        out.push_str(&format!("  ... and {} more\n", matches.len() - 8));
    }
    out
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
/// Format a [`SnapshotRestorePreview`] for the MCP dry-run response.
///
/// The body is the human summary on the first line, followed by the full
/// preview as pretty-printed JSON so an agent can drill into `will_apply`
/// / `will_skip` / per-action details without a second tool call. Closes
/// with a one-line reminder to pass `dry_run: false` to apply.
fn render_dry_run_body(preview: &SnapshotRestorePreview) -> String {
    let json = serde_json::to_string_pretty(preview)
        .unwrap_or_else(|_| "{}".to_string());
    format!(
        "[dry_run] {summary}\n\n{json}\n\nPass `dry_run: false` to apply.",
        summary = preview.human_summary,
    )
/// Format a `SnapshotDiff` into a human-readable preview body for
/// `snapshot_restore --dry-run`. Caps the action list at 10 lines; an
/// agent that wants the full list can still call `snapshot_diff` directly.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Pick the scale factor to convert OCR image-pixel coords back to
/// compositor logical pixels. Preference order: the monitor containing
/// the captured region's origin, then the focused monitor, then the
/// first monitor, finally 1.0. Multi-monitor full-screen captures with
/// mixed scales are not handled accurately — use a `region` to scope.
fn pick_scale(monitors: &[Monitor], region: Option<&Region>) -> f64 {
    if let Some(r) = region {
        if let Some(m) = monitors.iter().find(|m| {
            let logical_w = (m.width as f64 / m.scale).round() as i32;
            let logical_h = (m.height as f64 / m.scale).round() as i32;
            r.x >= m.x && r.x < m.x + logical_w && r.y >= m.y && r.y < m.y + logical_h
        }) {
            return m.scale;
        }
    }
    monitors
        .iter()
        .find(|m| m.focused)
        .or_else(|| monitors.first())
        .map(|m| m.scale)
        .unwrap_or(1.0)
}

fn render_restore_preview(name: &str, diff: &SnapshotDiff) -> String {
    let mut moves = 0usize;
    let mut floats = 0usize;
    let mut repositions = 0usize;
    let mut fullscreens = 0usize;
    let mut pins = 0usize;
    let mut refocus = 0usize;
    let mut missing = 0usize;
    let mut extra = 0usize;
    for a in &diff.actions {
        match a {
            RestoreAction::MoveToWorkspace { .. } => moves += 1,
            RestoreAction::ToggleFloating { .. } => floats += 1,
            RestoreAction::RepositionFloating { .. } => repositions += 1,
            RestoreAction::SetFullscreen { .. } => fullscreens += 1,
            RestoreAction::SetPin { .. } => pins += 1,
            RestoreAction::RestoreActiveFocus { .. } => refocus += 1,
            RestoreAction::WindowMissing { .. } => missing += 1,
            RestoreAction::WindowNotInSnapshot { .. } => extra += 1,
        }
    }

    let mut out = String::new();
    out.push_str(&format!("[dry_run] would restore snapshot `{name}`:\n"));
    out.push_str(&format!(
        "  {moves} workspace move{}, {floats} float toggle{}, \
         {repositions} reposition{}, {fullscreens} fullscreen change{}, \
         {pins} pin toggle{}, {refocus} refocus, \
         {missing} missing, {extra} not in snapshot\n",
        plural(moves),
        plural(floats),
        plural(repositions),
        plural(fullscreens),
        plural(pins),
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
                    RestoreAction::MoveToWorkspace { .. }
                        | RestoreAction::ToggleFloating { .. }
                        | RestoreAction::RepositionFloating { .. }
                        | RestoreAction::SetFullscreen { .. }
                        | RestoreAction::SetPin { .. }
                        | RestoreAction::RestoreActiveFocus { .. }
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
                RestoreAction::RepositionFloating {
                    address,
                    target_at,
                    target_size,
                    ..
                } => {
                    out.push_str(&format!(
                        "  reposition address:{address} → at=({},{}) size=({},{})\n",
                        target_at[0], target_at[1], target_size[0], target_size[1]
                    ));
                }
                RestoreAction::SetFullscreen { address, to_mode, .. } => {
                    let label = match to_mode {
                        0 => "none".to_string(),
                        1 => "maximize".to_string(),
                        2 => "fullscreen".to_string(),
                        n => format!("mode {n}"),
                    };
                    out.push_str(&format!(
                        "  fullscreen address:{address} → {label}\n"
                    ));
                }
                RestoreAction::SetPin { address, target } => {
                    out.push_str(&format!(
                        "  pin address:{address} → {target}\n"
                    ));
                }
                RestoreAction::RestoreActiveFocus { address, .. } => {
                    out.push_str(&format!(
                        "  refocus address:{address}\n"
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

/// Run a capture op and wrap the bytes in an MCP `image` content block.
/// Vision-backend errors surface as `isError` content, mirroring how
/// `forward` reports daemon failures.
async fn handle_capture(id: Value, op: CaptureOp) -> JsonRpcResponse {
    let cap = match GrimCapture::detect() {
        Ok(c) => c,
        Err(e) => return tool_error(id, ErrorCode::ToolExecution, e.to_string()),
    };
    let (bytes_res, mime) = match op {
        CaptureOp::Full { monitor, cursor, format } => {
            let cap = cap.with_cursor(cursor);
            (cap.full(monitor.as_deref(), format).await, format.mime())
        }
        CaptureOp::Region { region, cursor, format } => {
            let cap = cap.with_cursor(cursor);
            (cap.region(region, format).await, format.mime())
        }
    };
    match bytes_res {
        Ok(bytes) => JsonRpcResponse::ok(
            id,
            CallToolResult {
                content: vec![Content::image(&bytes, mime)],
                is_error: false,
            },
        ),
        Err(e) => tool_error(id, ErrorCode::ToolExecution, e.to_string()),
    }
}

/// Run an OCR op: capture, then extract text. Returns a `text` content
/// block. The OCR backend treats "no recognizable text" as `Ok("")` (not an
/// error), so an empty string is a legitimate success result.
async fn handle_ocr(id: Value, op: OcrOp) -> JsonRpcResponse {
    let cap = match GrimCapture::detect() {
        Ok(c) => c,
        Err(e) => return tool_error(id, ErrorCode::ToolExecution, e.to_string()),
    };
    let (bytes_res, lang, psm) = match op {
        OcrOp::Screen { lang, psm } => (
            cap.full(None, hyprpilot_vision::ImageFormat::Png).await,
            lang,
            psm,
        ),
        OcrOp::Region { region, lang, psm } => (
            cap.region(region, hyprpilot_vision::ImageFormat::Png).await,
            lang,
            psm,
        ),
    };
    let bytes = match bytes_res {
        Ok(b) => b,
        Err(e) => return tool_error(id, ErrorCode::ToolExecution, e.to_string()),
    };
    let ocr = match TesseractOcr::detect() {
        Ok(o) => o.with_lang(lang).with_psm(psm),
        Err(e) => return tool_error(id, ErrorCode::ToolExecution, e.to_string()),
    };
    match ocr.extract_text(&bytes).await {
        Ok(text) => JsonRpcResponse::ok(
            id,
            CallToolResult { content: vec![Content::text(text)], is_error: false },
        ),
        Err(e) => tool_error(id, ErrorCode::ToolExecution, e.to_string()),
    }
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
