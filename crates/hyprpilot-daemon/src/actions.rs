//! Action grammar for rule `do = [...]` entries.
//!
//! Each action string is `verb [args...]`, where the verb maps to one of
//! the daemon's dispatch methods and the args are parsed into typed forms
//! using the same selector / workspace-ref grammars the CLI and MCP layers
//! use.
//!
//! Supported verbs (mirrors the request surface the daemon already
//! exposes via RPC):
//!
//! | Verb                          | Args                          |
//! |-------------------------------|-------------------------------|
//! | `focus_window`                | selector                      |
//! | `close_window`                | selector                      |
//! | `kill_active`                 | —                             |
//! | `cycle_next` / `cycle_prev`   | —                             |
//! | `switch_workspace`            | workspace-ref                 |
//! | `move_to_workspace`           | workspace-ref                 |
//! | `move_to_workspace_silent`    | workspace-ref                 |
//! | `toggle_floating`             | —                             |
//! | `fullscreen`                  | `0` (maximize) or `1` (full)  |
//! | `center_active`               | —                             |
//! | `pin` / `pin_active`          | —                             |
//! | `move_active`                 | direction: `l\|r\|u\|d`       |
//! | `swap_active`                 | direction: `l\|r\|u\|d`       |
//! | `resize_active`               | `<dx> <dy>` (signed pixels)   |
//! | `exec`                        | rest-of-line shell command    |
//! | `focus_monitor`               | monitor name or direction     |
//! | `move_workspace_to_monitor`   | monitor name                  |
//!
//! Selectors and workspace refs use Hyprland's own grammar (the same
//! shapes accepted by [`crate::DaemonClient`] callers and the MCP server):
//! `active`, `class:Foo`, `pid:1234`, `address:0x…` for windows; `2`,
//! `+1`, `prev`, `next`, `empty`, `name:foo`, `special:term` for
//! workspaces.

use thiserror::Error;

use hyprpilot_core::dispatch::{Direction, FullscreenMode};
use hyprpilot_core::selector::{WindowSelector, WorkspaceRef};
use hyprpilot_core::{Connection, Error as CoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    FocusWindow(WindowSelector),
    CloseWindow(WindowSelector),
    KillActive,
    CycleNext,
    CyclePrev,
    SwitchWorkspace(WorkspaceRef),
    MoveToWorkspace(WorkspaceRef),
    MoveToWorkspaceSilent(WorkspaceRef),
    ToggleFloating,
    SetFullscreen(FullscreenMode),
    CenterActive,
    PinActive,
    MoveActive(Direction),
    ResizeActive(i32, i32),
    SwapActive(Direction),
    Exec(String),
    FocusMonitor(String),
    MoveWorkspaceToMonitor(String),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("action string is empty")]
    Empty,
    #[error("unknown action verb `{0}`")]
    UnknownVerb(String),
    #[error("action `{verb}` requires {expected} arg(s), got {got}")]
    WrongArgCount { verb: String, expected: usize, got: usize },
    #[error("action `{verb}` arg `{arg}` is invalid: {reason}")]
    InvalidArg { verb: String, arg: String, reason: String },
}

