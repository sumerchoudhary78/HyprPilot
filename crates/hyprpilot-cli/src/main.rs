use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use hyprpilot_core::dispatch::{Direction, FullscreenMode};
use hyprpilot_core::selector::{WindowSelector, WorkspaceRef};
use hyprpilot_core::Connection;
use hyprpilot_daemon::{client::DaemonClient, default_socket_path, protocol::Request};
use hyprpilot_input::keys::{KeyCombo, MouseButton};
use hyprpilot_vision::{
    find_word_runs, BBox, GrimCapture, ImageFormat, Psm, Region, TesseractOcr,
};

#[derive(Parser, Debug)]
#[command(
    name = "hyprpilot",
    version,
    about = "Typed control over Hyprland from the shell or via a daemon"
)]
struct Cli {
    /// Force every mutating op through the daemon (required for undo).
    #[arg(long, global = true)]
    use_daemon: bool,

    /// Override daemon socket path.
    #[arg(long, global = true, value_name = "PATH")]
    daemon_socket: Option<PathBuf>,

    /// Emit results as JSON instead of pretty text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Read-only queries against Hyprland.
    #[command(subcommand)]
    Query(QueryCmd),
    /// Window operations.
    #[command(subcommand)]
    Win(WinCmd),
    /// Workspace operations.
    #[command(subcommand)]
    Ws(WsCmd),
    /// Spawn an external command via Hyprland's `exec` dispatcher.
    Exec {
        /// The shell command to spawn.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Daemon control.
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Undo the last reversible operation (requires daemon).
    Undo,
    /// List the daemon's undo stack (requires daemon).
    UndoList,
    /// Named snapshots of window/workspace state.
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
    /// Declarative event-to-action rules.
    #[command(subcommand)]
    Rules(RulesCmd),
    /// Input synthesis: typing, key chords, mouse. Disabled by default
    /// in the daemon; requires HYPRPILOT_DANGEROUS_INPUT_OK=1 in the
    /// daemon's environment.
    #[command(subcommand)]
    Input(InputCmd),
    /// Screen capture via `grim` (wlr-screencopy).
    #[command(subcommand)]
    Capture(CaptureCmd),
    /// OCR via `tesseract`. Captures with `grim` then extracts text.
    #[command(subcommand)]
    Ocr(OcrCmd),
    /// Find on-screen text matching a query; print matches + bboxes as JSON.
    FindText(FindTextCmd),
    /// Click on-screen text matching a query. Defaults to dry-run.
    ClickText(ClickTextCmd),
}

#[derive(Args, Debug)]
struct FindTextCmd {
    /// Text to look for. Tokenized on whitespace; multi-word queries
    /// match a run of consecutive OCR'd words.
    query: String,
    /// Optional region: x y w h (all four or none).
    #[arg(long, allow_hyphen_values = true, num_args = 4, value_names = ["X", "Y", "W", "H"])]
    region: Option<Vec<i32>>,
    #[arg(long)]
    case_sensitive: bool,
    #[arg(long, default_value_t = 50)]
    min_confidence: i32,
    /// PSM mode: 3 (auto), 6 (single block), 7 (line), 11 (sparse, default).
    #[arg(long, default_value_t = 11)]
    psm: u8,
    #[arg(long, default_value = "eng")]
    lang: String,
}

#[derive(Args, Debug)]
struct ClickTextCmd {
    query: String,
    #[arg(long, allow_hyphen_values = true, num_args = 4, value_names = ["X", "Y", "W", "H"])]
    region: Option<Vec<i32>>,
    #[arg(long)]
    case_sensitive: bool,
    #[arg(long, default_value_t = 50)]
    min_confidence: i32,
    #[arg(long, default_value_t = 11)]
    psm: u8,
    #[arg(long, default_value = "eng")]
    lang: String,
    /// left, right, middle, x1/back, x2/forward.
    #[arg(long, default_value = "left")]
    button: String,
    /// Which match to click. Defaults to 0.
    #[arg(long, default_value_t = 0)]
    match_index: usize,
    /// Without this, ambiguous matches abort.
    #[arg(long)]
    allow_ambiguous: bool,
    /// Apply for real. Without this, prints what would be clicked and exits.
    #[arg(long)]
    apply: bool,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum ImageFormatArg {
    Png,
    Jpeg,
    Ppm,
}

impl ImageFormatArg {
    fn into_vision(self) -> ImageFormat {
        match self {
            ImageFormatArg::Png => ImageFormat::Png,
            ImageFormatArg::Jpeg => ImageFormat::Jpeg,
            ImageFormatArg::Ppm => ImageFormat::Ppm,
        }
    }
}

#[derive(Subcommand, Debug)]
enum CaptureCmd {
    /// Capture the whole compositor (or a single monitor).
    Full {
        /// Monitor name (e.g. `eDP-1`). Omit for the whole compositor.
        #[arg(long)]
        monitor: Option<String>,
        /// Include the mouse cursor in the capture.
        #[arg(long)]
        cursor: bool,
        /// Image format. Defaults to `png`.
        #[arg(long, value_enum, default_value_t = ImageFormatArg::Png)]
        format: ImageFormatArg,
        /// File to write image bytes to. If omitted, writes to stdout.
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// Capture a rectangular region of the compositor.
    Region {
        #[arg(allow_hyphen_values = true)]
        x: i32,
        #[arg(allow_hyphen_values = true)]
        y: i32,
        w: u32,
        h: u32,
        /// Include the mouse cursor in the capture.
        #[arg(long)]
        cursor: bool,
        /// Image format. Defaults to `png`.
        #[arg(long, value_enum, default_value_t = ImageFormatArg::Png)]
        format: ImageFormatArg,
        /// File to write image bytes to. If omitted, writes to stdout.
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum OcrCmd {
    /// OCR the full screen. Prints text to stdout.
    Screen {
        /// Tesseract language code, e.g. `eng`, `eng+fra`. Defaults to `eng`.
        #[arg(long, default_value = "eng")]
        lang: String,
        /// Tesseract page-segmentation mode. One of: 3 (auto), 6
        /// (single-block, default), 7 (single-line), 11 (sparse).
        #[arg(long, default_value_t = 6)]
        psm: u8,
    },
    /// OCR a rectangular region of the screen.
    Region {
        #[arg(allow_hyphen_values = true)]
        x: i32,
        #[arg(allow_hyphen_values = true)]
        y: i32,
        w: u32,
        h: u32,
        /// Tesseract language code, e.g. `eng`, `eng+fra`. Defaults to `eng`.
        #[arg(long, default_value = "eng")]
        lang: String,
        /// Tesseract page-segmentation mode. One of: 3 (auto), 6
        /// (single-block, default), 7 (single-line), 11 (sparse).
        #[arg(long, default_value_t = 6)]
        psm: u8,
    },
}

#[derive(Subcommand, Debug)]
enum InputCmd {
    /// Type free-form text into the focused window (via wtype).
    Type {
        /// Text to type. Use `--` to pass text that begins with `-`.
        #[arg(trailing_var_arg = true, required = true)]
        text: Vec<String>,
    },
    /// Press a key chord. Routed through ydotool (uinput→libinput) when
    /// available so Hyprland global binds fire (e.g. `super+T`); falls back
    /// to wtype (focused window only, no binds) if ydotoold is unreachable.
    Keys {
        /// e.g. `ctrl+shift+t`, `super+space`, `Escape`.
        combo: String,
    },
    /// Send a key chord to a specific window (via Hyprland sendshortcut;
    /// no external backend, no focus change).
    Shortcut {
        /// Key chord, e.g. `ctrl+t`.
        combo: String,
        /// Window selector: `active`, `class:Foo`, `pid:1234`, etc.
        target: String,
    },
    /// Move the mouse cursor (via ydotool).
    MouseMove {
        #[arg(allow_hyphen_values = true)]
        x: i32,
        #[arg(allow_hyphen_values = true)]
        y: i32,
        /// Interpret (x, y) as absolute screen coordinates rather than a
        /// delta from the current cursor position.
        #[arg(short, long)]
        absolute: bool,
    },
    /// Click a mouse button (via ydotool).
    MouseClick {
        /// `left`, `right`, `middle`, `x1`/`back`, `x2`/`forward`.
        button: String,
    },
}

#[derive(Subcommand, Debug)]
enum RulesCmd {
    /// Show the rules the daemon would load (re-reads the file each call).
    List,
    /// Parse + compile the rules file. Reports the first error if invalid.
    Validate,
    /// Print the path the daemon reads rules from.
    Path,
}

#[derive(Subcommand, Debug)]
enum SnapshotCmd {
    /// Capture the current state as a named snapshot.
    Save { name: String },
    /// List all on-disk snapshots.
    List,
    /// Show what restoring a snapshot would change, without applying.
    Diff { name: String },
    /// Restore a snapshot. Auto-saves a `_pre-restore-<ts>` snapshot first.
    Restore { name: String },
    /// Delete a snapshot.
    Delete { name: String },
}

#[derive(Subcommand, Debug)]
enum QueryCmd {
    Clients,
    Workspaces,
    Monitors,
    ActiveWindow,
    ActiveWorkspace,
    Version,
    CursorPos,
    /// List the user's keybinds (live from Hyprland) with decoded modifiers.
    Binds,
}

#[derive(Subcommand, Debug)]
enum WinCmd {
    /// Focus a window by selector.
    Focus(SelectorArg),
    /// Close a specific window by selector.
    Close(SelectorArg),
    /// Close the focused window (records undo when going through the daemon).
    Kill,
    /// Cycle focus to the next (or previous) tiled window.
    Cycle {
        #[arg(long)]
        prev: bool,
    },
    /// Toggle floating mode on the focused window.
    Float,
    /// Set fullscreen mode for the focused window.
    Fullscreen {
        #[arg(value_enum, default_value_t = FsMode::Fullscreen)]
        mode: FsMode,
    },
    /// Center the focused window (floating only).
    Center,
    /// Toggle pinned (sticky across workspaces) on the focused window.
    Pin,
    /// Move the focused window in a direction.
    Move {
        #[arg(value_enum)]
        direction: Dir,
    },
    /// Resize the focused window by (dx, dy) pixels.
    Resize { dx: i32, dy: i32 },
    /// Swap the focused window with its neighbour.
    Swap {
        #[arg(value_enum)]
        direction: Dir,
    },
}

#[derive(Subcommand, Debug)]
enum WsCmd {
    /// Switch to a workspace.
    Switch {
        #[arg(value_parser = parse_workspace_ref, allow_hyphen_values = true)]
        target: WorkspaceRef,
    },
    /// Send the focused window to a workspace and follow it.
    Send {
        #[arg(value_parser = parse_workspace_ref, allow_hyphen_values = true)]
        target: WorkspaceRef,
    },
    /// Send the focused window silently (no focus change).
    SendSilent {
        #[arg(value_parser = parse_workspace_ref, allow_hyphen_values = true)]
        target: WorkspaceRef,
    },
    /// Move the focused workspace to a named monitor.
    ToMonitor { monitor: String },
    /// Focus a named monitor (or direction: l/r/u/d).
    FocusMonitor { name_or_direction: String },
}

#[derive(Subcommand, Debug)]
enum DaemonCmd {
    /// Run the daemon in the foreground.
    Run {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Ping a running daemon.
    Ping,
    /// Ask the daemon to shut down.
    Stop,
    /// Show whether a daemon is reachable on the socket.
    Status,
}

#[derive(Args, Debug)]
struct SelectorArg {
    /// Selector: `address:0x...`, `pid:1234`, `class:kitty`, `title:foo`,
    /// `tag:x`, or `active`.
    selector: String,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum Dir { L, R, U, D }
impl Dir {
    fn into_core(self) -> Direction {
        match self {
            Dir::L => Direction::Left,
            Dir::R => Direction::Right,
            Dir::U => Direction::Up,
            Dir::D => Direction::Down,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum FsMode { Maximize, Fullscreen }
impl FsMode {
    fn into_core(self) -> FullscreenMode {
        match self {
            FsMode::Maximize => FullscreenMode::Maximize,
            FsMode::Fullscreen => FullscreenMode::Fullscreen,
        }
    }
}

fn parse_workspace_ref(s: &str) -> std::result::Result<WorkspaceRef, String> {
    WorkspaceRef::parse(s)
}

fn parse_selector(s: &str) -> Result<WindowSelector> {
    WindowSelector::parse(s).map_err(|e| anyhow!(e))
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Daemon(c) => run_daemon_cmd(c, cli.daemon_socket, cli.json).await,
        Cmd::Query(q) => run_query(q, cli.use_daemon, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Win(w) => run_win(w, cli.use_daemon, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Ws(w) => run_ws(w, cli.use_daemon, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Exec { command } => {
            let cmd = command.join(" ");
            let conn = Connection::new()?;
            conn.exec(&cmd).await?;
            emit_json_or_text(cli.json, &serde_json::json!({ "spawned": cmd }), "ok")
        }
        Cmd::Undo => {
            let mut client = open_daemon(cli.daemon_socket.as_deref()).await?;
            let v: serde_json::Value = client.call(Request::Undo).await?;
            emit_json_or_text(cli.json, &v, &format!("{v}"))
        }
        Cmd::UndoList => {
            let mut client = open_daemon(cli.daemon_socket.as_deref()).await?;
            let v: serde_json::Value = client.call(Request::UndoList).await?;
            emit_json_or_text(cli.json, &v, &pretty_undo_list(&v))
        }
        Cmd::Snapshot(s) => run_snapshot_cmd(s, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Rules(r) => run_rules_cmd(r, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Input(i) => run_input_cmd(i, cli.daemon_socket.as_deref(), cli.json).await,
        Cmd::Capture(c) => run_capture_cmd(c).await,
        Cmd::Ocr(o) => run_ocr_cmd(o).await,
        Cmd::FindText(f) => run_find_text_cmd(f, cli.json).await,
        Cmd::ClickText(c) => run_click_text_cmd(c, cli.daemon_socket.as_deref(), cli.json).await,
    }
}

fn region_from_vec(parts: Option<Vec<i32>>) -> Result<Option<Region>> {
    let Some(v) = parts else { return Ok(None) };
    if v.len() != 4 {
        return Err(anyhow!("--region needs exactly 4 values: X Y W H"));
    }
    let (x, y, w, h) = (v[0], v[1], v[2], v[3]);
    if w <= 0 || h <= 0 {
        return Err(anyhow!("--region width/height must be > 0; got {w}x{h}"));
    }
    Ok(Some(Region { x, y, w: w as u32, h: h as u32 }))
}

async fn run_find_text_cmd(f: FindTextCmd, _json: bool) -> Result<()> {
    let region = region_from_vec(f.region)?;
    let psm = parse_psm(f.psm).map_err(|e| anyhow!(e))?;
    let cap = GrimCapture::detect()?;
    let bytes = match region {
        Some(r) => cap.region(r, ImageFormat::Png).await?,
        None => cap.full(None, ImageFormat::Png).await?,
    };
    let ocr = TesseractOcr::detect()?
        .with_lang(f.lang)
        .with_psm(psm)
        .with_min_confidence(f.min_confidence);
    let words = ocr.extract_words(&bytes).await?;
    let mut matches = find_word_runs(&words, &f.query, f.case_sensitive);
    if let Some(r) = region {
        for m in &mut matches {
            m.bbox = BBox { x: m.bbox.x + r.x, y: m.bbox.y + r.y, w: m.bbox.w, h: m.bbox.h };
        }
    }
    let body = serde_json::json!({ "query": f.query, "match_count": matches.len(), "matches": matches });
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

async fn run_click_text_cmd(
    c: ClickTextCmd,
    socket: Option<&std::path::Path>,
    _json: bool,
) -> Result<()> {
    let region = region_from_vec(c.region)?;
    let psm = parse_psm(c.psm).map_err(|e| anyhow!(e))?;
    let button = MouseButton::parse(&c.button).map_err(|e| anyhow!(e))?;

    let cap = GrimCapture::detect()?;
    let bytes = match region {
        Some(r) => cap.region(r, ImageFormat::Png).await?,
        None => cap.full(None, ImageFormat::Png).await?,
    };
    let ocr = TesseractOcr::detect()?
        .with_lang(c.lang)
        .with_psm(psm)
        .with_min_confidence(c.min_confidence);
    let words = ocr.extract_words(&bytes).await?;
    let mut matches = find_word_runs(&words, &c.query, c.case_sensitive);
    if let Some(r) = region {
        for m in &mut matches {
            m.bbox = BBox { x: m.bbox.x + r.x, y: m.bbox.y + r.y, w: m.bbox.w, h: m.bbox.h };
        }
    }

    if matches.is_empty() {
        return Err(anyhow!("no on-screen text matched `{}`", c.query));
    }
    if !c.allow_ambiguous && matches.len() > 1 && c.match_index == 0 {
        let summary = matches
            .iter()
            .enumerate()
            .take(8)
            .map(|(i, m)| {
                format!(
                    "  [{i}] `{}` at ({},{}) {}x{} conf={}",
                    m.text, m.bbox.x, m.bbox.y, m.bbox.w, m.bbox.h, m.confidence
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(anyhow!(
            "ambiguous: `{}` matched {} regions. Pass --match-index N or --allow-ambiguous:\n{summary}",
            c.query,
            matches.len()
        ));
    }
    if c.match_index >= matches.len() {
        return Err(anyhow!(
            "--match-index {} out of bounds: only {} match(es)",
            c.match_index,
            matches.len()
        ));
    }
    let chosen = &matches[c.match_index];
    let (cx, cy) = chosen.bbox.center();

    if !c.apply {
        println!(
            "[dry_run] would {button:?}-click `{}` at compositor ({cx}, {cy}) \
             (bbox {},{} {}x{}, conf {}, match {}/{}).\n\
             Pass --apply to click for real.",
            chosen.text,
            chosen.bbox.x, chosen.bbox.y, chosen.bbox.w, chosen.bbox.h,
            chosen.confidence,
            c.match_index, matches.len(),
        );
        return Ok(());
    }

    let mut client = open_daemon(socket).await?;
    let _: serde_json::Value = client
        .call(Request::InputMouseMove { x: cx, y: cy, absolute: true })
        .await?;
    let _: serde_json::Value = client.call(Request::InputMouseClick { button }).await?;
    let body = serde_json::json!({
        "clicked": chosen,
        "click_at": { "x": cx, "y": cy },
        "total_matches": matches.len(),
    });
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

async fn run_capture_cmd(c: CaptureCmd) -> Result<()> {
    let grim = GrimCapture::detect()?;
    let (bytes, output) = match c {
        CaptureCmd::Full { monitor, cursor, format, output } => {
            let g = grim.with_cursor(cursor);
            let bytes = g.full(monitor.as_deref(), format.into_vision()).await?;
            (bytes, output)
        }
        CaptureCmd::Region { x, y, w, h, cursor, format, output } => {
            let g = grim.with_cursor(cursor);
            let bytes = g.region(Region { x, y, w, h }, format.into_vision()).await?;
            (bytes, output)
        }
    };
    write_bytes(output.as_deref(), &bytes)
}

fn write_bytes(output: Option<&std::path::Path>, bytes: &[u8]) -> Result<()> {
    match output {
        Some(path) => std::fs::write(path, bytes)
            .with_context(|| format!("write capture bytes to {}", path.display())),
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(bytes).context("write capture bytes to stdout")
        }
    }
}

async fn run_ocr_cmd(o: OcrCmd) -> Result<()> {
    let grim = GrimCapture::detect()?;
    let (lang, psm_raw, image) = match o {
        OcrCmd::Screen { lang, psm } => {
            let bytes = grim.full(None, ImageFormat::Png).await?;
            (lang, psm, bytes)
        }
        OcrCmd::Region { x, y, w, h, lang, psm } => {
            let bytes = grim.region(Region { x, y, w, h }, ImageFormat::Png).await?;
            (lang, psm, bytes)
        }
    };
    let psm = parse_psm(psm_raw).map_err(|e| anyhow!(e))?;
    let ocr = TesseractOcr::detect()?.with_lang(lang).with_psm(psm);
    let text = ocr.extract_text(&image).await?;
    println!("{text}");
    Ok(())
}

/// Map a numeric `--psm` to the Psm enum. We deliberately accept only the
/// four modes that are useful for UI screenshots — auto (3), single-block
/// (6, default), single-line (7), sparse (11). Other values reject early
/// with a clear hint rather than silently passing through to tesseract.
fn parse_psm(mode: u8) -> std::result::Result<Psm, String> {
    match mode {
        3 => Ok(Psm::Auto),
        6 => Ok(Psm::SingleBlock),
        7 => Ok(Psm::SingleLine),
        11 => Ok(Psm::SparseText),
        other => Err(format!(
            "unsupported --psm value `{other}`: expected one of 3 (auto), 6 (single-block), 7 (single-line), 11 (sparse)"
        )),
    }
}

async fn run_input_cmd(
    i: InputCmd,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    let mut client = open_daemon(socket).await?;
    let req = match i {
        InputCmd::Type { text } => Request::InputType { text: text.join(" ") },
        InputCmd::Keys { combo } => Request::InputKeys {
            combo: KeyCombo::parse(&combo).map_err(|e| anyhow!(e))?,
        },
        InputCmd::Shortcut { combo, target } => Request::InputShortcut {
            combo: KeyCombo::parse(&combo).map_err(|e| anyhow!(e))?,
            selector: hyprpilot_core::WindowSelector::parse(&target).map_err(|e| anyhow!(e))?,
        },
        InputCmd::MouseMove { x, y, absolute } => Request::InputMouseMove { x, y, absolute },
        InputCmd::MouseClick { button } => Request::InputMouseClick {
            button: MouseButton::parse(&button).map_err(|e| anyhow!(e))?,
        },
    };
    let v: serde_json::Value = client.call(req).await?;
    emit_json_or_text(json, &v, &pretty(&v))
}

async fn run_rules_cmd(
    r: RulesCmd,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    let mut client = open_daemon(socket).await?;
    let req = match r {
        RulesCmd::List => Request::RulesList,
        RulesCmd::Validate => Request::RulesValidate,
        RulesCmd::Path => Request::RulesPath,
    };
    let v: serde_json::Value = client.call(req).await?;
    emit_json_or_text(json, &v, &pretty(&v))
}

async fn run_snapshot_cmd(
    s: SnapshotCmd,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    let mut client = open_daemon(socket).await?;
    // `Diff` uses the rich preview RPC so the CLI shows the same plain-English
    // summary the MCP tool surfaces. Other subcommands stay on their existing
    // RPCs; their JSON shapes are unchanged.
    let (req, label) = match s {
        SnapshotCmd::Save { name } => (Request::SnapshotSave { name: name.clone() }, name),
        SnapshotCmd::List => (Request::SnapshotList, "list".to_string()),
        SnapshotCmd::Diff { name } => (Request::SnapshotPreview { name: name.clone() }, name),
        SnapshotCmd::Restore { name } => (Request::SnapshotRestore { name: name.clone() }, name),
        SnapshotCmd::Delete { name } => (Request::SnapshotDelete { name: name.clone() }, name),
    };
    let v: serde_json::Value = client.call(req).await?;
    let text = match &v {
        serde_json::Value::Array(arr) if arr.iter().all(|x| x.is_string()) => {
            // snapshot list — render as a plain list
            arr.iter()
                .filter_map(|x| x.as_str())
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        }
        serde_json::Value::Object(obj) if obj.contains_key("human_summary") => {
            // snapshot preview (from `snapshot diff` or `snapshot restore`):
            // surface the human summary at the top, then the JSON.
            let summary = obj
                .get("human_summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("{summary}\n\n{}", pretty(&v))
        }
        serde_json::Value::Object(obj) if obj.contains_key("preview") => {
            // snapshot_restore wrapped response: dig out the embedded preview's
            // human_summary for the headline line.
            let summary = obj
                .get("preview")
                .and_then(|p| p.get("human_summary"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!("{summary}\n\n{}", pretty(&v))
        }
        _ => pretty(&v),
    };
    let _ = label;
    emit_json_or_text(json, &v, &text)
}

fn pretty_undo_list(v: &serde_json::Value) -> String {
    let Some(arr) = v.as_array() else { return v.to_string(); };
    if arr.is_empty() {
        return "(empty)".into();
    }
    let mut out = String::new();
    for entry in arr {
        let idx = entry.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
        let desc = entry.get("description").and_then(|x| x.as_str()).unwrap_or("");
        let ts = entry.get("timestamp_unix").and_then(|x| x.as_i64()).unwrap_or(0);
        out.push_str(&format!("{idx:>3}  t={ts}  {desc}\n"));
    }
    out
}

async fn run_daemon_cmd(c: DaemonCmd, socket_override: Option<PathBuf>, json: bool) -> Result<()> {
    match c {
        DaemonCmd::Run { socket } => {
            let path = socket.or(socket_override).unwrap_or_else(default_socket_path);
            hyprpilot_daemon::server::run(path).await
        }
        DaemonCmd::Ping => {
            let mut client = open_daemon(socket_override.as_deref()).await?;
            let v: serde_json::Value = client.call(Request::Ping).await?;
            emit_json_or_text(json, &v, &v.to_string())
        }
        DaemonCmd::Stop => {
            let mut client = open_daemon(socket_override.as_deref()).await?;
            let v: serde_json::Value = client.call(Request::Shutdown).await?;
            emit_json_or_text(json, &v, &v.to_string())
        }
        DaemonCmd::Status => {
            let path = socket_override.unwrap_or_else(default_socket_path);
            match DaemonClient::connect(&path).await {
                Ok(_) => emit_json_or_text(
                    json,
                    &serde_json::json!({ "reachable": true, "socket": path }),
                    &format!("reachable: {}", path.display()),
                ),
                Err(e) => emit_json_or_text(
                    json,
                    &serde_json::json!({
                        "reachable": false, "socket": path, "error": e.to_string()
                    }),
                    &format!("not reachable: {} ({e})", path.display()),
                ),
            }
        }
    }
}

async fn open_daemon(socket: Option<&std::path::Path>) -> Result<DaemonClient> {
    let path = socket.map(|p| p.to_path_buf()).unwrap_or_else(default_socket_path);
    DaemonClient::connect(&path)
        .await
        .with_context(|| format!("connect to daemon at {}", path.display()))
}

async fn run_query(
    q: QueryCmd,
    use_daemon: bool,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    if use_daemon {
        let mut client = open_daemon(socket).await?;
        let req = match q {
            QueryCmd::Clients => Request::QueryClients,
            QueryCmd::Workspaces => Request::QueryWorkspaces,
            QueryCmd::Monitors => Request::QueryMonitors,
            QueryCmd::ActiveWindow => Request::QueryActiveWindow,
            QueryCmd::ActiveWorkspace => Request::QueryActiveWorkspace,
            QueryCmd::Version => Request::QueryVersion,
            QueryCmd::CursorPos => Request::QueryCursorPos,
            QueryCmd::Binds => Request::QueryBinds,
        };
        let v: serde_json::Value = client.call(req).await?;
        return emit_json_or_text(json, &v, &pretty(&v));
    }
    let conn = Connection::new()?;
    match q {
        QueryCmd::Clients => emit(json, &conn.clients().await?),
        QueryCmd::Workspaces => emit(json, &conn.workspaces().await?),
        QueryCmd::Monitors => emit(json, &conn.monitors().await?),
        QueryCmd::ActiveWindow => emit(json, &conn.active_window().await?),
        QueryCmd::ActiveWorkspace => emit(json, &conn.active_workspace().await?),
        QueryCmd::Version => emit(json, &conn.version().await?),
        QueryCmd::CursorPos => emit(json, &conn.cursor_position().await?),
        QueryCmd::Binds => emit(json, &conn.binds().await?),
    }
}

async fn run_win(
    w: WinCmd,
    use_daemon: bool,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    if use_daemon {
        let mut client = open_daemon(socket).await?;
        let req = match w {
            WinCmd::Focus(s) => Request::DispatchFocusWindow { selector: parse_selector(&s.selector)? },
            WinCmd::Close(s) => Request::DispatchCloseWindow { selector: parse_selector(&s.selector)? },
            WinCmd::Kill => Request::DispatchKillActive,
            WinCmd::Cycle { prev } => {
                if prev { Request::DispatchCyclePrev } else { Request::DispatchCycleNext }
            }
            WinCmd::Float => Request::DispatchToggleFloating,
            WinCmd::Fullscreen { mode } => Request::DispatchFullscreen { mode: mode.into_core() },
            WinCmd::Center => Request::DispatchCenterActive,
            WinCmd::Pin => Request::DispatchPinActive,
            WinCmd::Move { direction } => Request::DispatchMoveActive { direction: direction.into_core() },
            WinCmd::Resize { dx, dy } => Request::DispatchResizeActive { dx, dy },
            WinCmd::Swap { direction } => Request::DispatchSwapActive { direction: direction.into_core() },
        };
        let v: serde_json::Value = client.call(req).await?;
        return emit_json_or_text(json, &v, &v.to_string());
    }
    let conn = Connection::new()?;
    match w {
        WinCmd::Focus(s) => conn.focus_window(&parse_selector(&s.selector)?).await?,
        WinCmd::Close(s) => conn.close_window(&parse_selector(&s.selector)?).await?,
        WinCmd::Kill => conn.kill_active().await?,
        WinCmd::Cycle { prev } => if prev { conn.cycle_prev().await? } else { conn.cycle_next().await? },
        WinCmd::Float => conn.toggle_floating().await?,
        WinCmd::Fullscreen { mode } => conn.set_fullscreen(mode.into_core()).await?,
        WinCmd::Center => conn.center_active().await?,
        WinCmd::Pin => conn.pin_active().await?,
        WinCmd::Move { direction } => conn.move_active(direction.into_core()).await?,
        WinCmd::Resize { dx, dy } => conn.resize_active(dx, dy).await?,
        WinCmd::Swap { direction } => conn.swap_active(direction.into_core()).await?,
    }
    emit_json_or_text(json, &serde_json::json!({ "ok": true }), "ok")
}

async fn run_ws(
    w: WsCmd,
    use_daemon: bool,
    socket: Option<&std::path::Path>,
    json: bool,
) -> Result<()> {
    if use_daemon {
        let mut client = open_daemon(socket).await?;
        let req = match w {
            WsCmd::Switch { target } => Request::DispatchSwitchWorkspace { workspace: target },
            WsCmd::Send { target } => Request::DispatchMoveToWorkspace { workspace: target },
            WsCmd::SendSilent { target } => Request::DispatchMoveToWorkspaceSilent { workspace: target },
            WsCmd::ToMonitor { monitor } => Request::DispatchMoveWorkspaceToMonitor { monitor },
            WsCmd::FocusMonitor { name_or_direction } => Request::DispatchFocusMonitor { name: name_or_direction },
        };
        let v: serde_json::Value = client.call(req).await?;
        return emit_json_or_text(json, &v, &v.to_string());
    }
    let conn = Connection::new()?;
    match w {
        WsCmd::Switch { target } => conn.switch_workspace(&target).await?,
        WsCmd::Send { target } => conn.move_active_to_workspace(&target).await?,
        WsCmd::SendSilent { target } => conn.move_active_to_workspace_silent(&target).await?,
        WsCmd::ToMonitor { monitor } => conn.move_workspace_to_monitor(&monitor).await?,
        WsCmd::FocusMonitor { name_or_direction } => conn.focus_monitor(&name_or_direction).await?,
    }
    emit_json_or_text(json, &serde_json::json!({ "ok": true }), "ok")
}

fn emit<T: Serialize>(_json: bool, v: &T) -> Result<()> {
    // v0.1: pretty JSON for both modes. A non-JSON pretty printer per type is
    // future work; for now, pretty JSON is already human-readable.
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn emit_json_or_text(json: bool, v: &serde_json::Value, text: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
    } else {
        println!("{text}");
    }
    Ok(())
}

fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_relative_workspace_parses_as_value_not_flag() {
        // Regression for issue #11: `ws switch -1` used to fail clap parsing
        // because clap treated `-1` as a short flag.
        let cli = Cli::parse_from(["hyprpilot", "ws", "switch", "-1"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::Switch { target }) => {
                assert_eq!(target, WorkspaceRef::Relative(-1));
            }
            other => panic!("expected Ws Switch, got {other:?}"),
        }
    }

    #[test]
    fn negative_relative_workspace_parses_for_send() {
        let cli = Cli::parse_from(["hyprpilot", "ws", "send", "-1"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::Send { target }) => {
                assert_eq!(target, WorkspaceRef::Relative(-1));
            }
            other => panic!("expected Ws Send, got {other:?}"),
        }
    }

    #[test]
    fn negative_relative_workspace_parses_for_send_silent() {
        let cli = Cli::parse_from(["hyprpilot", "ws", "send-silent", "-1"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::SendSilent { target }) => {
                assert_eq!(target, WorkspaceRef::Relative(-1));
            }
            other => panic!("expected Ws SendSilent, got {other:?}"),
        }
    }

    #[test]
    fn positive_relative_workspace_still_parses() {
        let cli = Cli::parse_from(["hyprpilot", "ws", "switch", "+1"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::Switch { target }) => {
                assert_eq!(target, WorkspaceRef::Relative(1));
            }
            other => panic!("expected Ws Switch, got {other:?}"),
        }
    }

    #[test]
    fn absolute_workspace_id_still_parses() {
        let cli = Cli::parse_from(["hyprpilot", "ws", "switch", "2"]);
        match cli.cmd {
            Cmd::Ws(WsCmd::Switch { target }) => {
                assert_eq!(target, WorkspaceRef::Id(2));
            }
            other => panic!("expected Ws Switch, got {other:?}"),
        }
    }

    #[test]
    fn mouse_move_accepts_negative_x() {
        let cli = Cli::parse_from(["hyprpilot", "input", "mouse-move", "-10", "20"]);
        match cli.cmd {
            Cmd::Input(InputCmd::MouseMove { x, y, absolute }) => {
                assert_eq!((x, y, absolute), (-10, 20, false));
            }
            other => panic!("expected Input MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn mouse_move_accepts_negative_y() {
        let cli = Cli::parse_from(["hyprpilot", "input", "mouse-move", "10", "-20"]);
        match cli.cmd {
            Cmd::Input(InputCmd::MouseMove { x, y, absolute }) => {
                assert_eq!((x, y, absolute), (10, -20, false));
            }
            other => panic!("expected Input MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn mouse_move_accepts_both_negative() {
        let cli = Cli::parse_from(["hyprpilot", "input", "mouse-move", "-10", "-20"]);
        match cli.cmd {
            Cmd::Input(InputCmd::MouseMove { x, y, absolute }) => {
                assert_eq!((x, y, absolute), (-10, -20, false));
            }
            other => panic!("expected Input MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn mouse_move_absolute_flag_still_parses_with_negative_coords() {
        let cli = Cli::parse_from(["hyprpilot", "input", "mouse-move", "--absolute", "-5", "-5"]);
        match cli.cmd {
            Cmd::Input(InputCmd::MouseMove { x, y, absolute }) => {
                assert_eq!((x, y, absolute), (-5, -5, true));
            }
            other => panic!("expected Input MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn parse_psm_accepts_supported_modes() {
        assert!(matches!(parse_psm(3), Ok(Psm::Auto)));
        assert!(matches!(parse_psm(6), Ok(Psm::SingleBlock)));
        assert!(matches!(parse_psm(7), Ok(Psm::SingleLine)));
        assert!(matches!(parse_psm(11), Ok(Psm::SparseText)));
    }

    #[test]
    fn parse_psm_rejects_zero_with_clear_error() {
        let err = parse_psm(0).expect_err("psm 0 should reject");
        assert!(err.contains("unsupported --psm value"), "got: {err}");
        assert!(err.contains("0"), "got: {err}");
        // The hint should enumerate the accepted modes.
        assert!(err.contains("3"), "got: {err}");
        assert!(err.contains("6"), "got: {err}");
        assert!(err.contains("7"), "got: {err}");
        assert!(err.contains("11"), "got: {err}");
    }

    #[test]
    fn parse_psm_rejects_twelve_with_clear_error() {
        let err = parse_psm(12).expect_err("psm 12 should reject");
        assert!(err.contains("unsupported --psm value"), "got: {err}");
        assert!(err.contains("12"), "got: {err}");
    }

    #[test]
    fn capture_region_parses_negative_origin() {
        let cli = Cli::parse_from([
            "hyprpilot", "capture", "region", "-10", "-20", "100", "50",
        ]);
        match cli.cmd {
            Cmd::Capture(CaptureCmd::Region { x, y, w, h, .. }) => {
                assert_eq!((x, y, w, h), (-10, -20, 100, 50));
            }
            other => panic!("expected Capture Region, got {other:?}"),
        }
    }

    #[test]
    fn capture_full_default_format_is_png() {
        let cli = Cli::parse_from(["hyprpilot", "capture", "full"]);
        match cli.cmd {
            Cmd::Capture(CaptureCmd::Full { format, cursor, monitor, output }) => {
                assert!(matches!(format, ImageFormatArg::Png));
                assert!(!cursor);
                assert!(monitor.is_none());
                assert!(output.is_none());
            }
            other => panic!("expected Capture Full, got {other:?}"),
        }
    }

    #[test]
    fn ocr_screen_defaults() {
        let cli = Cli::parse_from(["hyprpilot", "ocr", "screen"]);
        match cli.cmd {
            Cmd::Ocr(OcrCmd::Screen { lang, psm }) => {
                assert_eq!(lang, "eng");
                assert_eq!(psm, 6);
            }
            other => panic!("expected Ocr Screen, got {other:?}"),
        }
    }
}
