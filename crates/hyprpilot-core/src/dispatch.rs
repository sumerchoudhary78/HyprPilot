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

    // ---- Selector-targeting variants ----------------------------------------
    //
    // These target a specific window by selector rather than the focused one.
    // Used by snapshot restore (and any caller that needs deterministic
    // targeting without disturbing the active window).
    //
    // Hyprland's dispatcher grammar puts the selector after the args,
    // comma-separated, e.g. `movewindowpixel exact 100 200,address:0x...`.

    /// Move a floating window to an exact (x, y) screen-space position.
    /// Hyprland silently no-ops this on tiled windows.
    pub async fn move_window_pixel(&self, sel: &WindowSelector, x: i32, y: i32) -> Result<()> {
        self.dispatch(&format!(
            "movewindowpixel exact {x} {y},{}",
            sel.encode()
        ))
        .await
    }

    /// Resize a floating window to an exact (width, height) in pixels.
    pub async fn resize_window_pixel(
        &self,
        sel: &WindowSelector,
        width: i32,
        height: i32,
    ) -> Result<()> {
        self.dispatch(&format!(
            "resizewindowpixel exact {width} {height},{}",
            sel.encode()
        ))
        .await
    }

    /// Toggle pin (sticky-across-workspaces) for a specific window. Hyprland
    /// only allows pinning floating windows; tiled windows return a typed
    /// rejection. The caller should ensure the window is floating first if
    /// the goal is to pin it.
    pub async fn pin_window(&self, sel: &WindowSelector) -> Result<()> {
        self.dispatch(&format!("pin {}", sel.encode())).await
    }

    /// Toggle floating mode on a specific window (not just the active one).
    pub async fn toggle_floating_window(&self, sel: &WindowSelector) -> Result<()> {
        self.dispatch(&format!("togglefloating {}", sel.encode())).await
    }

    /// Set fullscreen state on a specific window.
    ///
    /// Hyprland's `fullscreenstate` dispatcher takes two arguments: an
    /// `internal` value (what the app thinks it has) and an `external` value
    /// (what Hyprland renders). `-1` for either means "leave unchanged".
    /// For restoration we typically set internal to the target mode and
    /// leave external as `-1`.
    ///
    /// Mode values follow Hyprland's [`crate::types::Client::fullscreen`]
    /// reporting: 0 = none, 1 = maximize, 2 = exclusive fullscreen.
    pub async fn set_fullscreen_state(
        &self,
        sel: &WindowSelector,
        internal: i32,
        external: i32,
    ) -> Result<()> {
        self.dispatch(&format!(
            "fullscreenstate {internal} {external},{}",
            sel.encode()
        ))
        .await
    }
}