impl Action {
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ParseError::Empty);
        }
        let mut iter = trimmed.splitn(2, char::is_whitespace);
        let verb = iter.next().unwrap();
        let rest = iter.next().map(str::trim).unwrap_or("");

        match verb {
            "focus_window" => {
                require_nonempty(verb, "selector", rest)?;
                Ok(Action::FocusWindow(parse_window(verb, rest)?))
            }
            "close_window" => {
                require_nonempty(verb, "selector", rest)?;
                Ok(Action::CloseWindow(parse_window(verb, rest)?))
            }
            "kill_active" => require_empty(verb, rest).map(|_| Action::KillActive),
            "cycle_next" => require_empty(verb, rest).map(|_| Action::CycleNext),
            "cycle_prev" => require_empty(verb, rest).map(|_| Action::CyclePrev),
            "switch_workspace" => {
                require_nonempty(verb, "workspace", rest)?;
                Ok(Action::SwitchWorkspace(parse_ws(verb, rest)?))
            }
            "move_to_workspace" => {
                require_nonempty(verb, "workspace", rest)?;
                Ok(Action::MoveToWorkspace(parse_ws(verb, rest)?))
            }
            "move_to_workspace_silent" => {
                require_nonempty(verb, "workspace", rest)?;
                Ok(Action::MoveToWorkspaceSilent(parse_ws(verb, rest)?))
            }
            "toggle_floating" => require_empty(verb, rest).map(|_| Action::ToggleFloating),
            "fullscreen" => {
                require_nonempty(verb, "mode", rest)?;
                let mode = match rest {
                    "0" | "maximize" => FullscreenMode::Maximize,
                    "1" | "fullscreen" => FullscreenMode::Fullscreen,
                    other => {
                        return Err(ParseError::InvalidArg {
                            verb: verb.to_string(),
                            arg: other.to_string(),
                            reason: "expected `0`/`maximize` or `1`/`fullscreen`".into(),
                        })
                    }
                };
                Ok(Action::SetFullscreen(mode))
            }
            "center_active" => require_empty(verb, rest).map(|_| Action::CenterActive),
            "pin" | "pin_active" => require_empty(verb, rest).map(|_| Action::PinActive),
            "move_active" => Ok(Action::MoveActive(parse_dir(verb, rest)?)),
            "swap_active" => Ok(Action::SwapActive(parse_dir(verb, rest)?)),
            "resize_active" => {
                let mut parts = rest.split_whitespace();
                let dx_s = parts.next().ok_or(ParseError::WrongArgCount {
                    verb: verb.to_string(),
                    expected: 2,
                    got: 0,
                })?;
                let dy_s = parts.next().ok_or(ParseError::WrongArgCount {
                    verb: verb.to_string(),
                    expected: 2,
                    got: 1,
                })?;
                if parts.next().is_some() {
                    return Err(ParseError::WrongArgCount {
                        verb: verb.to_string(),
                        expected: 2,
                        got: 3,
                    });
                }
                let dx = dx_s.parse::<i32>().map_err(|e| ParseError::InvalidArg {
                    verb: verb.to_string(),
                    arg: dx_s.to_string(),
                    reason: format!("dx: {e}"),
                })?;
                let dy = dy_s.parse::<i32>().map_err(|e| ParseError::InvalidArg {
                    verb: verb.to_string(),
                    arg: dy_s.to_string(),
                    reason: format!("dy: {e}"),
                })?;
                Ok(Action::ResizeActive(dx, dy))
            }
            "exec" => {
                require_nonempty(verb, "command", rest)?;
                Ok(Action::Exec(rest.to_string()))
            }
            "focus_monitor" => {
                require_nonempty(verb, "monitor", rest)?;
                Ok(Action::FocusMonitor(rest.to_string()))
            }
            "move_workspace_to_monitor" => {
                require_nonempty(verb, "monitor", rest)?;
                Ok(Action::MoveWorkspaceToMonitor(rest.to_string()))
            }
            other => Err(ParseError::UnknownVerb(other.to_string())),
        }
    }

    /// Execute through a live Hyprland connection. Errors propagate as
    /// [`CoreError`] — the caller decides whether to log + continue or
    /// abort the rule's remaining actions.
    pub async fn execute(&self, conn: &Connection) -> Result<(), CoreError> {
        match self {
            Action::FocusWindow(s) => conn.focus_window(s).await,
            Action::CloseWindow(s) => conn.close_window(s).await,
            Action::KillActive => conn.kill_active().await,
            Action::CycleNext => conn.cycle_next().await,
            Action::CyclePrev => conn.cycle_prev().await,
            Action::SwitchWorkspace(w) => conn.switch_workspace(w).await,
            Action::MoveToWorkspace(w) => conn.move_active_to_workspace(w).await,
            Action::MoveToWorkspaceSilent(w) => conn.move_active_to_workspace_silent(w).await,
            Action::ToggleFloating => conn.toggle_floating().await,
            Action::SetFullscreen(mode) => conn.set_fullscreen(*mode).await,
            Action::CenterActive => conn.center_active().await,
            Action::PinActive => conn.pin_active().await,
            Action::MoveActive(d) => conn.move_active(*d).await,
            Action::ResizeActive(dx, dy) => conn.resize_active(*dx, *dy).await,
            Action::SwapActive(d) => conn.swap_active(*d).await,
            Action::Exec(cmd) => conn.exec(cmd).await,
            Action::FocusMonitor(m) => conn.focus_monitor(m).await,
            Action::MoveWorkspaceToMonitor(m) => conn.move_workspace_to_monitor(m).await,
        }
    }
}

