//! Tool catalog: parameter shapes, descriptions, schemas, and dispatch.
//!
//! Each tool is defined by:
//!
//! - a parameter struct deriving `Serialize`, `Deserialize`, and `JsonSchema`,
//!   from which we generate the MCP `inputSchema`;
//! - a name, group, and LLM-facing description;
//! - a handler in [`dispatch`] that parses args and either forwards to the
//!   daemon or returns a dry-run preview.
//!
//! Conventions for tool descriptions:
//! - State whether the tool reads or mutates.
//! - For mutating tools, name the side effect and whether it is reversible
//!   via the undo stack.
//! - For destructive tools, spell out the irreversible parts.
//!
//! Stringy selectors (`address:0x...`, `class:Foo`, `pid:1234`, `title:Bar`,
//! `active`) and workspace refs (`2`, `+1`, `name:foo`, `special:term`,
//! `empty`, `prev`, `next`) are accepted as-is. They mirror Hyprland's
//! native grammar and what an LLM tends to produce.

use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use hyprpilot_core::dispatch::{Direction, FullscreenMode};
use hyprpilot_core::selector::{WindowSelector, WorkspaceRef};
use hyprpilot_daemon::protocol::Request;
use hyprpilot_input::keys::{KeyCombo, MouseButton};
use hyprpilot_vision::{ImageFormat as VisionImageFormat, Psm, Region};

/// Default tesseract word-confidence floor for the composite vision tools.
/// 50/100 drops obvious noise (single-pixel blobs scored near zero) but
/// keeps anti-aliased text and font hinting glitches that score 50-70.
/// Callers can override via the `min_confidence` arg.
fn default_min_confidence() -> i32 {
    50
}

/// PSM default for composite tools that *scan* a screenshot for a target
/// string. `SparseText` (11) handles UI layouts where labels are scattered
/// across whitespace; the per-block default (6) over-segments and loses
/// labels that aren't part of a single contiguous block.
fn default_scan_psm() -> u8 {
    11
}

use crate::capability::ToolGroup;

// =============================================================================
// Tool metadata
// =============================================================================

/// One entry in the registry. Stable across an MCP server's lifetime.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub group: ToolGroup,
    pub description: &'static str,
    pub mutating: bool,
    pub schema: Value,
}

/// Outcome of parsing + handling a tool call.
#[derive(Debug)]
pub enum Dispatch {
    /// Forward this request to the daemon and surface its response.
    Forward(Request),
    /// Return a dry-run preview without touching the daemon.
    Preview(String),
    /// Fetch a diff for the named snapshot from the daemon and render it
    /// into a structured dry-run preview for `snapshot_restore`. Lets the
    /// agent see exactly what would change without applying anything.
    PreviewSnapshotRestore { name: String },
    /// Capture an image directly via `hyprpilot-vision`. Daemon-agnostic:
    /// capture is stateless and the bytes are bulky, so it would be wasteful
    /// to round-trip them through the daemon's newline-JSON RPC.
    Capture(CaptureOp),
    /// OCR a captured image. Same daemon-bypass reasoning as `Capture`.
    Ocr(OcrOp),
    /// Composite: capture + OCR + filter to `query` matches. Daemon-bypass
    /// for the same reason `Ocr` is.
    FindText(FindTextOp),
    /// Composite: capture + OCR + filter + optionally drive the mouse
    /// (via the daemon's input RPCs). Both vision and input touch points
    /// live here so the server can handle them in one place.
    ClickText(ClickTextOp),
    /// Composite: find an accessibility element by label/role (daemon
    /// A11yFind), turn its window-relative box into a screen point using the
    /// window's geometry, and optionally click it. The a11y analogue of
    /// `ClickText`, without OCR.
    A11yClick(A11yClickOp),
}

/// What to capture and how. Image format is resolved here so we don't pass
/// stringy formats further than necessary.
#[derive(Debug)]
pub enum CaptureOp {
    /// Capture the whole compositor, or a specific monitor if `monitor` is set.
    Full {
        monitor: Option<String>,
        cursor: bool,
        format: VisionImageFormat,
    },
    /// Capture a rectangular region.
    Region {
        region: Region,
        cursor: bool,
        format: VisionImageFormat,
    },
}

/// What to OCR.
#[derive(Debug)]
pub enum OcrOp {
    /// OCR the whole compositor.
    Screen { lang: String, psm: Psm },
    /// OCR a specific region.
    Region { region: Region, lang: String, psm: Psm },
}

/// What to scan for. `region=None` means full screen.
#[derive(Debug)]
pub struct FindTextOp {
    pub query: String,
    pub region: Option<Region>,
    pub case_sensitive: bool,
    pub min_confidence: i32,
    pub psm: Psm,
    pub lang: String,
}

/// What to click. Same fields as [`FindTextOp`] plus the click-side
/// controls. `button=None` defaults to left at the server layer.
#[derive(Debug)]
pub struct ClickTextOp {
    pub query: String,
    pub region: Option<Region>,
    pub case_sensitive: bool,
    pub min_confidence: i32,
    pub psm: Psm,
    pub lang: String,
    pub button: hyprpilot_input::keys::MouseButton,
    pub match_index: usize,
    pub require_unique: bool,
    pub dry_run: bool,
}

/// What to click via accessibility. `pid=None` targets the focused window's
/// app (resolved daemon-side from the world-model cache).
#[derive(Debug)]
pub struct A11yClickOp {
    pub query: String,
    pub role: Option<String>,
    pub pid: Option<i32>,
    pub button: hyprpilot_input::keys::MouseButton,
    pub match_index: usize,
    pub require_unique: bool,
    pub max_nodes: Option<usize>,
    pub dry_run: bool,
}

/// Parameter-parsing or validation failure.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
    #[error("invalid arguments for `{tool}`: {message}")]
    InvalidArgs { tool: String, message: String },
}

// =============================================================================
// Shared param fields
// =============================================================================

/// Common `dry_run` field, defaults to `true` for safety. Mutating tools
/// embed this; query tools do not.
fn default_true() -> bool {
    true
}

