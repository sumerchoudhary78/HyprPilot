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

    /// Linux input-event-code for the left variant of this modifier, used
    /// by the ydotool backend. From `<linux/input-event-codes.h>`:
    /// `KEY_LEFTCTRL=29`, `KEY_LEFTSHIFT=42`, `KEY_LEFTALT=56`,
    /// `KEY_LEFTMETA=125`.
    pub fn ydotool_keycode(self) -> u16 {
        match self {
            Modifier::Ctrl => 29,
            Modifier::Shift => 42,
            Modifier::Alt => 56,
            Modifier::Super => 125,
        }
    }
}

/// Map an xkb keysym name (as accepted by wtype's `-k`) to its Linux
/// input-event-code for the ydotool backend. Returns `None` for keysyms
/// we don't have a code for — callers fall back to the wtype (keysym-name)
/// path in that case.
///
/// Single ASCII letters are case-insensitive (`t` and `T` both → `KEY_T`);
/// the actual upper/lower distinction is the Shift modifier's job, same as
/// a real keyboard. Codes are from `<linux/input-event-codes.h>`.
pub fn keysym_to_evdev(key: &str) -> Option<u16> {
    // Single ASCII letter → KEY_A..KEY_Z (evdev's qwerty layout order).
    if key.len() == 1 {
        let c = key.as_bytes()[0].to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            return Some(match c {
                b'a' => 30, b'b' => 48, b'c' => 46, b'd' => 32, b'e' => 18,
                b'f' => 33, b'g' => 34, b'h' => 35, b'i' => 23, b'j' => 36,
                b'k' => 37, b'l' => 38, b'm' => 50, b'n' => 49, b'o' => 24,
                b'p' => 25, b'q' => 16, b'r' => 19, b's' => 31, b't' => 20,
                b'u' => 22, b'v' => 47, b'w' => 17, b'x' => 45, b'y' => 21,
                b'z' => 44,
                _ => unreachable!("guarded by is_ascii_lowercase"),
            });
        }
        if c.is_ascii_digit() {
            // KEY_1..KEY_9 = 2..10, KEY_0 = 11.
            return Some(if c == b'0' { 11 } else { (c - b'1') as u16 + 2 });
        }
    }
    // Named keys. Accept the wtype/xkb spelling plus common aliases,
    // case-insensitively.
    Some(match key.to_ascii_lowercase().as_str() {
        "return" | "enter" | "kp_enter" => 28,
        "space" => 57,
        "tab" => 15,
        "escape" | "esc" => 1,
        "backspace" => 14,
        "delete" | "del" => 111,
        "insert" | "ins" => 110,
        "home" => 102,
        "end" => 107,
        "page_up" | "pageup" | "prior" => 104,
        "page_down" | "pagedown" | "next" => 109,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "minus" => 12,
        "equal" => 13,
        "comma" => 51,
        "period" | "dot" => 52,
        "slash" => 53,
        "semicolon" => 39,
        "apostrophe" => 40,
        "grave" => 41,
        "bracketleft" => 26,
        "bracketright" => 27,
        "backslash" => 43,
        "f1" => 59, "f2" => 60, "f3" => 61, "f4" => 62, "f5" => 63,
        "f6" => 64, "f7" => 65, "f8" => 66, "f9" => 67, "f10" => 68,
        "f11" => 87, "f12" => 88,
        _ => return None,
    })
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

    /// Encode for `ydotool key`: a sequence of `<keycode>:<state>` tokens
    /// where state `1` is press and `0` is release. Modifiers are pressed
    /// first (BTreeSet order) then released in reverse, bracketing the key:
    ///
    /// ```text
    /// ctrl+shift+t  ->  29:1 42:1 20:1 20:0 42:0 29:0
    /// ```
    ///
    /// Unlike [`Self::encode_wtype`], events go through uinput→libinput, so
    /// Hyprland's global bind matcher sees the chord (e.g. `super+T` fires a
    /// `bind`). wtype's virtual-keyboard events are filtered out of the bind
    /// matcher and only reach the focused client.
    ///
    /// Returns [`InputError::InvalidCombo`] if the key has no known
    /// input-event-code; the runner falls back to the wtype path then.
    pub fn encode_ydotool_key(&self) -> Result<Vec<String>> {
        let key_code = keysym_to_evdev(&self.key).ok_or_else(|| {
            InputError::InvalidCombo(format!(
                "key `{}` has no known Linux input-event code for the ydotool backend",
                self.key
            ))
        })?;
        let mods: Vec<u16> = self.modifiers.iter().map(|m| m.ydotool_keycode()).collect();
        let mut argv = Vec::with_capacity(mods.len() * 2 + 2);
        for m in &mods {
            argv.push(format!("{m}:1"));
        }
        argv.push(format!("{key_code}:1"));
        argv.push(format!("{key_code}:0"));
        for m in mods.iter().rev() {
            argv.push(format!("{m}:0"));
        }
        Ok(argv)
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
    fn encodes_ydotool_brackets_modifiers_around_key() {
        // ctrl+shift+t: mods down in BTreeSet order, key down/up, mods up reversed.
        let c = KeyCombo::parse("ctrl+shift+t").unwrap();
        assert_eq!(
            c.encode_ydotool_key().unwrap(),
            vec!["29:1", "42:1", "20:1", "20:0", "42:0", "29:0"]
        );
    }

    #[test]
    fn encodes_ydotool_super_t() {
        // The headline case: super+T must produce KEY_LEFTMETA(125) + KEY_T(20).
        let c = KeyCombo::parse("super+t").unwrap();
        assert_eq!(c.encode_ydotool_key().unwrap(), vec!["125:1", "20:1", "20:0", "125:0"]);
    }

    #[test]
    fn encodes_ydotool_lone_key() {
        let c = KeyCombo::parse("Return").unwrap();
        assert_eq!(c.encode_ydotool_key().unwrap(), vec!["28:1", "28:0"]);
    }

    #[test]
    fn ydotool_key_is_case_insensitive_for_letters() {
        assert_eq!(keysym_to_evdev("t"), keysym_to_evdev("T"));
        assert_eq!(keysym_to_evdev("t"), Some(20));
    }

    #[test]
    fn ydotool_digits_and_named_keys() {
        assert_eq!(keysym_to_evdev("1"), Some(2));
        assert_eq!(keysym_to_evdev("0"), Some(11));
        assert_eq!(keysym_to_evdev("space"), Some(57));
        assert_eq!(keysym_to_evdev("Escape"), Some(1));
        assert_eq!(keysym_to_evdev("esc"), Some(1));
        assert_eq!(keysym_to_evdev("F12"), Some(88));
        assert_eq!(keysym_to_evdev("Page_Up"), Some(104));
    }

    #[test]
    fn ydotool_unknown_keysym_is_none() {
        // Falls back to wtype in the runner.
        assert_eq!(keysym_to_evdev("Hyper_L"), None);
        assert_eq!(keysym_to_evdev("XF86AudioPlay"), None);
    }

    #[test]
    fn encode_ydotool_errors_on_unmappable_key() {
        let c = KeyCombo::parse("ctrl+XF86AudioMute").unwrap();
        assert!(matches!(c.encode_ydotool_key(), Err(InputError::InvalidCombo(_))));
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