// ---- helpers ---------------------------------------------------------------

fn require_empty(verb: &str, rest: &str) -> Result<(), ParseError> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(ParseError::WrongArgCount {
            verb: verb.to_string(),
            expected: 0,
            got: rest.split_whitespace().count(),
        })
    }
}

fn require_nonempty(verb: &str, arg_name: &str, rest: &str) -> Result<(), ParseError> {
    if rest.is_empty() {
        Err(ParseError::InvalidArg {
            verb: verb.to_string(),
            arg: arg_name.to_string(),
            reason: "missing required argument".into(),
        })
    } else {
        Ok(())
    }
}

fn parse_window(verb: &str, s: &str) -> Result<WindowSelector, ParseError> {
    WindowSelector::parse(s).map_err(|reason| ParseError::InvalidArg {
        verb: verb.to_string(),
        arg: s.to_string(),
        reason,
    })
}

fn parse_ws(verb: &str, s: &str) -> Result<WorkspaceRef, ParseError> {
    WorkspaceRef::parse(s).map_err(|reason| ParseError::InvalidArg {
        verb: verb.to_string(),
        arg: s.to_string(),
        reason,
    })
}

fn parse_dir(verb: &str, s: &str) -> Result<Direction, ParseError> {
    match s {
        "l" | "left" => Ok(Direction::Left),
        "r" | "right" => Ok(Direction::Right),
        "u" | "up" => Ok(Direction::Up),
        "d" | "down" => Ok(Direction::Down),
        "" => Err(ParseError::InvalidArg {
            verb: verb.to_string(),
            arg: String::new(),
            reason: "missing direction (l/r/u/d)".into(),
        }),
        other => Err(ParseError::InvalidArg {
            verb: verb.to_string(),
            arg: other.to_string(),
            reason: "expected one of l/r/u/d".into(),
        }),
    }
}

