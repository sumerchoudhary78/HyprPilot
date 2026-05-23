//! Strongly typed views of Hyprland's JSON outputs.
//!
//! Field naming follows Hyprland's JSON (camelCase / lowercase) via serde
//! aliases. Unknown fields are tolerated so future Hyprland additions don't
//! break deserialisation.

use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub initial_class: String,
    #[serde(default)]
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
