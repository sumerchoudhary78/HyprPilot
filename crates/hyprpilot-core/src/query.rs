//! Typed read-only queries against Hyprland's control socket.
//!
//! Hyprland's `activewindow` returns the literal JSON `{}` when no window is
//! focused. We surface that as `Option::None` rather than failing to parse.

use crate::error::{Error, Result};
use crate::ipc::Connection;
use crate::types::*;

impl Connection {
    pub async fn version(&self) -> Result<Version> {
        self.query("version").await
    }

    pub async fn clients(&self) -> Result<Vec<Client>> {
        self.query("clients").await
    }

    pub async fn workspaces(&self) -> Result<Vec<Workspace>> {
        self.query("workspaces").await
    }

    pub async fn monitors(&self) -> Result<Vec<Monitor>> {
        self.query("monitors").await
    }

    /// All configured keybinds. Hyprland doesn't send readable modifier
    /// names, so we decode each `modmask` into [`Bind::mods`] here.
    pub async fn binds(&self) -> Result<Vec<Bind>> {
        let mut binds: Vec<Bind> = self.query("binds").await?;
        for b in &mut binds {
            b.mods = Bind::decode_mods(b.modmask);
        }
        Ok(binds)
    }

    pub async fn active_workspace(&self) -> Result<ActiveWorkspace> {
        self.query("activeworkspace").await
    }

    /// `None` when no window currently has focus.
    pub async fn active_window(&self) -> Result<Option<Client>> {
        let raw = self.send_raw("j/activewindow").await?;
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(Error::Json)?;
        match &value {
            serde_json::Value::Object(map) if map.is_empty() => Ok(None),
            _ => Ok(Some(serde_json::from_value(value).map_err(Error::Json)?)),
        }
    }

    pub async fn cursor_position(&self) -> Result<CursorPos> {
        self.query("cursorpos").await
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CursorPos {
    pub x: i32,
    pub y: i32,
}
