//! Strongly typed views of Hyprland's JSON outputs.
//!
//! Field naming follows Hyprland's JSON (camelCase / lowercase) via serde
//! aliases. Unknown fields are tolerated so future Hyprland additions don't
//! break deserialisation.

use serde::{Deserialize, Serialize};

/// One keybind, as reported by Hyprland's `binds` query.
///
/// `modmask` is Hyprland's raw modifier bitmask; [`Bind::decode_mods`]
/// turns it into the readable `mods` list (`["SUPER", "SHIFT"]`). `mods`
/// is not part of Hyprland's JSON — [`crate::ipc::Connection::binds`]
/// populates it after the query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bind {
    /// Readable modifier names, derived from `modmask`. Empty until the
    /// query layer fills it in.
    #[serde(default)]
    pub mods: Vec<String>,
    pub modmask: u32,
    /// Keysym, e.g. `T`, `Return`, `space`. Empty for keycode-only binds.
    #[serde(default)]
    pub key: String,
    /// Raw evdev keycode, set instead of `key` for `bindcode`-style binds.
    #[serde(default)]
    pub keycode: i64,
    /// Submap this bind belongs to; empty string is the global submap.
    #[serde(default)]
    pub submap: String,
    pub dispatcher: String,
    #[serde(default)]
    pub arg: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub mouse: bool,
    #[serde(default)]
    pub release: bool,
    #[serde(default)]
    pub repeat: bool,
}

impl Bind {
    /// Decode Hyprland's `modmask` into readable modifier names. Only the
    /// modifiers that matter for chords are surfaced (`SHIFT`, `CTRL`,
    /// `ALT`, `SUPER`); lock bits (Caps, NumLock) are ignored. Bit values
    /// follow X11 / Hyprland: SHIFT=1, CTRL=4, ALT=8, SUPER=64.
    pub fn decode_mods(modmask: u32) -> Vec<String> {
        let mut mods = Vec::new();
        if modmask & 1 != 0 {
            mods.push("SHIFT".to_string());
        }
        if modmask & 4 != 0 {
            mods.push("CTRL".to_string());
        }
        if modmask & 8 != 0 {
            mods.push("ALT".to_string());
        }
        if modmask & 64 != 0 {
            mods.push("SUPER".to_string());
        }
        mods
    }
}

#[cfg(test)]
mod bind_tests {
    use super::Bind;

    #[test]
    fn decode_mods_super_only() {
        assert_eq!(Bind::decode_mods(64), vec!["SUPER"]);
    }

    #[test]
    fn decode_mods_combinations() {
        // 72 = SUPER(64) + ALT(8)
        assert_eq!(Bind::decode_mods(72), vec!["ALT", "SUPER"]);
        // 69 = SUPER(64) + CTRL(4) + SHIFT(1)
        assert_eq!(Bind::decode_mods(69), vec!["SHIFT", "CTRL", "SUPER"]);
        // 5 = CTRL(4) + SHIFT(1)
        assert_eq!(Bind::decode_mods(5), vec!["SHIFT", "CTRL"]);
    }

    #[test]
    fn decode_mods_none() {
        assert!(Bind::decode_mods(0).is_empty());
    }

    #[test]
    fn decode_mods_ignores_lock_bits() {
        // CAPS(2) and NumLock/MOD2(16) are not chord modifiers.
        assert!(Bind::decode_mods(2).is_empty());
        assert_eq!(Bind::decode_mods(2 | 64), vec!["SUPER"]);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub branch: String,
    pub commit: String,
    pub version: String,
    #[serde(default)]
    pub dirty: bool,
    #[serde(default)]
    pub commit_message: String,
    #[serde(default)]
    pub commit_date: String,
    #[serde(default)]
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientWorkspace {
    pub id: i32,
    pub name: String,
}

/// A managed window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    /// Hex pointer used as the canonical handle in dispatchers.
    pub address: String,
    pub mapped: bool,
    #[serde(default)]
    pub hidden: bool,
    pub at: [i32; 2],
    pub size: [i32; 2],
    pub workspace: ClientWorkspace,
    pub floating: bool,
    pub monitor: i32,
    pub class: String,
    pub title: String,
    #[serde(default, alias = "initialClass")]
    pub initial_class: String,
    #[serde(default, alias = "initialTitle")]
    pub initial_title: String,
    pub pid: i32,
    pub xwayland: bool,
    #[serde(default)]
    pub pinned: bool,
    /// 0 = none, 1 = maximize, 2 = fullscreen
    #[serde(default)]
    pub fullscreen: i32,
    #[serde(default)]
    pub grouped: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, alias = "focusHistoryID")]
    pub focus_history_id: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: i32,
    pub name: String,
    pub monitor: String,
    #[serde(default, alias = "monitorID")]
    pub monitor_id: i32,
    pub windows: u32,
    #[serde(default)]
    pub hasfullscreen: bool,
    #[serde(default)]
    pub lastwindow: String,
    #[serde(default)]
    pub lastwindowtitle: String,
    #[serde(default)]
    pub ispersistent: bool,
}

/// `activeworkspace` shares the same shape as a normal workspace entry.
pub type ActiveWorkspace = Workspace;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Monitor {
    pub id: i32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub make: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub serial: String,
    pub width: i32,
    pub height: i32,
    #[serde(default, alias = "refreshRate")]
    pub refresh_rate: f64,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: i32,
    pub focused: bool,
    pub disabled: bool,
    #[serde(default)]
    pub dpms_status: bool,
    #[serde(default, alias = "activeWorkspace")]
    pub active_workspace: Option<ClientWorkspace>,
    #[serde(default, alias = "specialWorkspace")]
    pub special_workspace: Option<ClientWorkspace>,
}