// =============================================================================
// Param structs — one per tool
// =============================================================================

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MutNoArgs {
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}
impl Default for MutNoArgs {
    fn default() -> Self {
        Self { dry_run: true }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SelectorArgs {
    /// Window selector. One of: `active`, `address:0x<hex>`, `pid:<int>`,
    /// `class:<regex>`, `title:<regex>`, `tag:<name>`.
    pub selector: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceArgs {
    /// Workspace target. Accepts: `<id>` (e.g. `2`), `+N` / `-N` (relative),
    /// `prev`, `next`, `empty`, `name:<n>`, `special[:<n>]`.
    pub target: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DirArg {
    L,
    R,
    U,
    D,
}
impl DirArg {
    fn to_core(&self) -> Direction {
        match self {
            DirArg::L => Direction::Left,
            DirArg::R => Direction::Right,
            DirArg::U => Direction::Up,
            DirArg::D => Direction::Down,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DirectionArgs {
    /// Direction: `l` (left), `r` (right), `u` (up), `d` (down).
    pub direction: DirArg,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ResizeArgs {
    /// Horizontal delta in pixels.
    pub dx: i32,
    /// Vertical delta in pixels.
    pub dy: i32,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsModeArg {
    Maximize,
    Fullscreen,
}
impl FsModeArg {
    fn to_core(&self) -> FullscreenMode {
        match self {
            FsModeArg::Maximize => FullscreenMode::Maximize,
            FsModeArg::Fullscreen => FullscreenMode::Fullscreen,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FullscreenArgs {
    /// `maximize` respects gaps/bars; `fullscreen` is true exclusive.
    pub mode: FsModeArg,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FocusMonitorArgs {
    /// Monitor name (e.g. `eDP-1`) or direction (`l`, `r`, `u`, `d`).
    pub name_or_direction: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MonitorArgs {
    /// Monitor name (e.g. `eDP-1`, `HDMI-A-1`).
    pub monitor: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ExecArgs {
    /// Shell command to spawn. Forked-and-detached by Hyprland.
    pub command: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotNameArgs {
    /// Snapshot name. Allowed chars: `[A-Za-z0-9_.-]+`, must not start with
    /// `.`, max 128 chars.
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputTextArgs {
    /// Free-form text to type into the focused window. Whatever is
    /// currently focused will receive this — verify with
    /// `query_active_window` first if you're not sure.
    pub text: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputKeysArgs {
    /// Key chord, e.g. `ctrl+shift+t`, `super+space`, `Escape`.
    /// Modifiers: ctrl, shift, alt, super (aliases: control, meta, cmd,
    /// win, mod4). Last token is the keysym (passed to wtype as-is).
    pub combo: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputShortcutArgs {
    /// Key chord, e.g. `ctrl+t`.
    pub combo: String,
    /// Window selector: `active`, `class:Foo`, `pid:1234`, `title:Foo`,
    /// `address:0x<hex>`, or `tag:<name>`.
    pub selector: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputMouseMoveArgs {
    /// X coordinate (pixels). Absolute screen-space if `absolute=true`,
    /// otherwise a delta from the current cursor position.
    pub x: i32,
    /// Y coordinate (pixels). Same semantics as x.
    pub y: i32,
    /// When true, (x, y) are absolute screen coordinates.
    /// When false (default), they are deltas from the current cursor.
    #[serde(default)]
    pub absolute: bool,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InputMouseClickArgs {
    /// `left`, `right`, `middle`, `x1`/`back`, `x2`/`forward`.
    pub button: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

/// Image format exposed to LLMs. `ppm` is intentionally not exposed —
/// niche, large, and offers no advantage over PNG for screenshots.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormatArg {
    #[default]
    Png,
    Jpeg,
}

impl ImageFormatArg {
    fn to_vision(self) -> VisionImageFormat {
        match self {
            ImageFormatArg::Png => VisionImageFormat::Png,
            ImageFormatArg::Jpeg => VisionImageFormat::Jpeg,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ScreenshotArgs {
    /// Optional monitor name (e.g. `eDP-1`, `HDMI-A-1`). When unset,
    /// captures the entire compositor across all monitors.
    #[serde(default)]
    pub monitor: Option<String>,
    /// Include the mouse cursor in the capture. Defaults to `false`.
    #[serde(default)]
    pub cursor: bool,
    /// Image format: `png` (default, lossless) or `jpeg` (smaller, lossy).
    #[serde(default)]
    pub format: ImageFormatArg,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ScreenshotRegionArgs {
    /// Top-left X in compositor pixel coordinates (may be negative).
    pub x: i32,
    /// Top-left Y in compositor pixel coordinates (may be negative).
    pub y: i32,
    /// Region width in pixels. Must be > 0.
    pub w: u32,
    /// Region height in pixels. Must be > 0.
    pub h: u32,
    /// Include the mouse cursor in the capture. Defaults to `false`.
    #[serde(default)]
    pub cursor: bool,
    /// Image format: `png` (default, lossless) or `jpeg` (smaller, lossy).
    #[serde(default)]
    pub format: ImageFormatArg,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ScreenshotMonitorArgs {
    /// Monitor name (e.g. `eDP-1`, `HDMI-A-1`). Matches `hyprctl monitors`.
    pub name: String,
    /// Include the mouse cursor in the capture. Defaults to `false`.
    #[serde(default)]
    pub cursor: bool,
    /// Image format: `png` (default, lossless) or `jpeg` (smaller, lossy).
    #[serde(default)]
    pub format: ImageFormatArg,
}

fn default_lang() -> String {
    "eng".to_string()
}

fn default_psm() -> u8 {
    6
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct OcrScreenArgs {
    /// Tesseract language code (e.g. `eng`, `fra`, `eng+fra`). Default `eng`.
    #[serde(default = "default_lang")]
    pub lang: String,
    /// Page-segmentation mode. One of: 3 (auto), 6 (single uniform block,
    /// default), 7 (single line), 11 (sparse text).
    #[serde(default = "default_psm")]
    pub psm: u8,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct OcrRegionArgs {
    /// Top-left X in compositor pixel coordinates (may be negative).
    pub x: i32,
    /// Top-left Y in compositor pixel coordinates (may be negative).
    pub y: i32,
    /// Region width in pixels. Must be > 0.
    pub w: u32,
    /// Region height in pixels. Must be > 0.
    pub h: u32,
    /// Tesseract language code (e.g. `eng`, `fra`, `eng+fra`). Default `eng`.
    #[serde(default = "default_lang")]
    pub lang: String,
    /// Page-segmentation mode. One of: 3 (auto), 6 (single uniform block,
    /// default), 7 (single line), 11 (sparse text).
    #[serde(default = "default_psm")]
    pub psm: u8,
}

/// Convert a wire-side PSM number into the typed [`Psm`]. Only the four
/// useful values are accepted; everything else is an `InvalidArgs`.
fn parse_psm(tool: &str, n: u8) -> Result<Psm, DispatchError> {
    match n {
        3 => Ok(Psm::Auto),
        6 => Ok(Psm::SingleBlock),
        7 => Ok(Psm::SingleLine),
        11 => Ok(Psm::SparseText),
        other => Err(DispatchError::InvalidArgs {
            tool: tool.to_string(),
            message: format!(
                "psm must be one of 3, 6, 7, 11; got {other}"
            ),
        }),
    }
}

/// Inputs to the composite `find_text_position` tool. Search a region (or
/// the full screen) for word runs that match `query` and return their
/// bboxes in compositor coordinates.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct FindTextPositionArgs {
    /// Text to search for. Tokenized on whitespace; multi-token queries
    /// match a run of consecutive tesseract words (e.g. `"Save File"`
    /// matches a row that tesseract split into two words).
    pub query: String,
    /// Optional region. When unset, searches the whole compositor.
    /// Coordinates may be negative (multi-monitor layouts).
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub w: Option<u32>,
    #[serde(default)]
    pub h: Option<u32>,
    /// Case-sensitive match. Defaults to false.
    #[serde(default)]
    pub case_sensitive: bool,
    /// Drop tesseract words scoring below this 0-100 confidence.
    /// Default 50. Lower to catch faint text; raise to suppress noise.
    #[serde(default = "default_min_confidence")]
    pub min_confidence: i32,
    /// Tesseract page-segmentation mode. 3 (auto), 6 (single block),
    /// 7 (single line), 11 (sparse text, default for scanning).
    #[serde(default = "default_scan_psm")]
    pub psm: u8,
    /// Tesseract language code (e.g. `eng`, `eng+fra`). Default `eng`.
    #[serde(default = "default_lang")]
    pub lang: String,
}

/// Inputs to the composite `click_text` tool. Superset of
/// [`FindTextPositionArgs`] plus button + dispatch controls.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ClickTextArgs {
    pub query: String,
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub w: Option<u32>,
    #[serde(default)]
    pub h: Option<u32>,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default = "default_min_confidence")]
    pub min_confidence: i32,
    #[serde(default = "default_scan_psm")]
    pub psm: u8,
    #[serde(default = "default_lang")]
    pub lang: String,
    /// `left` (default), `right`, `middle`, `x1`/`back`, `x2`/`forward`.
    #[serde(default)]
    pub button: Option<String>,
    /// Which match to click if `query` is ambiguous. Default 0.
    #[serde(default)]
    pub match_index: Option<usize>,
    /// Fail loudly if more than one match was found and the caller did
    /// not pin `match_index`. Default true — prefer "explain the
    /// ambiguity" over "click whatever". Set to false to take the first
    /// match silently.
    #[serde(default = "default_true")]
    pub require_unique: bool,
    /// When true (default), do not move/click. Return what WOULD be
    /// clicked plus the centre coordinates. Pass `false` to apply.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct A11yTreeArgs {
    /// Process id of the app to introspect. When omitted, the focused
    /// window's pid is used (resolved from the world-model cache).
    #[serde(default)]
    pub pid: Option<i32>,
    /// Cap on nodes walked. Default 1500. Large apps (browsers) are
    /// truncated; scope by app or use `a11y_find` instead.
    #[serde(default)]
    pub max_nodes: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct A11yFindArgs {
    /// Substring to match against each element's accessible name or inline
    /// text (case-insensitive). Empty matches on `role` alone.
    #[serde(default)]
    pub query: String,
    /// Restrict to this AT-SPI role, e.g. `push button`, `entry`, `check box`,
    /// `menu item`. Case-insensitive, exact role match.
    #[serde(default)]
    pub role: Option<String>,
    /// Process id of the app to search. Omit to use the focused window.
    #[serde(default)]
    pub pid: Option<i32>,
    /// Cap on nodes walked. Default 1500.
    #[serde(default)]
    pub max_nodes: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct A11yClickArgs {
    /// Substring matched against the element's accessible name or inline text.
    pub query: String,
    /// Restrict to this AT-SPI role (e.g. `push button`). Case-insensitive.
    #[serde(default)]
    pub role: Option<String>,
    /// Process id of the app. Omit to target the focused window's app.
    #[serde(default)]
    pub pid: Option<i32>,
    /// `left` (default), `right`, `middle`, `x1`/`back`, `x2`/`forward`.
    #[serde(default)]
    pub button: Option<String>,
    /// Which match to click when the query is ambiguous. Default 0.
    #[serde(default)]
    pub match_index: Option<usize>,
    /// Fail loudly when more than one element matches and `match_index` was
    /// not pinned. Default true.
    #[serde(default = "default_true")]
    pub require_unique: bool,
    /// Cap on nodes walked. Default 1500.
    #[serde(default)]
    pub max_nodes: Option<usize>,
    /// When true (default), do not move/click: return the matched element and
    /// the computed screen coordinates. Pass `false` to apply.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotMutArgs {
    /// Snapshot name. Allowed chars: `[A-Za-z0-9_.-]+`, must not start with `.`,
    /// max 128 chars.
    pub name: String,
    /// When true (default), do not modify Hyprland; return a preview only.
    /// Pass `false` to actually apply the change.
    #[serde(default = "default_true")]
    pub dry_run: bool,
}

// =============================================================================
// Registry
// =============================================================================

/// All tools known to this server, in stable order. The registry is the
/// single source of truth for `tools/list`.
pub fn registry() -> Vec<ToolDef> {
    use ToolGroup::*;
    vec![
        // ---- Queries (group: read) -------------------------------------------
        def::<NoArgs>(
            "query_clients",
            Read,
            false,
            "List all open windows managed by Hyprland. Returns a JSON array of \
             clients with address, class, title, workspace, monitor, geometry, \
             pid, and floating/fullscreen state. Read-only.",
        ),
        def::<NoArgs>(
            "query_workspaces",
            Read,
            false,
            "List all workspaces with id, name, monitor, window count, and \
             whether any window is currently fullscreen on them. Read-only.",
        ),
        def::<NoArgs>(
            "query_monitors",
            Read,
            false,
            "List physical monitors with name, geometry, scale, refresh rate, \
             and which workspace is currently active on each. Read-only.",
        ),
        def::<NoArgs>(
            "query_active_window",
            Read,
            false,
            "Return the currently focused window, or `null` if no window has \
             focus. Read-only.",
        ),
        def::<NoArgs>(
            "query_active_workspace",
            Read,
            false,
            "Return the currently focused workspace. Read-only.",
        ),
        def::<NoArgs>(
            "query_version",
            Read,
            false,
            "Return the live Hyprland version, commit, and build tag. Use this \
             to detect compositor-version-specific behaviour. Read-only.",
        ),
        def::<NoArgs>(
            "query_cursor_pos",
            Read,
            false,
            "Return the cursor's screen-space (x, y) position in pixels. Read-only.",
        ),
        def::<NoArgs>(
            "query_binds",
            Read,
            false,
            "List the user's live Hyprland keybinds: decoded modifier names \
             (`mods`: e.g. `[\"SUPER\"]`), key, dispatcher, and arg, plus submap \
             and flags. Read from the running compositor (always current), so \
             prefer this over parsing ~/.config/hypr or asking the user. Use it \
             to discover what a chord does (e.g. find the terminal bind) before \
             triggering it with `input_keys`. Read-only.",
        ),
        // ---- Window state (group: window) ------------------------------------
        def::<SelectorArgs>(
            "focus_window",
            Window,
            true,
            "Move keyboard focus to a window. Reversible by focusing the \
             previously-focused window. Not recorded in the undo stack (focus \
             changes are too frequent to be useful there).",
        ),
        def::<MutNoArgs>(
            "cycle_next",
            Window,
            true,
            "Cycle focus to the next tiled window in the focused workspace. \
             No state mutation beyond focus.",
        ),
        def::<MutNoArgs>(
            "cycle_prev",
            Window,
            true,
            "Cycle focus to the previous tiled window in the focused workspace.",
        ),
        def::<MutNoArgs>(
            "toggle_floating",
            Window,
            true,
            "Toggle floating mode on the focused window. Reversible by calling \
             again. Not in undo stack.",
        ),
        def::<FullscreenArgs>(
            "set_fullscreen",
            Window,
            true,
            "Set fullscreen mode on the focused window. `maximize` covers the \
             monitor while respecting gaps and bars; `fullscreen` is true \
             exclusive. Reversible by calling with the same mode again.",
        ),
        def::<MutNoArgs>(
            "center_active",
            Window,
            true,
            "Center the focused window on its monitor. Floating windows only \
             — has no effect on tiled windows.",
        ),
        def::<MutNoArgs>(
            "pin_active",
            Window,
            true,
            "Toggle the pinned (sticky-across-workspaces) state of the focused \
             window. Floating windows only.",
        ),
        def::<DirectionArgs>(
            "move_active",
            Window,
            true,
            "Move the focused window in a direction (`l`, `r`, `u`, `d`). For \
             tiled windows this swaps with the neighbour; for floating, it \
             nudges by a fixed amount configured in Hyprland.",
        ),
        def::<ResizeArgs>(
            "resize_active",
            Window,
            true,
            "Resize the focused window by (dx, dy) pixels. Negative values \
             shrink. For tiled windows this adjusts the split ratio.",
        ),
        def::<DirectionArgs>(
            "swap_active",
            Window,
            true,
            "Swap the focused window with its neighbour in the given direction. \
             Tiled windows only.",
        ),
        // ---- Workspace (group: workspace) ------------------------------------
        def::<WorkspaceArgs>(
            "switch_workspace",
            Workspace,
            true,
            "Switch the active workspace on the focused monitor. Does not move \
             any windows.",
        ),
        def::<WorkspaceArgs>(
            "move_to_workspace",
            Workspace,
            true,
            "Move the focused window to the target workspace and follow it. \
             Recorded in the undo stack: undo will move the window back to its \
             prior workspace and re-focus it.",
        ),
        def::<WorkspaceArgs>(
            "move_to_workspace_silent",
            Workspace,
            true,
            "Move the focused window to the target workspace without changing \
             focus or visible workspace. Recorded in the undo stack.",
        ),
        def::<FocusMonitorArgs>(
            "focus_monitor",
            Workspace,
            true,
            "Focus a monitor by name (e.g. `eDP-1`) or direction (`l`, `r`, \
             `u`, `d`).",
        ),
        def::<MonitorArgs>(
            "move_workspace_to_monitor",
            Workspace,
            true,
            "Move the focused workspace to a named monitor. Windows go with it.",
        ),
        // ---- Destructive (group: destructive) --------------------------------
        def::<MutNoArgs>(
            "kill_active",
            Destructive,
            true,
            "Close the currently focused window. DESTRUCTIVE: the application \
             receives a close signal and loses unsaved state. The daemon \
             captures the killed process's command line; undo will re-spawn \
             a fresh instance (state is NOT restored). Requires `dry_run=false` \
             to actually execute.",
        ),
        def::<SelectorArgs>(
            "close_window",
            Destructive,
            true,
            "Close a specific window matched by selector. DESTRUCTIVE — same \
             caveats as `kill_active`. Recorded in the undo stack when a \
             matching window is found at call time.",
        ),
        // ---- Process spawn (group: process) ----------------------------------
        def::<ExecArgs>(
            "exec",
            Process,
            true,
            "Spawn an external process via Hyprland's exec dispatcher. \
             POTENTIALLY DANGEROUS: this runs an arbitrary shell command with \
             the user's full privileges. Not in the undo stack — spawned \
             processes are not tracked. Requires `dry_run=false`.",
        ),
        // ---- Undo (group: undo) ---------------------------------------------
        def::<MutNoArgs>(
            "undo",
            Undo,
            true,
            "Reverse the most recent reversible operation recorded by the \
             daemon. May fail if the inverse cannot be applied (e.g. a window \
             that was moved no longer exists). `dry_run=true` reports which \
             entry would be undone without applying it.",
        ),
        def::<NoArgs>(
            "undo_list",
            Undo,
            false,
            "List the daemon's undo stack: index, timestamp, and description \
             of each reversible entry. The most recent entry has index 0. \
             Read-only.",
        ),
        // ---- Snapshots (group: snapshot) -------------------------------------
        def::<SnapshotNameArgs>(
            "snapshot_save",
            Snapshot,
            true,
            "Capture the current Hyprland state (windows + workspaces + \
             monitors) as a named snapshot on disk. Use to mark a known-good \
             layout before experimenting. The snapshot can be restored later \
             via `snapshot_restore`.",
        ),
        def::<NoArgs>(
            "snapshot_list",
            Snapshot,
            false,
            "List all on-disk snapshot names. Read-only.",
        ),
        def::<SnapshotNameArgs>(
            "snapshot_diff",
            Snapshot,
            false,
            "Show what a `snapshot_restore` of this snapshot would change: \
             which windows would move, which would toggle floating, which \
             are missing, and which exist now but weren't in the snapshot. \
             Read-only. Use this to preview a restore before applying.",
        ),
        def::<SnapshotMutArgs>(
            "snapshot_restore",
            Snapshot,
            true,
            "Restore a named snapshot. Best-effort: matches live windows to \
             snapshot entries by address, then pid, then (initial_class, \
             initial_title). For each match, restores workspace, floating \
             state, floating geometry, fullscreen mode, and pin. \
             Re-spawn of missing windows is NOT supported — those are \
             reported and skipped. \
             With `dry_run: true` (default) the response is a rich \
             preview: a plain-English summary, per-category counts, the \
             list of actions that would be applied, and the list that \
             would be skipped. With `dry_run: false` the preview is \
             still included alongside the per-action apply outcomes. \
             Before applying, the current state is auto-saved as \
             `_pre-restore-<unix_ts>` so this op is itself reversible.",
        ),
        def::<SnapshotNameArgs>(
            "snapshot_delete",
            Snapshot,
            true,
            "Delete a snapshot file from disk. Idempotent. Does not affect \
             live Hyprland state.",
        ),
        // ---- Rules (group: rules) -------------------------------------------
        def::<NoArgs>(
            "rules_list",
            Rules,
            false,
            "List the rules from the daemon's configured rules.toml. \
             Returns the rules array as declared in the file. The daemon \
             reads the file fresh on each call, so this reflects current \
             on-disk state; if you edit rules.toml after the daemon \
             started, the engine still uses the version it loaded at \
             startup — restart the daemon to reload. Read-only.",
        ),
        def::<NoArgs>(
            "rules_validate",
            Rules,
            false,
            "Parse and compile the rules.toml file. Returns \
             `{ ok: true, rules: N }` on success, or a structured error \
             with the offending rule label and action index. Use this to \
             check a config change before restarting the daemon. Read-only.",
        ),
        def::<NoArgs>(
            "rules_path",
            Rules,
            false,
            "Return the on-disk path the daemon reads rules from. \
             Useful for opening the file in an editor without guessing \
             the XDG resolution. Read-only.",
        ),
        // ---- Input (group: input) — NOT in default profile ------------------
        def::<InputTextArgs>(
            "input_type",
            Input,
            true,
            "Type free-form text into the focused window via wtype. \
             VERY DANGEROUS: any focused terminal will execute the typed \
             text as shell commands. Verify the focused window with \
             `query_active_window` before applying. Daemon requires \
             HYPRPILOT_DANGEROUS_INPUT_OK=1; this tool also requires the \
             `input` capability group, which is NOT in the default profile.",
        ),
        def::<InputKeysArgs>(
            "input_keys",
            Input,
            true,
            "Press a key chord in the focused window (e.g. `ctrl+shift+t`). \
             Uses wtype, so the chord goes wherever focus is. Same safety \
             gates as `input_type`.",
        ),
        def::<InputShortcutArgs>(
            "input_shortcut",
            Input,
            true,
            "Send a key chord to a specific window via Hyprland's \
             sendshortcut dispatcher. Does not require wtype and does not \
             change focus — the target window receives the chord even if \
             it is hidden on another workspace. Safer than `input_keys` \
             because it cannot accidentally fire in a terminal you didn't \
             mean, but still subject to the same capability + env-var gates.",
        ),
        def::<InputMouseMoveArgs>(
            "input_mouse_move",
            Input,
            true,
            "Move the mouse cursor via ydotool. With `absolute=true`, moves \
             to screen coordinates (x, y); otherwise moves by delta. \
             Requires ydotoold running with input-device access. Same \
             capability + env-var gates as the rest of the input surface.",
        ),
        def::<InputMouseClickArgs>(
            "input_mouse_click",
            Input,
            true,
            "Click a mouse button at the current cursor position via \
             ydotool. Button names: left, right, middle, x1/back, \
             x2/forward. Same gates as input_mouse_move.",
        ),
        // ---- Vision (group: vision) — NOT in default profile ----------------
        def::<ScreenshotArgs>(
            "screenshot",
            Vision,
            false,
            "Capture a screenshot of the entire compositor (or a single \
             monitor if `monitor` is set). Returns the raw image bytes as a \
             single MCP `image` content block, base64-encoded. Defaults: \
             full compositor, no cursor, PNG. Privacy: this captures \
             whatever is on screen, including potentially sensitive content; \
             the `vision` capability group is NOT in the default profile.",
        ),
        def::<ScreenshotRegionArgs>(
            "screenshot_region",
            Vision,
            false,
            "Capture a rectangular region of the compositor in pixel \
             coordinates. `(x, y)` is the top-left corner; `w` and `h` are \
             the dimensions. Coordinates may be negative (multi-monitor \
             layouts). Returns the image as a base64 MCP `image` block. \
             Same privacy caveats as `screenshot`.",
        ),
        def::<ScreenshotMonitorArgs>(
            "screenshot_monitor",
            Vision,
            false,
            "Capture a specific monitor by name (e.g. `eDP-1`). \
             Shortcut equivalent to `screenshot` with `monitor` set. \
             Returns the image as a base64 MCP `image` block.",
        ),
        def::<OcrScreenArgs>(
            "ocr_screen",
            Vision,
            false,
            "Run tesseract OCR over the entire compositor capture and \
             return the extracted text. Defaults: language `eng`, PSM 6 \
             (single uniform block). Returns a `text` content block — \
             empty if tesseract found no text.",
        ),
        def::<OcrRegionArgs>(
            "ocr_region",
            Vision,
            false,
            "Capture a rectangular region of the compositor and run OCR \
             over it. Useful for reading a single dialog or panel without \
             OCR noise from the rest of the screen.",
        ),
        def::<FindTextPositionArgs>(
            "find_text_position",
            Vision,
            false,
            "Capture the screen (or a region) and find on-screen text \
             matching `query`. Returns an array of matches with the \
             bounding box (in compositor pixel coordinates) and \
             tesseract confidence for each. Multi-word queries match a \
             run of consecutive OCR'd words. Read-only. Useful as the \
             first half of click-by-label flows.",
        ),
        // click_text needs vision (it scans) AND input (it dispatches a \
        // mouse click). The capability system tags each tool with exactly \
        // one group; we tag this with `Input` because that is the \
        // strictly more privileged surface — anyone allowed to use \
        // `click_text` could already screenshot via the vision tools, \
        // but the inverse is not true.
        def::<ClickTextArgs>(
            "click_text",
            Input,
            true,
            "Scan the screen (or a region) for text matching `query` and \
             click the centre of the first match. Composite of \
             `find_text_position` + `input_mouse_move` + \
             `input_mouse_click`. dry_run defaults to true: returns the \
             match and target coordinates without moving the cursor. \
             VERY DANGEROUS for the same reasons as `input_mouse_click` \
             — OCR noise or off-by-one matches can fire a click in an \
             unexpected place. Requires the `input` capability group \
             (NOT in the default profile) and the daemon's \
             HYPRPILOT_DANGEROUS_INPUT_OK=1 gate.",
        ),
        // ---- Accessibility (group: a11y) — NOT in default profile -----------
        def::<A11yTreeArgs>(
            "a11y_tree",
            A11y,
            false,
            "Read the accessibility (AT-SPI) tree of an application as a flat \
             list of elements: role, accessible name, inline text, and a \
             window-relative bounding box for each. Structured UI content \
             WITHOUT a screenshot or OCR — prefer this over `ocr_screen` for \
             reading app content when the app exposes accessibility. `pid` \
             defaults to the focused window. Coverage is toolkit-dependent \
             (GTK/Qt/Firefox expose rich trees; terminals, games, and some \
             Electron apps expose little) — fall back to vision when empty. \
             Read-only. The `a11y` capability group is NOT in the default \
             profile (a field's text may be sensitive).",
        ),
        def::<A11yFindArgs>(
            "a11y_find",
            A11y,
            false,
            "Find accessibility elements whose name or text matches `query` \
             (and, optionally, whose role equals `role`, e.g. `push button`). \
             Returns matches with window-relative bounding boxes. The \
             structured, OCR-free analogue of `find_text_position`, and the \
             first half of a click-by-label flow. `pid` defaults to the \
             focused window. Read-only.",
        ),
        def::<A11yClickArgs>(
            "a11y_click",
            Input,
            true,
            "Find an accessibility element by label/role and click its centre. \
             Composite of `a11y_find` + window geometry + `input_mouse_move` + \
             `input_mouse_click` — no OCR, exact target box. dry_run defaults \
             to true: returns the matched element and the computed screen \
             coordinates (window origin + element box) without moving the \
             cursor, so you can verify the target first. Requires the `input` \
             capability group (NOT in the default profile) and the daemon's \
             HYPRPILOT_DANGEROUS_INPUT_OK=1 gate.",
        ),
    ]
}

fn def<T: JsonSchema>(
    name: &'static str,
    group: ToolGroup,
    mutating: bool,
    description: &'static str,
) -> ToolDef {
    let schema_obj = schema_for!(T);
    let mut schema = serde_json::to_value(schema_obj.schema)
        .unwrap_or(Value::Object(Default::default()));
    // schemars sets `title`; strip it — MCP doesn't need it and it adds noise
    // to the LLM-visible schema.
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("title");
    }
    ToolDef { name, group, description, mutating, schema }
}

// =============================================================================
// Dispatch
// =============================================================================

/// Parse the arguments for the named tool and decide whether to forward to
/// the daemon or return a dry-run preview.
pub fn dispatch(name: &str, args: Value) -> Result<Dispatch, DispatchError> {
    match name {
        // ---- queries (always forwarded) --------------------------------------
        "query_clients" => parse_no_args(name, args, Request::QueryClients),
        "query_workspaces" => parse_no_args(name, args, Request::QueryWorkspaces),
        "query_monitors" => parse_no_args(name, args, Request::QueryMonitors),
        "query_active_window" => parse_no_args(name, args, Request::QueryActiveWindow),
        "query_active_workspace" => parse_no_args(name, args, Request::QueryActiveWorkspace),
        "query_version" => parse_no_args(name, args, Request::QueryVersion),
        "query_cursor_pos" => parse_no_args(name, args, Request::QueryCursorPos),
        "query_binds" => parse_no_args(name, args, Request::QueryBinds),
        "undo_list" => parse_no_args(name, args, Request::UndoList),

        // ---- mutating with selector ----------------------------------------
        "focus_window" => parse_selector(name, args, |sel| Request::DispatchFocusWindow {
            selector: sel,
        }),
        "close_window" => parse_selector(name, args, |sel| Request::DispatchCloseWindow {
            selector: sel,
        }),

        // ---- mutating with workspace ref -----------------------------------
        "switch_workspace" => parse_workspace(name, args, |ws| {
            Request::DispatchSwitchWorkspace { workspace: ws }
        }),
        "move_to_workspace" => parse_workspace(name, args, |ws| {
            Request::DispatchMoveToWorkspace { workspace: ws }
        }),
        "move_to_workspace_silent" => parse_workspace(name, args, |ws| {
            Request::DispatchMoveToWorkspaceSilent { workspace: ws }
        }),

        // ---- mutating with no/simple args ----------------------------------
        "cycle_next" => parse_mut_no_args(name, args, Request::DispatchCycleNext),
        "cycle_prev" => parse_mut_no_args(name, args, Request::DispatchCyclePrev),
        "toggle_floating" => parse_mut_no_args(name, args, Request::DispatchToggleFloating),
        "center_active" => parse_mut_no_args(name, args, Request::DispatchCenterActive),
        "pin_active" => parse_mut_no_args(name, args, Request::DispatchPinActive),
        "kill_active" => parse_mut_no_args(name, args, Request::DispatchKillActive),
        "undo" => parse_mut_no_args(name, args, Request::Undo),

        "set_fullscreen" => {
            let p: FullscreenArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::DispatchFullscreen { mode: p.mode.to_core() },
                || format!("would set fullscreen mode={}", mode_name(&p.mode)),
            )
        }

        "move_active" => {
            let p: DirectionArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::DispatchMoveActive { direction: p.direction.to_core() },
                || format!("would move active window {:?}", p.direction),
            )
        }
        "resize_active" => {
            let p: ResizeArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::DispatchResizeActive { dx: p.dx, dy: p.dy },
                || format!("would resize active window by ({}, {}) px", p.dx, p.dy),
            )
        }
        "swap_active" => {
            let p: DirectionArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::DispatchSwapActive { direction: p.direction.to_core() },
                || format!("would swap active window {:?}", p.direction),
            )
        }

        "focus_monitor" => {
            let p: FocusMonitorArgs = parse_args(name, args)?;
            let name_or_dir = p.name_or_direction.clone();
            forward_or_preview(
                p.dry_run,
                Request::DispatchFocusMonitor { name: p.name_or_direction },
                || format!("would focus monitor `{name_or_dir}`"),
            )
        }
        "move_workspace_to_monitor" => {
            let p: MonitorArgs = parse_args(name, args)?;
            let mon = p.monitor.clone();
            forward_or_preview(
                p.dry_run,
                Request::DispatchMoveWorkspaceToMonitor { monitor: p.monitor },
                || format!("would move focused workspace to monitor `{mon}`"),
            )
        }

        "exec" => {
            let p: ExecArgs = parse_args(name, args)?;
            let cmd = p.command.clone();
            forward_or_preview(
                p.dry_run,
                Request::DispatchExec { command: p.command },
                || format!("would spawn: {cmd}"),
            )
        }

        // ---- snapshots -----------------------------------------------------
        "snapshot_save" => {
            let p: SnapshotNameArgs = parse_args(name, args)?;
            Ok(Dispatch::Forward(Request::SnapshotSave { name: p.name }))
        }
        "snapshot_list" => parse_no_args(name, args, Request::SnapshotList),
        "snapshot_diff" => {
            let p: SnapshotNameArgs = parse_args(name, args)?;
            Ok(Dispatch::Forward(Request::SnapshotDiff { name: p.name }))
        }
        "snapshot_restore" => {
            let p: SnapshotMutArgs = parse_args(name, args)?;
            Ok(if p.dry_run {
                // Defer to the server, which calls the daemon's
                // SnapshotPreview RPC to fetch a rich preview (summary +
                // will_apply / will_skip). Generic Preview text is too
                // vague for a restore — the operation can be dozens of
                // mutations and the LLM needs the structured shape.
                Dispatch::PreviewSnapshotRestore { name: p.name }
            } else {
                Dispatch::Forward(Request::SnapshotRestore { name: p.name })
            })
        }
        "snapshot_delete" => {
            let p: SnapshotNameArgs = parse_args(name, args)?;
            Ok(Dispatch::Forward(Request::SnapshotDelete { name: p.name }))
        }

        // ---- rules ---------------------------------------------------------
        "rules_list" => parse_no_args(name, args, Request::RulesList),
        "rules_validate" => parse_no_args(name, args, Request::RulesValidate),
        "rules_path" => parse_no_args(name, args, Request::RulesPath),

        // ---- input ---------------------------------------------------------
        "input_type" => {
            let p: InputTextArgs = parse_args(name, args)?;
            let preview_text = p.text.clone();
            forward_or_preview(
                p.dry_run,
                Request::InputType { text: p.text },
                || {
                    format!(
                        "would type {} bytes into the focused window: {:?}",
                        preview_text.len(),
                        truncate(&preview_text, 80),
                    )
                },
            )
        }
        "input_keys" => {
            let p: InputKeysArgs = parse_args(name, args)?;
            let combo = parse_combo(name, &p.combo)?;
            let combo_label = combo.to_string();
            forward_or_preview(
                p.dry_run,
                Request::InputKeys { combo },
                || format!("would press `{combo_label}` in the focused window"),
            )
        }
        "input_shortcut" => {
            let p: InputShortcutArgs = parse_args(name, args)?;
            let combo = parse_combo(name, &p.combo)?;
            let selector = WindowSelector::parse(&p.selector).map_err(|e| {
                DispatchError::InvalidArgs { tool: name.to_string(), message: e }
            })?;
            let combo_label = combo.to_string();
            let sel_label = p.selector.clone();
            forward_or_preview(
                p.dry_run,
                Request::InputShortcut { combo, selector },
                || format!("would send `{combo_label}` to `{sel_label}`"),
            )
        }
        "input_mouse_move" => {
            let p: InputMouseMoveArgs = parse_args(name, args)?;
            forward_or_preview(
                p.dry_run,
                Request::InputMouseMove { x: p.x, y: p.y, absolute: p.absolute },
                || {
                    let mode = if p.absolute { "to absolute" } else { "by delta" };
                    format!("would move mouse {mode} ({}, {})", p.x, p.y)
                },
            )
        }
        "input_mouse_click" => {
            let p: InputMouseClickArgs = parse_args(name, args)?;
            let button = MouseButton::parse(&p.button).map_err(|e| {
                DispatchError::InvalidArgs { tool: name.to_string(), message: e.to_string() }
            })?;
            let label = format!("{button:?}");
            forward_or_preview(
                p.dry_run,
                Request::InputMouseClick { button },
                || format!("would click {label} at current cursor position"),
            )
        }

        // ---- vision -------------------------------------------------------
        "screenshot" => {
            let p: ScreenshotArgs = parse_args(name, args)?;
            Ok(Dispatch::Capture(CaptureOp::Full {
                monitor: p.monitor,
                cursor: p.cursor,
                format: p.format.to_vision(),
            }))
        }
        "screenshot_region" => {
            let p: ScreenshotRegionArgs = parse_args(name, args)?;
            let region = Region { x: p.x, y: p.y, w: p.w, h: p.h };
            region.validate().map_err(|e| DispatchError::InvalidArgs {
                tool: name.to_string(),
                message: e.to_string(),
            })?;
            Ok(Dispatch::Capture(CaptureOp::Region {
                region,
                cursor: p.cursor,
                format: p.format.to_vision(),
            }))
        }
        "screenshot_monitor" => {
            let p: ScreenshotMonitorArgs = parse_args(name, args)?;
            Ok(Dispatch::Capture(CaptureOp::Full {
                monitor: Some(p.name),
                cursor: p.cursor,
                format: p.format.to_vision(),
            }))
        }
        "ocr_screen" => {
            let p: OcrScreenArgs = parse_args(name, args)?;
            let psm = parse_psm(name, p.psm)?;
            Ok(Dispatch::Ocr(OcrOp::Screen { lang: p.lang, psm }))
        }
        "ocr_region" => {
            let p: OcrRegionArgs = parse_args(name, args)?;
            let psm = parse_psm(name, p.psm)?;
            let region = Region { x: p.x, y: p.y, w: p.w, h: p.h };
            region.validate().map_err(|e| DispatchError::InvalidArgs {
                tool: name.to_string(),
                message: e.to_string(),
            })?;
            Ok(Dispatch::Ocr(OcrOp::Region { region, lang: p.lang, psm }))
        }

        "find_text_position" => {
            let p: FindTextPositionArgs = parse_args(name, args)?;
            if p.query.trim().is_empty() {
                return Err(DispatchError::InvalidArgs {
                    tool: name.to_string(),
                    message: "query must not be empty".into(),
                });
            }
            let region = build_optional_region(name, p.x, p.y, p.w, p.h)?;
            let psm = parse_psm(name, p.psm)?;
            Ok(Dispatch::FindText(FindTextOp {
                query: p.query,
                region,
                case_sensitive: p.case_sensitive,
                min_confidence: p.min_confidence,
                psm,
                lang: p.lang,
            }))
        }

        "click_text" => {
            let p: ClickTextArgs = parse_args(name, args)?;
            if p.query.trim().is_empty() {
                return Err(DispatchError::InvalidArgs {
                    tool: name.to_string(),
                    message: "query must not be empty".into(),
                });
            }
            let region = build_optional_region(name, p.x, p.y, p.w, p.h)?;
            let psm = parse_psm(name, p.psm)?;
            let button = match p.button.as_deref() {
                None => MouseButton::Left,
                Some(s) => MouseButton::parse(s).map_err(|e| {
                    DispatchError::InvalidArgs {
                        tool: name.to_string(),
                        message: e.to_string(),
                    }
                })?,
            };
            Ok(Dispatch::ClickText(ClickTextOp {
                query: p.query,
                region,
                case_sensitive: p.case_sensitive,
                min_confidence: p.min_confidence,
                psm,
                lang: p.lang,
                button,
                match_index: p.match_index.unwrap_or(0),
                require_unique: p.require_unique,
                dry_run: p.dry_run,
            }))
        }

        // ---- accessibility -------------------------------------------------
        "a11y_tree" => {
            let p: A11yTreeArgs = parse_args(name, args)?;
            Ok(Dispatch::Forward(Request::A11yTree {
                pid: p.pid,
                max_nodes: p.max_nodes,
            }))
        }
        "a11y_find" => {
            let p: A11yFindArgs = parse_args(name, args)?;
            Ok(Dispatch::Forward(Request::A11yFind {
                pid: p.pid,
                query: p.query,
                role: p.role,
                max_nodes: p.max_nodes,
            }))
        }
        "a11y_click" => {
            let p: A11yClickArgs = parse_args(name, args)?;
            if p.query.trim().is_empty() {
                return Err(DispatchError::InvalidArgs {
                    tool: name.to_string(),
                    message: "query must not be empty".into(),
                });
            }
            let button = match p.button.as_deref() {
                None => MouseButton::Left,
                Some(s) => MouseButton::parse(s).map_err(|e| DispatchError::InvalidArgs {
                    tool: name.to_string(),
                    message: e.to_string(),
                })?,
            };
            Ok(Dispatch::A11yClick(A11yClickOp {
                query: p.query,
                role: p.role,
                pid: p.pid,
                button,
                match_index: p.match_index.unwrap_or(0),
                require_unique: p.require_unique,
                max_nodes: p.max_nodes,
                dry_run: p.dry_run,
            }))
        }

        other => Err(DispatchError::UnknownTool(other.to_string())),
    }
}

/// Either all of (x, y, w, h) are present or none. Reject mixed inputs
/// (e.g. just `x` with no `w`) — silently ignoring a partial region would
/// surprise callers who think they're scanning a region.
fn build_optional_region(
    tool: &str,
    x: Option<i32>,
    y: Option<i32>,
    w: Option<u32>,
    h: Option<u32>,
) -> Result<Option<Region>, DispatchError> {
    match (x, y, w, h) {
        (None, None, None, None) => Ok(None),
        (Some(x), Some(y), Some(w), Some(h)) => {
            let r = Region { x, y, w, h };
            r.validate().map_err(|e| DispatchError::InvalidArgs {
                tool: tool.to_string(),
                message: e.to_string(),
            })?;
            Ok(Some(r))
        }
        _ => Err(DispatchError::InvalidArgs {
            tool: tool.to_string(),
            message: "region requires all of x, y, w, h (or none of them)".into(),
        }),
    }
}

fn parse_combo(tool: &str, s: &str) -> Result<KeyCombo, DispatchError> {
    KeyCombo::parse(s).map_err(|e| DispatchError::InvalidArgs {
        tool: tool.to_string(),
        message: e.to_string(),
    })
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Find a char boundary at or before `max`.
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

// ---- helpers ----------------------------------------------------------------

fn parse_args<T: for<'de> Deserialize<'de>>(
    tool: &str,
    args: Value,
) -> Result<T, DispatchError> {
    // serde rejects `null` for structs with required fields; treat `null`
    // and missing args the same as `{}` for ergonomics.
    let args = if args.is_null() { Value::Object(Default::default()) } else { args };
    serde_json::from_value(args).map_err(|e| DispatchError::InvalidArgs {
        tool: tool.to_string(),
        message: e.to_string(),
    })
}

fn parse_no_args(
    name: &str,
    args: Value,
    request: Request,
) -> Result<Dispatch, DispatchError> {
    let _: NoArgs = parse_args(name, args)?;
    Ok(Dispatch::Forward(request))
}

fn parse_mut_no_args(
    name: &str,
    args: Value,
    request: Request,
) -> Result<Dispatch, DispatchError> {
    let p: MutNoArgs = parse_args(name, args)?;
    Ok(forward_or_preview_static(p.dry_run, request, name))
}

fn parse_selector(
    name: &str,
    args: Value,
    build: impl FnOnce(WindowSelector) -> Request,
) -> Result<Dispatch, DispatchError> {
    let p: SelectorArgs = parse_args(name, args)?;
    let sel = WindowSelector::parse(&p.selector).map_err(|e| DispatchError::InvalidArgs {
        tool: name.to_string(),
        message: e,
    })?;
    let sel_str = p.selector.clone();
    forward_or_preview(p.dry_run, build(sel), || {
        format!("would {} on selector `{}`", name, sel_str)
    })
}

fn parse_workspace(
    name: &str,
    args: Value,
    build: impl FnOnce(WorkspaceRef) -> Request,
) -> Result<Dispatch, DispatchError> {
    let p: WorkspaceArgs = parse_args(name, args)?;
    let ws = WorkspaceRef::parse(&p.target).map_err(|e| DispatchError::InvalidArgs {
        tool: name.to_string(),
        message: e,
    })?;
    let ws_str = p.target.clone();
    forward_or_preview(p.dry_run, build(ws), || {
        format!("would {} target=`{}`", name, ws_str)
    })
}

fn forward_or_preview(
    dry_run: bool,
    request: Request,
    preview: impl FnOnce() -> String,
) -> Result<Dispatch, DispatchError> {
    Ok(if dry_run {
        Dispatch::Preview(preview())
    } else {
        Dispatch::Forward(request)
    })
}

fn forward_or_preview_static(dry_run: bool, request: Request, name: &str) -> Dispatch {
    if dry_run {
        Dispatch::Preview(format!("would invoke {name}"))
    } else {
        Dispatch::Forward(request)
    }
}

fn mode_name(m: &FsModeArg) -> &'static str {
    match m {
        FsModeArg::Maximize => "maximize",
        FsModeArg::Fullscreen => "fullscreen",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_no_duplicate_names() {
        let reg = registry();
        let mut names: Vec<_> = reg.iter().map(|t| t.name).collect();
        names.sort();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len, "duplicate tool name in registry");
    }

    #[test]
    fn registry_count() {
        // Lock the surface so additions are deliberate. Update this number
        // intentionally when adding/removing tools.
        assert_eq!(registry().len(), 51);
    }

    #[test]
    fn input_tools_are_in_input_group() {
        let reg = registry();
        for name in [
            "input_type",
            "input_keys",
            "input_shortcut",
            "input_mouse_move",
            "input_mouse_click",
            "click_text",
        ] {
            let t = reg
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("missing tool `{name}` in registry"));
            assert_eq!(t.group, crate::capability::ToolGroup::Input);
            assert!(t.mutating, "input tool `{name}` must be flagged mutating");
        }
    }

    #[test]
    fn default_profile_does_not_include_input_tools() {
        let p = crate::capability::Profile::default_safe();
        for name in [
            "input_type",
            "input_keys",
            "input_shortcut",
            "input_mouse_move",
            "input_mouse_click",
            "click_text",
        ] {
            assert!(
                !p.allows(name, crate::capability::ToolGroup::Input),
                "default profile must NOT permit `{name}`"
            );
        }
    }

    #[test]
    fn dispatch_unknown_tool() {
        let err = dispatch("does_not_exist", Value::Null).unwrap_err();
        match err {
            DispatchError::UnknownTool(s) => assert_eq!(s, "does_not_exist"),
            other => panic!("expected UnknownTool, got {other:?}"),
        }
    }

    #[test]
    fn query_clients_no_args_forwards() {
        let d = dispatch("query_clients", Value::Null).unwrap();
        assert!(matches!(d, Dispatch::Forward(Request::QueryClients)));
    }

    #[test]
    fn kill_active_dry_run_default_previews() {
        let d = dispatch("kill_active", serde_json::json!({})).unwrap();
        assert!(matches!(d, Dispatch::Preview(_)));
    }

    #[test]
    fn kill_active_explicit_apply_forwards() {
        let d = dispatch("kill_active", serde_json::json!({"dry_run": false})).unwrap();
        assert!(matches!(d, Dispatch::Forward(Request::DispatchKillActive)));
    }

    #[test]
    fn focus_window_parses_stringy_selector() {
        let d = dispatch(
            "focus_window",
            serde_json::json!({"selector": "class:Slack", "dry_run": false}),
        )
        .unwrap();
        match d {
            Dispatch::Forward(Request::DispatchFocusWindow { selector }) => {
                assert_eq!(selector, WindowSelector::Class("Slack".into()));
            }
            other => panic!("expected Forward(DispatchFocusWindow), got {other:?}"),
        }
    }

    #[test]
    fn focus_window_dry_run_schema_has_description() {
        let reg = registry();
        let tool = reg
            .iter()
            .find(|t| t.name == "focus_window")
            .expect("focus_window in registry");
        let desc = tool.schema["properties"]["dry_run"]["description"]
            .as_str()
            .expect("dry_run description should be a string");
        assert!(!desc.is_empty(), "dry_run description should be non-empty");
    }

    #[test]
    fn find_text_position_parses_minimal_args() {
        let d = dispatch(
            "find_text_position",
            serde_json::json!({"query": "Save"}),
        )
        .unwrap();
        match d {
            Dispatch::FindText(op) => {
                assert_eq!(op.query, "Save");
                assert!(op.region.is_none());
                assert!(!op.case_sensitive);
                assert_eq!(op.min_confidence, 50);
            }
            other => panic!("expected FindText, got {other:?}"),
        }
    }

    #[test]
    fn find_text_position_rejects_partial_region() {
        let err = dispatch(
            "find_text_position",
            serde_json::json!({"query": "X", "x": 0, "y": 0}),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::InvalidArgs { .. }));
    }

    #[test]
    fn find_text_position_rejects_empty_query() {
        let err = dispatch(
            "find_text_position",
            serde_json::json!({"query": "   "}),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::InvalidArgs { .. }));
    }

    #[test]
    fn find_text_position_accepts_full_region() {
        let d = dispatch(
            "find_text_position",
            serde_json::json!({
                "query": "Save",
                "x": 100, "y": 200, "w": 400u32, "h": 300u32,
                "psm": 6,
            }),
        )
        .unwrap();
        match d {
            Dispatch::FindText(op) => {
                let r = op.region.expect("region present");
                assert_eq!((r.x, r.y, r.w, r.h), (100, 200, 400, 300));
            }
            other => panic!("expected FindText, got {other:?}"),
        }
    }

    #[test]
    fn click_text_defaults_to_dry_run_preview_path() {
        let d = dispatch(
            "click_text",
            serde_json::json!({"query": "OK"}),
        )
        .unwrap();
        match d {
            Dispatch::ClickText(op) => {
                assert!(op.dry_run, "click_text MUST default to dry_run=true");
                assert_eq!(op.match_index, 0);
                assert!(op.require_unique);
                assert!(matches!(op.button, hyprpilot_input::keys::MouseButton::Left));
            }
            other => panic!("expected ClickText, got {other:?}"),
        }
    }

    #[test]
    fn click_text_apply_path_carries_dry_run_false() {
        let d = dispatch(
            "click_text",
            serde_json::json!({"query": "OK", "dry_run": false, "button": "right"}),
        )
        .unwrap();
        match d {
            Dispatch::ClickText(op) => {
                assert!(!op.dry_run);
                assert!(matches!(op.button, hyprpilot_input::keys::MouseButton::Right));
            }
            other => panic!("expected ClickText, got {other:?}"),
        }
    }

    #[test]
    fn click_text_rejects_bad_button() {
        let err = dispatch(
            "click_text",
            serde_json::json!({"query": "OK", "button": "nonsense"}),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::InvalidArgs { .. }));
    }

    #[test]
    fn click_text_registered_in_input_group() {
        let reg = registry();
        let t = reg.iter().find(|t| t.name == "click_text").expect("click_text in registry");
        assert_eq!(t.group, crate::capability::ToolGroup::Input);
        assert!(t.mutating);
    }

    #[test]
    fn find_text_position_registered_in_vision_group() {
        let reg = registry();
        let t = reg
            .iter()
            .find(|t| t.name == "find_text_position")
            .expect("find_text_position in registry");
        assert_eq!(t.group, crate::capability::ToolGroup::Vision);
        assert!(!t.mutating);
    }

    #[test]
    fn move_to_workspace_rejects_bad_target() {
        let err = dispatch(
            "move_to_workspace",
            serde_json::json!({"target": "not_a_workspace", "dry_run": false}),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::InvalidArgs { .. }));
    }

    #[test]
    fn a11y_tree_forwards_with_defaults() {
        let d = dispatch("a11y_tree", serde_json::json!({})).unwrap();
        match d {
            Dispatch::Forward(Request::A11yTree { pid, max_nodes }) => {
                assert_eq!(pid, None, "pid defaults to focused window (None)");
                assert_eq!(max_nodes, None);
            }
            other => panic!("expected Forward(A11yTree), got {other:?}"),
        }
    }

    #[test]
    fn a11y_find_forwards_query_and_role() {
        let d = dispatch(
            "a11y_find",
            serde_json::json!({"query": "Submit", "role": "push button", "pid": 4321}),
        )
        .unwrap();
        match d {
            Dispatch::Forward(Request::A11yFind { pid, query, role, .. }) => {
                assert_eq!(pid, Some(4321));
                assert_eq!(query, "Submit");
                assert_eq!(role.as_deref(), Some("push button"));
            }
            other => panic!("expected Forward(A11yFind), got {other:?}"),
        }
    }

    #[test]
    fn a11y_click_defaults_to_dry_run() {
        let d = dispatch("a11y_click", serde_json::json!({"query": "OK"})).unwrap();
        match d {
            Dispatch::A11yClick(op) => {
                assert!(op.dry_run, "a11y_click MUST default to dry_run=true");
                assert_eq!(op.match_index, 0);
                assert!(op.require_unique);
                assert!(matches!(op.button, MouseButton::Left));
            }
            other => panic!("expected A11yClick, got {other:?}"),
        }
    }

    #[test]
    fn a11y_click_rejects_empty_query() {
        let err = dispatch("a11y_click", serde_json::json!({"query": "  "})).unwrap_err();
        assert!(matches!(err, DispatchError::InvalidArgs { .. }));
    }

    #[test]
    fn a11y_click_rejects_bad_button() {
        let err = dispatch("a11y_click", serde_json::json!({"query": "OK", "button": "nope"}))
            .unwrap_err();
        assert!(matches!(err, DispatchError::InvalidArgs { .. }));
    }

    #[test]
    fn a11y_read_tools_are_in_a11y_group_and_not_mutating() {
        let reg = registry();
        for name in ["a11y_tree", "a11y_find"] {
            let t = reg.iter().find(|t| t.name == name).expect("a11y tool in registry");
            assert_eq!(t.group, crate::capability::ToolGroup::A11y);
            assert!(!t.mutating);
        }
        let click = reg.iter().find(|t| t.name == "a11y_click").unwrap();
        assert_eq!(click.group, crate::capability::ToolGroup::Input, "a11y_click moves the mouse");
        assert!(click.mutating);
    }

    #[test]
    fn default_profile_excludes_a11y_tools() {
        let p = crate::capability::Profile::default_safe();
        for name in ["a11y_tree", "a11y_find", "a11y_click"] {
            let group = registry().iter().find(|t| t.name == name).unwrap().group;
            assert!(!p.allows(name, group), "default profile must NOT permit `{name}`");
        }
    }
}
