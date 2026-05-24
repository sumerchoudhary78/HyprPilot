//! [`InputRunner`] facade — real implementations land in the next
//! commit. This file holds the public type signature so callers can
//! depend on it.

use crate::detect::BackendAvailability;
use crate::error::Result;
use crate::keys::{KeyCombo, MouseButton};

pub struct InputRunner {
    pub(crate) backends: BackendAvailability,
}

impl InputRunner {
    pub fn with_backends(backends: BackendAvailability) -> Self {
        Self { backends }
    }

    pub fn detect() -> Self {
        Self::with_backends(BackendAvailability::detect())
    }

    pub fn backends(&self) -> &BackendAvailability {
        &self.backends
    }

    pub async fn type_text(&self, _text: &str) -> Result<()> {
        unimplemented!("InputRunner::type_text lands in the next commit");
    }

    pub async fn press_keys(&self, _combo: &KeyCombo) -> Result<()> {
        unimplemented!("InputRunner::press_keys lands in the next commit");
    }

    pub async fn mouse_move(&self, _x: i32, _y: i32, _absolute: bool) -> Result<()> {
        unimplemented!("InputRunner::mouse_move lands in the next commit");
    }

    pub async fn mouse_click(&self, _button: MouseButton) -> Result<()> {
        unimplemented!("InputRunner::mouse_click lands in the next commit");
    }
}
