# HyprPilot

Programmatic control of Hyprland for AI agents and humans. Typed Rust IPC
client, long-running daemon with undo and snapshots, CLI, and an MCP server.

> Status: v0.3 in development. v0.2 shipped MCP, capability profiles, and
> dry-run. v0.3 adds named snapshots (capture / list / diff / restore /
> delete) and persistent undo across daemon restarts. Rules engine, input
> synthesis, and screen capture remain future milestones.

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
- v0.4 (in progress):
  - **restore completeness**: geometry, fullscreen, pin, active focus —
    in restore.
  - **rules engine**: daemon-side reactions to socket2 events with TOML
    rule files (separate PR).
- v0.5: input synthesis (wtype / ydotool / libei).
- v0.6: screen capture + OCR.
