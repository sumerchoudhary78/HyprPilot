# HyprPilot

Programmatic control of Hyprland for AI agents and humans. Typed Rust IPC
client, long-running daemon with undo and snapshots, CLI, MCP server, input
synthesis, and screen capture + OCR.

> Status: v0.6 in development. Adds screen capture (`grim`) and OCR
> (`tesseract`) as a new `hyprpilot-vision` crate, with five new MCP tools
> in a `Vision` capability group (excluded from the default profile because
> screenshots can capture private content) and matching `hyprpilot capture`
> / `hyprpilot ocr` CLI subcommands.

## Crates

- `hyprpilot-core` — async Hyprland IPC client (control socket + event
  socket), typed queries and dispatchers, error types. No daemon awareness.
- `hyprpilot-daemon` — long-running process exposing JSON-RPC over a Unix
  socket. Records reversible operations in an undo stack.
- `hyprpilot-cli` — `hyprpilot` binary. Subcommands for queries,
  window/workspace ops, daemon control, and undo.
- `hyprpilot-mcp` — `hyprpilot-mcp` stdio MCP server. Exposes Hyprland
  control to MCP hosts (Claude Desktop, Claude Code) as typed tools with
  capability profiles and dry-run default.
- `hyprpilot-input` — input synthesis (typing, key chords, mouse) via
  `wtype`, `ydotool`, and Hyprland's `sendshortcut`.
- `hyprpilot-vision` — screen capture via `grim` (wlr-screencopy) and
  OCR via `tesseract`. Daemon-agnostic: image bytes are bulky and
  stateless, so the CLI and MCP server call into this crate directly.

## Quick start (CLI)

```sh
cargo build --release

# Direct queries (no daemon needed)
./target/release/hyprpilot query active-window
./target/release/hyprpilot query clients

# Window ops (direct to Hyprland)
./target/release/hyprpilot win cycle
./target/release/hyprpilot ws switch 2

# Daemon-backed undo
./target/release/hyprpilot-daemon &
./target/release/hyprpilot --use-daemon ws send +1
./target/release/hyprpilot undo
./target/release/hyprpilot daemon stop
```

## Quick start (MCP for agents)

1. Run the daemon (persists across MCP sessions, owns undo state):
   ```sh
   ./target/release/hyprpilot-daemon &
   ```

2. Register the MCP server with your host. Example Claude Desktop config
   (`~/.config/Claude/claude_desktop_config.json`):
   ```json
   {
     "mcpServers": {
       "hyprland": {
         "command": "/absolute/path/to/target/release/hyprpilot-mcp",
         "args": ["--profile", "default"]
       }
     }
   }
   ```

3. Restart the host. It will discover ~24 tools by default (read,
   window, workspace, undo groups). Destructive and process tools are
   hidden unless you switch to a more permissive profile.

### Capability profiles

Profiles live at `~/.config/hyprpilot/profiles/<name>.toml`. Built-in
profiles available without a file:

- `default` — read + window + workspace + undo. Hides kill, close, exec.
- `unrestricted` — every tool. Use sparingly.

Profile file format:

```toml
# ~/.config/hyprpilot/profiles/strict.toml
allow = ["read", "window", "workspace", "undo"]
allow_tools = ["kill_active"]   # add specific tools outside the groups
deny_tools  = ["focus_window"]  # remove specific tools from the groups
```

Tool groups: `read`, `window`, `workspace`, `destructive`, `process`, `undo`.

### Dry-run

Mutating tools accept a `dry_run: bool` argument, default `true`. With
`dry_run=true` the server returns a preview ("would …") and does *not*
touch the daemon. Agents must pass `dry_run=false` to actually mutate.

This is layered on top of capability profiles; both must agree before
a mutation reaches the daemon.

### Snapshots

Capture a known-good layout and restore to it later. Snapshots are JSON
files under `$XDG_STATE_HOME/hyprpilot/snapshots/<name>.json`.

```sh
./target/release/hyprpilot snapshot save before-meeting
./target/release/hyprpilot snapshot list
./target/release/hyprpilot snapshot diff before-meeting   # preview restore
./target/release/hyprpilot snapshot restore before-meeting
# Auto-saves a `_pre-restore-<unix_ts>` snapshot so the restore itself is
# reversible.
./target/release/hyprpilot snapshot delete before-meeting
```

Restore is best-effort. It matches live windows to snapshot entries by
address, then PID, then `(initial_class, initial_title)`. For each
match, the diff is computed across these dimensions:

