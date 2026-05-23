# HyprPilot

Programmatic control of Hyprland for AI agents and humans. Typed Rust IPC
client, long-running daemon with undo, CLI, and an MCP server.

> Status: v0.2 in development. v0.1 surface (~15 dispatchers, read-only
> queries, in-memory undo for kill/move) is shipped. v0.2 adds an MCP
> (Model Context Protocol) server with capability profiles and dry-run
> default for mutating tools. Rules engine, input synthesis, and screen
> capture remain future milestones.

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

## Design

- The compositor's IPC is the source of truth. We do not cache.
- Mutating operations are typed at the API boundary. No stringly-typed
  dispatch in user code.
- The daemon is optional for the CLI; it's required for the MCP server
  (undo state must outlive the MCP host's spawn lifecycle).
- Hyprland version is detected on connect; commands incompatible with
  the live version fail fast with a typed error.

## Milestones

- v0.1 (done): core + daemon + CLI, ~15 dispatchers, undo for kill/move.
- v0.2 (in progress): MCP server, capability profiles, dry-run.
- v0.3: snapshot/restore, event-driven rules engine.
- v0.4: input synthesis (wtype/ydotool/libei).
- v0.5: screen capture + OCR.
