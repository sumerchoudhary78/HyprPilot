//! Typed key combos and mouse buttons + parsers.
//!
//! `KeyCombo` parses strings like `"ctrl+shift+t"` or `"super+space"`
//! into a structured representation, and re-emits in the two formats the
//! backends accept:
//!
//! - **wtype**: `wtype -M ctrl -M shift -k t` (modifiers then key)
//! - **Hyprland `sendshortcut`**: `MODS,KEY,WINDOW` where `MODS` is
//!   space-separated all-caps modifier names (`CTRL`, `SHIFT`, etc.) and
//!   `KEY` is the keysym (`t`, `Return`, `F4`).
//!
//! ## Modifier surface
//!
//! Four modifiers are recognised: `ctrl`, `shift`, `alt`, `super` (the
//! Windows / Meta / Mod4 key). Common aliases accepted on input
//! (`control`, `meta`, `mod4`, `cmd`) but normalised on output.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{InputError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Ctrl,
    Shift,
    Alt,
    Super,
}

impl Modifier {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => Some(Modifier::Ctrl),
            "shift" => Some(Modifier::Shift),
            "alt" | "meta" => Some(Modifier::Alt),
            "super" | "win" | "windows" | "mod4" | "cmd" | "command" => Some(Modifier::Super),
            _ => None,
        }
    }

    /// wtype's modifier name on the command line.
    pub fn wtype_name(self) -> &'static str {
        match self {
            Modifier::Ctrl => "ctrl",
            Modifier::Shift => "shift",
            Modifier::Alt => "alt",
            Modifier::Super => "logo",
        }
    }

    /// Hyprland `sendshortcut` modifier name (ALL CAPS).
    pub fn hyprctl_name(self) -> &'static str {
        match self {
            Modifier::Ctrl => "CTRL",
            Modifier::Shift => "SHIFT",
            Modifier::Alt => "ALT",
            Modifier::Super => "SUPER",
        }
    }
}

/// An ordered set of modifiers. `BTreeSet` gives deterministic encoding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifierSet(pub BTreeSet<Modifier>);

impl ModifierSet {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Modifier> {
        self.0.iter()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCombo {
    pub modifiers: ModifierSet,
    /// The keysym — passed verbatim to wtype (`-k`) and `sendshortcut`.
    /// Examples: `t`, `Return`, `F4`, `Escape`, `space`.
    pub key: String,
}

impl KeyCombo {
    /// Parse `"ctrl+shift+t"` into a structured combo.
    ///
    /// - Tokens are split on `+`.
    /// - Whitespace around tokens is trimmed.
    /// - The last token is the key; everything before must be a modifier.
    /// - The key must be non-empty and contain no `+`.
    /// - Duplicate modifiers collapse silently.
    pub fn parse(s: &str) -> Result<Self> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err(InputError::InvalidCombo("empty combo".into()));
        }
        let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(InputError::InvalidCombo(format!(
                "`{trimmed}` has an empty token (consecutive or leading/trailing `+`)"
            )));
        }
        // Last token = key, prior = modifiers.
        let (key, mods) = parts.split_last().expect("parts non-empty");
        let mut modifiers = BTreeSet::new();
        for m in mods {
            let parsed = Modifier::parse(m).ok_or_else(|| {
                InputError::InvalidCombo(format!(
                    "`{m}` in `{trimmed}` is not a recognised modifier; \
                     expected ctrl, shift, alt, or super"
                ))
            })?;
            modifiers.insert(parsed);
        }
        Ok(KeyCombo {
            modifiers: ModifierSet(modifiers),
            key: (*key).to_string(),
        })
    }

    /// Encode for wtype: emits a flat argv ending with `-k <key>`. Each
    /// modifier becomes `-M <name>`.
    ///
    /// `wtype -M ctrl -M shift -k t` presses + releases the chord and
    /// returns. (wtype `-M` modifiers persist across keys in a single
    /// invocation; we emit them once and a single `-k` at the end.)
    pub fn encode_wtype(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.modifiers.0.len() * 2 + 2);
        for m in self.modifiers.iter() {
            argv.push("-M".into());
            argv.push(m.wtype_name().into());
        }
        argv.push("-k".into());
        argv.push(self.key.clone());
        argv
    }

    /// Encode the MODS field for Hyprland's `sendshortcut` dispatcher.
    /// Space-separated all-caps modifier names. Empty string if there
    /// are no modifiers (the dispatcher accepts that).
    pub fn encode_hyprctl_mods(&self) -> String {
        let names: Vec<&str> = self.modifiers.iter().map(|m| m.hyprctl_name()).collect();
        names.join(" ")
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in self.modifiers.iter() {
            f.write_str(m.wtype_name())?;
            f.write_str("+")?;
        }
        f.write_str(&self.key)
    }
}

// ---- Mouse buttons --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

