//! Input synthesis for HyprPilot — typing, key chords, mouse.
//!
//! Built on top of three external tools:
//!
//! - **wtype** for free-form text typing and key combos in the focused
//!   window. Wayland-native; ubiquitous on Hyprland setups.
//! - **ydotool** for mouse moves and clicks (and a kernel-level keyboard
//!   path we deliberately do not use here). Requires `ydotoold` running.
//! - **Hyprland's `sendshortcut` dispatcher** for sending a key chord to
//!   a *specific* window by selector — the one input primitive that does
//!   not need an external tool and can target non-focused windows.
//!
//! ## Detection
//!
//! [`BackendAvailability::detect`] runs at daemon startup, probes the
//! `$PATH` for `wtype` and `ydotool`, and reports which backends are
//! present. `sendshortcut` is always available (it ships with Hyprland).
//! Missing backends turn the corresponding `InputRunner` methods into
//! typed errors ([`InputError::BackendMissing`]) rather than runtime
//! panics — the daemon surfaces this so the caller (CLI or MCP) can show
//! a clear "install wtype" message.
//!
//! ## Safety
//!
//! Input synthesis is the most dangerous tool surface in HyprPilot: an
//! agent that can type can run arbitrary shell commands in any focused
//! terminal. The daemon gates the whole subsystem behind a startup env
//! var (`HYPRPILOT_DANGEROUS_INPUT_OK=1`); the MCP layer further keeps
//! the `Input` tool group out of the default capability profile. This
//! crate itself does **not** enforce either gate — it's a thin
//! capability layer. Enforcement lives at the daemon RPC boundary.
//!
//! ## Shell-injection safety
//!
//! All external-tool invocations build their argv as a `Vec<String>` and
//! pass it to `tokio::process::Command::arg`/`args` — never to a shell.
//! User-supplied text reaches `wtype` as a single positional argument,
//! so no quoting / escaping is required and no metacharacters can break
//! out into command position.

pub mod detect;
pub mod error;
pub mod keys;
#[cfg(feature = "libei")]
pub mod libei;
pub mod runner;
pub mod virtual_pointer;

pub use detect::BackendAvailability;
pub use error::InputError;
pub use keys::{KeyCombo, Modifier, ModifierSet, MouseButton};
pub use runner::InputRunner;
pub use virtual_pointer::VirtualPointer;
