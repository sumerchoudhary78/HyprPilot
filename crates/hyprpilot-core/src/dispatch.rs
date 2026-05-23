//! Typed dispatcher API.
//!
//! Each method maps to exactly one Hyprland dispatcher and encodes its
//! arguments. Methods that target the focused window are named `*_active`;
//! methods that take a selector accept a [`crate::WindowSelector`].

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ipc::Connection;
use crate::selector::{WindowSelector, WorkspaceRef};

/// Movement / resize direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn as_letter(self) -> &'static str {
        match self {
            Direction::Left => "l",
            Direction::Right => "r",
            Direction::Up => "u",
            Direction::Down => "d",
        }
    }
}

/// Fullscreen mode argument for `fullscreen`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullscreenMode {
    /// Maximized (covers monitor, respects gaps/bars).
    Maximize = 0,
    /// True fullscreen.
    Fullscreen = 1,
}

impl Connection {
    // ---- Focus / cycling -----------------------------------------------------

    pub async fn focus_window(&self, sel: &WindowSelector) -> Result<()> {
        self.dispatch(&format!("focuswindow {}", sel.encode())).await
    }

    pub async fn cycle_next(&self) -> Result<()> {
        self.dispatch("cyclenext").await
    }

    pub async fn cycle_prev(&self) -> Result<()> {
        self.dispatch("cyclenext prev").await
    }

    pub async fn focus_monitor(&self, name_or_dir: &str) -> Result<()> {
        self.dispatch(&format!("focusmonitor {name_or_dir}")).await
    }

    // ---- Lifecycle -----------------------------------------------------------

    pub async fn kill_active(&self) -> Result<()> {
        self.dispatch("killactive").await
    }

    pub async fn close_window(&self, sel: &WindowSelector) -> Result<()> {
        self.dispatch(&format!("closewindow {}", sel.encode())).await
    }

    /// Spawn an external command. Hyprland forks and execs; the command runs
    /// detached from the dispatcher's reply.
    pub async fn exec(&self, command: &str) -> Result<()> {
        self.dispatch(&format!("exec {command}")).await
    }

    // ---- Workspace movement --------------------------------------------------

    pub async fn switch_workspace(&self, ws: &WorkspaceRef) -> Result<()> {
        self.dispatch(&format!("workspace {}", ws.encode())).await
    }

    pub async fn move_active_to_workspace(&self, ws: &WorkspaceRef) -> Result<()> {
        self.dispatch(&format!("movetoworkspace {}", ws.encode())).await
    }

    pub async fn move_active_to_workspace_silent(&self, ws: &WorkspaceRef) -> Result<()> {
        self.dispatch(&format!("movetoworkspacesilent {}", ws.encode())).await
    }

    /// Move the focused workspace to a named monitor.
    pub async fn move_workspace_to_monitor(&self, monitor: &str) -> Result<()> {
        self.dispatch(&format!("movecurrentworkspacetomonitor {monitor}")).await
    }

    // ---- Window state --------------------------------------------------------

    pub async fn toggle_floating(&self) -> Result<()> {
        self.dispatch("togglefloating").await
    }

    pub async fn set_fullscreen(&self, mode: FullscreenMode) -> Result<()> {
        self.dispatch(&format!("fullscreen {}", mode as u8)).await
    }

    pub async fn center_active(&self) -> Result<()> {
        self.dispatch("centerwindow").await
    }

    pub async fn pin_active(&self) -> Result<()> {
        self.dispatch("pin").await
    }

    // ---- Geometry ------------------------------------------------------------

    pub async fn move_active(&self, dir: Direction) -> Result<()> {
        self.dispatch(&format!("movewindow {}", dir.as_letter())).await
    }

    pub async fn resize_active(&self, dx: i32, dy: i32) -> Result<()> {
        self.dispatch(&format!("resizeactive {dx} {dy}")).await
    }

    pub async fn swap_active(&self, dir: Direction) -> Result<()> {
        self.dispatch(&format!("swapwindow {}", dir.as_letter())).await
    }
}
