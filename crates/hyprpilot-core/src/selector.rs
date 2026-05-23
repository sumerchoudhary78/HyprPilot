//! Typed selectors that compile to Hyprland's dispatcher argument grammar.
//!
//! These are intentionally enums rather than free strings: the wire format is
//! sensitive to prefixes (`address:`, `class:`, `pid:`) and getting it wrong
//! silently mis-targets a different window. Caller code constructs the variant
//! once; we encode it.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Identifies a window for dispatchers like `focuswindow`, `closewindow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum WindowSelector {
    /// Hex address from a [`crate::types::Client`].
    Address(String),
    /// Process ID.
    Pid(i32),
    /// Window class. Hyprland treats this as a regex.
    Class(String),
    /// Window title. Hyprland treats this as a regex.
    Title(String),
    /// Tag name.
    Tag(String),
    /// The currently focused window. Most dispatchers have a dedicated form
    /// (e.g. `killactive`) — prefer those when possible.
    Active,
}

impl WindowSelector {
    /// Encode in Hyprland dispatcher argument form.
    pub fn encode(&self) -> String {
        match self {
            WindowSelector::Address(a) => {
                if a.starts_with("0x") { format!("address:{a}") } else { format!("address:0x{a}") }
            }
            WindowSelector::Pid(p) => format!("pid:{p}"),
            WindowSelector::Class(c) => format!("class:{c}"),
            WindowSelector::Title(t) => format!("title:{t}"),
            WindowSelector::Tag(t) => format!("tag:{t}"),
            WindowSelector::Active => "activewindow".into(),
        }
    }
}

impl fmt::Display for WindowSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

/// Identifies a workspace for `workspace`, `movetoworkspace`, etc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum WorkspaceRef {
    /// Absolute numeric id, e.g. `2`.
    Id(i32),
    /// Named workspace, encoded as `name:<n>`.
    Name(String),
    /// Relative move: `+1`, `-2`.
    Relative(i32),
    /// `prev` / `next` (open workspaces only).
    Previous,
    Next,
    /// `empty` — first empty workspace.
    Empty,
    /// `special:<name>` — scratchpad workspace.
    Special(String),
}

impl WorkspaceRef {
    pub fn encode(&self) -> String {
        match self {
            WorkspaceRef::Id(n) => n.to_string(),
            WorkspaceRef::Name(n) => format!("name:{n}"),
            WorkspaceRef::Relative(n) => {
                if *n >= 0 { format!("+{n}") } else { n.to_string() }
            }
            WorkspaceRef::Previous => "prev".into(),
            WorkspaceRef::Next => "next".into(),
            WorkspaceRef::Empty => "empty".into(),
            WorkspaceRef::Special(n) => {
                if n.is_empty() { "special".into() } else { format!("special:{n}") }
            }
        }
    }
}

impl fmt::Display for WorkspaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_selector_encoding() {
        assert_eq!(WindowSelector::Address("0xdead".into()).encode(), "address:0xdead");
        assert_eq!(WindowSelector::Address("dead".into()).encode(), "address:0xdead");
        assert_eq!(WindowSelector::Pid(1234).encode(), "pid:1234");
        assert_eq!(WindowSelector::Class("kitty".into()).encode(), "class:kitty");
        assert_eq!(WindowSelector::Active.encode(), "activewindow");
    }

    #[test]
    fn workspace_ref_encoding() {
        assert_eq!(WorkspaceRef::Id(2).encode(), "2");
        assert_eq!(WorkspaceRef::Relative(1).encode(), "+1");
        assert_eq!(WorkspaceRef::Relative(-1).encode(), "-1");
        assert_eq!(WorkspaceRef::Empty.encode(), "empty");
        assert_eq!(WorkspaceRef::Special("term".into()).encode(), "special:term");
        assert_eq!(WorkspaceRef::Special(String::new()).encode(), "special");
    }
}