- **workspace** — always.
- **floating** state — always.
- **floating-window geometry** — exact (x, y) and (w, h), via
  `movewindowpixel` and `resizewindowpixel`. Tiled windows are
  layout-driven, so geometry diffs are suppressed for them.
- **fullscreen mode** — via `fullscreenstate`, no focus disturbance.
- **pinned state** — for floating windows only (Hyprland refuses to
  pin tiled windows).
- **active focus** — the snapshot's focused window is refocused last
  so it overrides any focus side-effects from earlier actions.

Re-spawn of missing windows (snapshot entries whose process is gone) is
future work. Windows present live but absent from the snapshot are left
alone; restore is never destructive.

### Persistent undo

The daemon's undo stack is persisted to
`$XDG_STATE_HOME/hyprpilot/undo.json` on every push/pop. Surviving a
daemon restart means `hyprpilot undo` still works after the daemon's
process dies. Malformed files are surfaced at startup; the stack starts
empty rather than aborting.

### Input synthesis (v0.5)

Type text, press key chords, move and click the mouse. Built on
external tools plus Hyprland's native `sendshortcut` for per-window
key delivery.

Backend routing per operation:

| Operation | Backend | Why |
|---|---|---|
| `input type` | `wtype` (virtual-keyboard) | Fast batched text into the focused window; bind-triggering is irrelevant for plain text. |
| `input keys` | `ydotool` (uinput→libinput), falls back to `wtype` | **ydotool events reach Hyprland's global bind matcher, so `super+T` fires its `bind`.** wtype's virtual-keyboard events are filtered out of the bind matcher (focused client only). |
| `input mouse-move` / `mouse-click` | `ydotool` | Pointer synthesis. |
| `input shortcut` | Hyprland `sendshortcut` | Per-window, no focus change, no external backend. |

> **`input keys` needs `ydotoold` running** to trigger binds. hyprpilot
> looks for its socket at `$YDOTOOL_SOCKET`, then
> `$XDG_RUNTIME_DIR/.ydotool_socket`, then `/tmp/.ydotool_socket`. If
> ydotoold is unreachable, `input keys` degrades to wtype — chords still
> reach the focused window but **won't** trigger compositor binds. Run
> ydotoold as a `systemd --user` service so it's always available.

**This is the most dangerous tool surface in HyprPilot.** An agent
that can type can execute arbitrary shell commands in any focused
terminal. Two independent gates apply before any input call reaches
the wire:

1. **Daemon env-var.** The daemon refuses every `Input*` request
   unless started with `HYPRPILOT_DANGEROUS_INPUT_OK=1`:
   ```sh
   HYPRPILOT_DANGEROUS_INPUT_OK=1 hyprpilot-daemon &
   ```

2. **Capability profile.** The `input` group is NOT in the built-in
   `default` profile. To grant it, either use the built-in
   `unrestricted` profile or write a custom one — see
   `docs/profile-with-input.example.toml`:
   ```sh
   hyprpilot-mcp --profile unrestricted
   # or, with a custom profile:
   hyprpilot-mcp --profile my-input-profile
   ```

Both gates must align before an agent can synthesize input.

```sh
# CLI examples (require both gates above)
hyprpilot input type "hello world"
hyprpilot input keys ctrl+shift+t
hyprpilot input shortcut ctrl+t class:firefox
hyprpilot input mouse-move 100 200 --absolute
hyprpilot input mouse-click left
```

`input_shortcut` uses Hyprland's `sendshortcut` dispatcher and is
strictly safer than `input_keys` — it targets a specific window
by selector instead of going wherever focus happens to be.

Backends are detected at daemon startup; if `wtype` or `ydotool` is
missing, the corresponding tools return `input_backend_missing` with
the missing backend name.

### Declarative rules

The daemon subscribes to Hyprland's event socket and matches each
event against rules declared in
`$XDG_CONFIG_HOME/hyprpilot/rules.toml`. Each rule pairs an event kind
with field-equality predicates and a list of actions to fire on match.

```toml
[[rule]]
name = "slack to scratchpad"
on = "openwindow"
when = { class = "Slack" }
do = ["move_to_workspace_silent special:scratch"]
```

Semantics:

- **First match wins.** Rules evaluated top-to-bottom; first whose
  `on` matches and whose `when` predicates all hold fires.
