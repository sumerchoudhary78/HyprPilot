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

use hyprpilot_a11y::Element;
use hyprpilot_core::snapshot::SnapshotRestorePreview;
use hyprpilot_core::types::{Client, Monitor};
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
use crate::tools::{
    self, A11yClickOp, CaptureOp, ClickTextOp, Dispatch, DispatchError, FindTextOp, OcrOp, ToolDef,
};

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
            Dispatch::Ocr(op) => self.handle_ocr(id, op).await,
            Dispatch::FindText(op) => self.handle_find_text(id, op).await,
            Dispatch::ClickText(op) => self.handle_click_text(id, op).await,
            Dispatch::A11yClick(op) => self.handle_a11y_click(id, op).await,
        }
    }

    async fn handle_click_text(&mut self, id: Value, op: ClickTextOp) -> JsonRpcResponse {
        // Effective scan region: the focused window (from the cache) when
        // `active_window` is set, otherwise the explicit region (or full
        // screen). dispatch() already rejected setting both.
        let region = if op.active_window {
            match self.resolve_active_window_region(&id).await {
                Ok(r) => Some(r),
                Err(resp) => return resp,
            }
        } else {
            op.region
        };

        // Stage 1: vision (daemon-bypass — capture + OCR run locally).
        let matches = match scan_for_matches(
            region,
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
                Ok(ms) => pick_scale(&ms, region.as_ref()),
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
        let (cx, cy) = match region {
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

    /// `a11y_click`: find an element by label/role via the daemon, turn its
    /// window-relative box into a screen point using the window's geometry,
    /// then (unless dry-run) move + click.
    ///
    /// Coordinate model: AT-SPI window-relative extents and Hyprland's window
    /// `at` are both treated as logical compositor pixels, so the click point
    /// is simply `window.at + element.center`. The dry-run preview surfaces
    /// every input (window origin, element box, result) so a wrong assumption
    /// is visible before anything moves.
    async fn handle_a11y_click(&mut self, id: Value, op: A11yClickOp) -> JsonRpcResponse {
        // Stage 1: find candidates via the daemon's a11y walk.
        let find = Request::A11yFind {
            pid: op.pid,
            query: op.query.clone(),
            role: op.role.clone(),
            max_nodes: op.max_nodes,
        };
        let value = match self.call_daemon(find).await {
            Ok(v) => v,
            Err(e) => {
                self.daemon = None;
                let (code, message) = classify_daemon_error(&e);
                return tool_error(id, code, message);
            }
        };
        let resolved_pid = value.get("pid").and_then(Value::as_i64).map(|n| n as i32);
        let elements: Vec<Element> = match value.get("elements").cloned() {
            Some(v) => match serde_json::from_value(v) {
                Ok(els) => els,
                Err(e) => {
                    return tool_error(
                        id,
                        ErrorCode::ToolExecution,
                        format!("daemon returned malformed a11y elements: {e}"),
                    );
                }
            },
            None => Vec::new(),
        };

        // Only positive-area elements are clickable targets.
        let matches: Vec<Element> = elements.into_iter().filter(Element::is_clickable).collect();
        if matches.is_empty() {
            return tool_error(
                id,
                ErrorCode::ToolExecution,
                format!("no clickable accessibility element matched `{}`", op.query),
            );
        }
        let total = matches.len();
        if op.require_unique && total > 1 && op.match_index == 0 {
            return tool_error(id, ErrorCode::ToolExecution, render_a11y_ambiguity(&op.query, &matches));
        }
        if op.match_index >= total {
            return tool_error(
                id,
                ErrorCode::InvalidParams,
                format!(
                    "match_index {} out of bounds: only {} clickable match(es) for `{}`",
                    op.match_index, total, op.query
                ),
            );
        }
        let chosen = matches[op.match_index].clone();

        // Stage 2: anchor the window-relative box to the window's screen origin.
        let Some(pid) = resolved_pid else {
            return tool_error(
                id,
                ErrorCode::ToolExecution,
                "daemon did not report a resolved pid to anchor the click".to_string(),
            );
        };
        let clients = match self.call_daemon(Request::QueryClients).await {
            Ok(v) => match serde_json::from_value::<Vec<Client>>(v) {
                Ok(c) => c,
                Err(e) => {
                    return tool_error(
                        id,
                        ErrorCode::ToolExecution,
                        format!("daemon returned malformed clients: {e}"),
                    );
                }
            },
            Err(e) => {
                self.daemon = None;
                let (code, message) = classify_daemon_error(&e);
                return tool_error(id, code, message);
            }
        };
        let Some(window) = pick_window_for_pid(&clients, pid) else {
            return tool_error(
                id,
                ErrorCode::ToolExecution,
                format!("no window found for pid {pid} to anchor the click"),
            );
        };
        let (ecx, ecy) = chosen.center();
        let cx = window.at[0] + ecx;
        let cy = window.at[1] + ecy;

        if op.dry_run {
            let body = render_a11y_click_preview(&op, &chosen, total, window, cx, cy);
            return JsonRpcResponse::ok(
                id,
                CallToolResult { content: vec![Content::text(body)], is_error: false },
            );
        }

        // Stage 3: drive input via the daemon (absolute move, then click).
        if let Err(resp) = self
            .run_input(id.clone(), Request::InputMouseMove { x: cx, y: cy, absolute: true })
            .await
        {
            return resp;
        }
        if let Err(resp) =
            self.run_input(id.clone(), Request::InputMouseClick { button: op.button }).await
        {
            return resp;
        }
        let body = render_a11y_click_applied(&chosen, total, cx, cy);
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
    // PPM (raw) not PNG: this image is piped straight into tesseract, never
    // shown, so the lossless PNG encode is pure overhead.
    let bytes = match region {
        Some(r) => cap.region(r, VisionImageFormat::Ppm).await,
        None => cap.full(None, VisionImageFormat::Ppm).await,
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

impl Server {
    async fn handle_find_text(&mut self, id: Value, op: FindTextOp) -> JsonRpcResponse {
        let region = if op.active_window {
            match self.resolve_active_window_region(&id).await {
                Ok(r) => Some(r),
                Err(resp) => return resp,
            }
        } else {
            op.region
        };
        match scan_for_matches(
            region,
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
                            serde_json::to_string_pretty(&body)
                                .unwrap_or_else(|_| body.to_string()),
                        )],
                        is_error: false,
                    },
                )
            }
            Err(e) => tool_error(id, ErrorCode::ToolExecution, e),
        }
    }

    /// Resolve the focused window's rectangle (logical compositor coords) from
    /// the daemon's world-model cache, for `active_window`-scoped vision.
    async fn resolve_active_window_region(&mut self, id: &Value) -> Result<Region, JsonRpcResponse> {
        let value = match self.call_daemon(Request::QueryActiveWindow).await {
            Ok(v) => v,
            Err(e) => {
                self.daemon = None;
                let (code, message) = classify_daemon_error(&e);
                return Err(tool_error(id.clone(), code, message));
            }
        };
        let client: Option<Client> = serde_json::from_value(value).map_err(|e| {
            tool_error(
                id.clone(),
                ErrorCode::ToolExecution,
                format!("daemon returned malformed active window: {e}"),
            )
        })?;
        let Some(c) = client else {
            return Err(tool_error(
                id.clone(),
                ErrorCode::ToolExecution,
                "no active window to scope to; focus a window or pass an explicit region"
                    .to_string(),
            ));
        };
        let (w, h) = (c.size[0], c.size[1]);
        if w <= 0 || h <= 0 {
            return Err(tool_error(
                id.clone(),
                ErrorCode::ToolExecution,
                format!("active window `{}` has no positive size ({w}x{h})", c.class),
            ));
        }
        Ok(Region { x: c.at[0], y: c.at[1], w: w as u32, h: h as u32 })
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

/// Pick the window to anchor an a11y click to: prefer the focused window
/// (`focusHistoryID == 0`) among the pid's windows, else the first match.
fn pick_window_for_pid(clients: &[Client], pid: i32) -> Option<&Client> {
    let mut fallback = None;
    for c in clients {
        if c.pid == pid {
            if c.focus_history_id == 0 {
                return Some(c);
            }
            fallback.get_or_insert(c);
        }
    }
    fallback
}

fn render_a11y_ambiguity(query: &str, matches: &[Element]) -> String {
    let mut out = format!(
        "ambiguous: `{}` matched {} clickable accessibility elements. Pass \
         `match_index` (0..{}) to pick one, or `require_unique: false` to take \
         the first:\n",
        query,
        matches.len(),
        matches.len().saturating_sub(1),
    );
    for (i, m) in matches.iter().enumerate().take(8) {
        out.push_str(&format!(
            "  [{i}] {role} `{name}` (window-rel {x},{y} {w}x{h})\n",
            role = m.role,
            name = m.name,
            x = m.x,
            y = m.y,
            w = m.w,
            h = m.h,
        ));
    }
    if matches.len() > 8 {
        out.push_str(&format!("  ... and {} more\n", matches.len() - 8));
    }
    out
}

fn render_a11y_click_preview(
    op: &A11yClickOp,
    chosen: &Element,
    total: usize,
    window: &Client,
    cx: i32,
    cy: i32,
) -> String {
    let (ecx, ecy) = chosen.center();
    format!(
        "[dry_run] a11y_click would {button:?}-click {role} `{name}` at compositor ({cx}, {cy}).\n\
         window `{class}` origin ({wx}, {wy}) + element window-relative centre ({ecx}, {ecy}); \
         box {x},{y} {w}x{h}; match {idx}/{total}.\n\n\
         Pass `dry_run: false` to apply.",
        button = op.button,
        role = chosen.role,
        name = chosen.name,
        class = window.class,
        wx = window.at[0],
        wy = window.at[1],
        x = chosen.x,
        y = chosen.y,
        w = chosen.w,
        h = chosen.h,
        idx = op.match_index,
        total = total,
    )
}

fn render_a11y_click_applied(chosen: &Element, total: usize, cx: i32, cy: i32) -> String {
    let body = serde_json::json!({
        "clicked": {
            "role": chosen.role,
            "name": chosen.name,
            "window_rel": { "x": chosen.x, "y": chosen.y, "w": chosen.w, "h": chosen.h },
        },
        "click_at": { "x": cx, "y": cy },
        "total_matches": total,
    });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
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
///
/// Captures use PPM (raw): the bytes are piped to tesseract, never shown, so
/// the PNG encode would be wasted work.
impl Server {
    async fn handle_ocr(&mut self, id: Value, op: OcrOp) -> JsonRpcResponse {
        let (region, lang, psm) = match op {
            OcrOp::Screen { lang, psm } => (None, lang, psm),
            OcrOp::Region { region, lang, psm } => (Some(region), lang, psm),
            OcrOp::ActiveWindow { lang, psm } => match self.resolve_active_window_region(&id).await {
                Ok(r) => (Some(r), lang, psm),
                Err(resp) => return resp,
            },
        };
        let cap = match GrimCapture::detect() {
            Ok(c) => c,
            Err(e) => return tool_error(id, ErrorCode::ToolExecution, e.to_string()),
        };
        let bytes_res = match region {
            Some(r) => cap.region(r, VisionImageFormat::Ppm).await,
            None => cap.full(None, VisionImageFormat::Ppm).await,
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