/// Parse a list of action strings. On the first error, returns the index
/// and the parse error — the rest are not parsed. Used by rule-validation
/// paths that want to fail fast on a bad config rather than discover the
/// error at firing time.
pub fn parse_all(actions: &[String]) -> Result<Vec<Action>, (usize, ParseError)> {
    actions
        .iter()
        .enumerate()
        .map(|(i, s)| Action::parse(s).map_err(|e| (i, e)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_no_arg_verbs() {
        assert_eq!(Action::parse("kill_active").unwrap(), Action::KillActive);
        assert_eq!(Action::parse("cycle_next").unwrap(), Action::CycleNext);
        assert_eq!(Action::parse("toggle_floating").unwrap(), Action::ToggleFloating);
        assert_eq!(Action::parse("pin").unwrap(), Action::PinActive);
        assert_eq!(Action::parse("pin_active").unwrap(), Action::PinActive);
    }

    #[test]
    fn parses_selector_verbs() {
        let a = Action::parse("focus_window class:Slack").unwrap();
        assert_eq!(a, Action::FocusWindow(WindowSelector::Class("Slack".into())));
        let a = Action::parse("close_window pid:1234").unwrap();
        assert_eq!(a, Action::CloseWindow(WindowSelector::Pid(1234)));
    }

    #[test]
    fn parses_workspace_verbs() {
        let a = Action::parse("move_to_workspace_silent special:scratch").unwrap();
        assert_eq!(
            a,
            Action::MoveToWorkspaceSilent(WorkspaceRef::Special("scratch".into()))
        );
        let a = Action::parse("switch_workspace +1").unwrap();
        assert_eq!(a, Action::SwitchWorkspace(WorkspaceRef::Relative(1)));
    }

    #[test]
    fn parses_fullscreen_mode_aliases() {
        assert_eq!(
            Action::parse("fullscreen 0").unwrap(),
            Action::SetFullscreen(FullscreenMode::Maximize)
        );
        assert_eq!(
            Action::parse("fullscreen maximize").unwrap(),
            Action::SetFullscreen(FullscreenMode::Maximize)
        );
        assert_eq!(
            Action::parse("fullscreen 1").unwrap(),
            Action::SetFullscreen(FullscreenMode::Fullscreen)
        );
        assert_eq!(
            Action::parse("fullscreen fullscreen").unwrap(),
            Action::SetFullscreen(FullscreenMode::Fullscreen)
        );
        assert!(matches!(
            Action::parse("fullscreen banana"),
            Err(ParseError::InvalidArg { .. })
        ));
    }

    #[test]
    fn parses_direction_verbs() {
        assert_eq!(
            Action::parse("move_active l").unwrap(),
            Action::MoveActive(Direction::Left)
        );
        assert_eq!(
            Action::parse("swap_active down").unwrap(),
            Action::SwapActive(Direction::Down)
        );
        assert!(matches!(
            Action::parse("move_active sideways"),
            Err(ParseError::InvalidArg { .. })
        ));
    }

    #[test]
    fn parses_resize() {
        assert_eq!(
            Action::parse("resize_active 100 -50").unwrap(),
            Action::ResizeActive(100, -50)
        );
        assert!(matches!(
            Action::parse("resize_active 100"),
            Err(ParseError::WrongArgCount { expected: 2, got: 1, .. })
        ));
        assert!(matches!(
            Action::parse("resize_active a b"),
            Err(ParseError::InvalidArg { .. })
        ));
    }

    #[test]
    fn parses_exec_with_trailing_command() {
        let a = Action::parse("exec firefox --new-window https://example.com").unwrap();
        assert_eq!(
            a,
            Action::Exec("firefox --new-window https://example.com".to_string())
        );
    }

    #[test]
    fn unknown_verb_rejected() {
        assert_eq!(
            Action::parse("teleport").unwrap_err(),
            ParseError::UnknownVerb("teleport".to_string())
        );
    }

    #[test]
    fn empty_string_rejected() {
        assert_eq!(Action::parse("").unwrap_err(), ParseError::Empty);
        assert_eq!(Action::parse("   ").unwrap_err(), ParseError::Empty);
    }

    #[test]
    fn no_arg_verb_with_extra_args_rejected() {
        assert!(matches!(
            Action::parse("kill_active oops"),
            Err(ParseError::WrongArgCount { expected: 0, got: 1, .. })
        ));
    }

    #[test]
    fn missing_required_arg_rejected() {
        assert!(matches!(
            Action::parse("focus_window"),
            Err(ParseError::InvalidArg { .. })
        ));
    }

    #[test]
    fn parse_all_returns_index_on_first_error() {
        let v = vec![
            "kill_active".to_string(),
            "bogus_verb".to_string(),
            "cycle_next".to_string(),
        ];
        let err = parse_all(&v).unwrap_err();
        assert_eq!(err.0, 1);
        assert!(matches!(err.1, ParseError::UnknownVerb(_)));
    }
}