- **Equality predicates only** in v0.4 (no regex, ranges, negation).
- **Reentrance guard.** While the engine is executing actions, incoming
  events skip rule matching, with a ~100 ms tail after actions finish.
  Prevents rule → action → echo → rule loops.
- **Per-action failure isolation.** A failing dispatcher logs a warning
  and the rule's remaining actions still run.

Inspect and validate via CLI or MCP:

```sh
hyprpilot rules path        # where the daemon reads from
hyprpilot rules list        # current on-disk rules
hyprpilot rules validate    # parse + compile, before restarting
```

See `docs/rules.example.toml` for the full action grammar reference.

Editing `rules.toml` while the daemon is running has no effect — the
engine uses the version it loaded at startup. Restart the daemon to
reload.

### Screen capture + OCR (v0.6)

The `hyprpilot-vision` crate shells out to `grim` (wlr-screencopy) for
capture and `tesseract` for OCR. Both binaries are detected at
construction time; missing → typed error.

```sh
# CLI — image bytes go to --output or stdout.
hyprpilot capture full   --monitor eDP-1  --output /tmp/screen.png
hyprpilot capture region 0 0 1920 1080    --output /tmp/region.jpeg --format jpeg

# OCR a region.
hyprpilot ocr region 100 200 800 400 --lang eng --psm 6
```

MCP tools (group: `vision`, opt-in only, NOT in the default profile):

| Tool                  | Result                                                              |
|-----------------------|---------------------------------------------------------------------|
| `screenshot`          | `Content::Image` block (base64 PNG/JPEG); whole compositor          |
| `screenshot_region`   | `Content::Image`; rectangular region                                |
| `screenshot_monitor`  | `Content::Image`; single named monitor                              |
| `ocr_screen`          | `Content::Text`; extracted text from the whole screen               |
| `ocr_region`          | `Content::Text`; extracted text from a region                       |

Vision is excluded from `Profile::default_safe()` because screenshots can
capture private content. To grant it, list `vision` in a custom profile's
`allow` (see `docs/profile.example.toml`) or use `--profile unrestricted`
for trusted local development.

Both backends are graceful about misuse:

- Missing binary → `backend `grim` is not installed (or not on $PATH)`.
- Zero-dimension region → `invalid region: width and height must be > 0`.
- Tesseract finding no text in a blank image → empty string (success), not an error.

## Design

- The compositor's IPC is the source of truth. We do not cache.
- Mutating operations are typed at the API boundary. No stringly-typed
  dispatch in user code.
- The daemon is optional for the CLI; it's required for the MCP server
  (undo state must outlive the MCP host's spawn lifecycle).
- Hyprland version is detected on connect; commands incompatible with
  the live version fail fast with a typed error.

## Milestones

- v0.1 (done): core + daemon + CLI, ~15 dispatchers, in-memory undo.
- v0.2 (done): MCP server, capability profiles, dry-run.
- v0.3 (done): snapshots (capture / list / diff / restore / delete),
  persistent undo across daemon restarts.
- v0.4 (done):
  - **restore completeness**: geometry, fullscreen, pin, active focus
    in `snapshot_restore`.
  - **rules engine**: daemon-side reactions to socket2 events with TOML
    rule files, first-match semantics, reentrance-guarded execution.
- v0.5 (done):
  - **input synthesis**: typing, key chords, mouse via `wtype`,
    `ydotool`, and Hyprland's `sendshortcut`. Gated by env var +
    capability profile.
- v0.6 (in progress):
  - **screen capture**: `grim`-backed PNG / JPEG capture (full,
    per-monitor, region) via new `hyprpilot-vision` crate.
  - **OCR**: `tesseract`-backed text extraction.
  - **MCP `Content::Image`** with base64 PNG/JPEG plus 5 new tools
    in `ToolGroup::Vision` (opt-in only).
- v0.7: composite tools (`click_text`, `find_text_position`); rich
  preview for snapshot-restore that embeds the diff actions; libei
  backend scaffold (feature-flagged stub).
- v0.8: `input keys` routed through ydotool (uinput→libinput) so
  Hyprland global binds fire — `super+T` now triggers its `bind`.
  XDG-aware ydotoold socket discovery. The libei migration was
  **shelved** (`docs/libei-design.md`): Hyprland has no RemoteDesktop
  portal, and the only community bridge falls back to virtual-keyboard,
  which Hyprland filters from its bind matcher — so libei would not
  have achieved the goal. ydotool already does.