impl MouseButton {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "left" | "l" | "1" => Ok(MouseButton::Left),
            "right" | "r" | "2" => Ok(MouseButton::Right),
            "middle" | "m" | "3" => Ok(MouseButton::Middle),
            "x1" | "back" | "4" => Ok(MouseButton::X1),
            "x2" | "forward" | "5" => Ok(MouseButton::X2),
            other => Err(InputError::InvalidButton(other.to_string())),
        }
    }

    /// ydotool's `click` button code. ydotool uses hex bitmasks: 0xC0 +
    /// (down|up). For a "press and release" of left, the canonical code
    /// is `0xC0` (down + up combined). For just the button code we use
    /// the raw button number per ydotool's documentation:
    /// - 0x40 / button 1 = left
    /// - 0x41 / button 2 = right
    /// - 0x42 / button 3 = middle
    /// - 0x43 = X1
    /// - 0x44 = X2
    ///
    /// Combined with 0x80 (down+up) → 0xC0..=0xC4.
    pub fn ydotool_click_code(self) -> &'static str {
        match self {
            MouseButton::Left => "0xC0",
            MouseButton::Right => "0xC1",
            MouseButton::Middle => "0xC2",
            MouseButton::X1 => "0xC3",
            MouseButton::X2 => "0xC4",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_modifier_combo() {
        let c = KeyCombo::parse("ctrl+t").unwrap();
        assert_eq!(c.key, "t");
        assert_eq!(c.modifiers.0.len(), 1);
        assert!(c.modifiers.0.contains(&Modifier::Ctrl));
    }

    #[test]
    fn parses_multi_modifier_combo() {
        let c = KeyCombo::parse("ctrl+shift+t").unwrap();
        assert_eq!(c.key, "t");
        assert!(c.modifiers.0.contains(&Modifier::Ctrl));
        assert!(c.modifiers.0.contains(&Modifier::Shift));
    }

    #[test]
    fn parses_aliases() {
        let c = KeyCombo::parse("control+meta+t").unwrap();
        assert!(c.modifiers.0.contains(&Modifier::Ctrl));
        assert!(c.modifiers.0.contains(&Modifier::Alt));

        let c = KeyCombo::parse("super+space").unwrap();
        assert!(c.modifiers.0.contains(&Modifier::Super));
        assert_eq!(c.key, "space");

        let c = KeyCombo::parse("CMD+q").unwrap();
        assert!(c.modifiers.0.contains(&Modifier::Super));
    }

    #[test]
    fn parses_lone_key() {
        // No modifiers — just the key.
        let c = KeyCombo::parse("Escape").unwrap();
        assert!(c.modifiers.0.is_empty());
        assert_eq!(c.key, "Escape");
    }

    #[test]
    fn deduplicates_modifiers() {
        let c = KeyCombo::parse("ctrl+ctrl+t").unwrap();
        assert_eq!(c.modifiers.0.len(), 1);
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(KeyCombo::parse(""), Err(InputError::InvalidCombo(_))));
        assert!(matches!(KeyCombo::parse("   "), Err(InputError::InvalidCombo(_))));
    }

    #[test]
    fn rejects_double_plus_or_trailing_plus() {
        assert!(matches!(KeyCombo::parse("ctrl++t"), Err(InputError::InvalidCombo(_))));
        assert!(matches!(KeyCombo::parse("ctrl+"), Err(InputError::InvalidCombo(_))));
        assert!(matches!(KeyCombo::parse("+t"), Err(InputError::InvalidCombo(_))));
    }

    #[test]
    fn rejects_unknown_modifier() {
        assert!(matches!(
            KeyCombo::parse("hyper+t"),
            Err(InputError::InvalidCombo(_))
        ));
    }

    #[test]
    fn encodes_wtype_argv_in_order() {
        let c = KeyCombo::parse("ctrl+shift+t").unwrap();
        let argv = c.encode_wtype();
        // Modifiers come in BTreeSet order: Ctrl < Shift < Alt < Super
        // — derived Ord on the enum, so order is by variant declaration:
        // Ctrl, Shift, Alt, Super.
        assert_eq!(
            argv,
            vec!["-M", "ctrl", "-M", "shift", "-k", "t"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn encodes_hyprctl_mods() {
        let c = KeyCombo::parse("ctrl+shift+t").unwrap();
        assert_eq!(c.encode_hyprctl_mods(), "CTRL SHIFT");

        let c = KeyCombo::parse("t").unwrap();
        assert_eq!(c.encode_hyprctl_mods(), "");
    }

    #[test]
    fn display_roundtrips_through_parse() {
        let c = KeyCombo::parse("ctrl+shift+t").unwrap();
        let s = c.to_string();
        let c2 = KeyCombo::parse(&s).unwrap();
        assert_eq!(c, c2);
    }

    #[test]
    fn parses_mouse_buttons() {
        assert_eq!(MouseButton::parse("left").unwrap(), MouseButton::Left);
        assert_eq!(MouseButton::parse("R").unwrap(), MouseButton::Right);
        assert_eq!(MouseButton::parse("3").unwrap(), MouseButton::Middle);
        assert_eq!(MouseButton::parse("back").unwrap(), MouseButton::X1);
        assert!(MouseButton::parse("nonsense").is_err());
    }
}
