# HyprPilot

Programmatic control of Hyprland for AI agents and humans. Typed Rust IPC client,
optional long-running daemon with undo support, and a CLI.

> Status: v0.1 scaffold. Read-only queries, ~15 dispatchers, in-memory undo for
> kill/move. MCP server, rules engine, input synthesis, and screen capture are
> planned for later milestones.

## Crates

- `hyprpilot-core` — async Hyprland IPC client (control socket + event socket),
  typed queries and dispatchers, error types. No daemon awareness.
- `hyprpilot-daemon` — long-running process exposing JSON-RPC over a Unix socket.
  Records reversible operations in an undo stack.
- `hyprpilot-cli` — `hyprpilot` binary. Subcommands for queries, window/workspace
  ops, daemon control, and undo.

## Quick start

```sh
cargo build --release

# Direct queries (no daemon needed)
./target/release/hyprpilot query active-window
./target/release/hyprpilot query clients --json

# Window ops (direct to Hyprland)
./target/release/hyprpilot win cycle
./target/release/hyprpilot ws switch 2

# Daemon-backed undo
./target/release/hyprpilot daemon start &
./target/release/hyprpilot --use-daemon win kill
./target/release/hyprpilot undo
```

## Design

See `docs/` (to be written) for the architecture document. Short version:

- The compositor's IPC is the source of truth. We do not cache.
- Mutating operations are typed at the API boundary. No stringly-typed dispatch
  in user code.
- The daemon is optional. Without it, the CLI is a thin async client over the
  socket. With it, mutating operations are recorded and reversible.
- Hyprland version is detected on connect; commands incompatible with the live
  version fail fast with a typed error.

## Non-goals (v0.1)

- MCP server (v0.2)
- Capability profiles / dry-run (v0.2)
- Event-driven automation rules (v0.3)
- Input synthesis (wtype/ydotool/libei) — v0.4
- Screen capture + OCR — v0.5
