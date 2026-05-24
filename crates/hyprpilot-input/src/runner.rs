//! [`InputRunner`] — shell-out implementations over wtype + ydotool.
//!
//! Each method:
//! 1. Checks the backend is available; returns
//!    [`InputError::BackendMissing`] if not.
//! 2. Builds the argv (see `wtype_argv_for_text` etc. for the
//!    unit-testable parts).
//! 3. Spawns `tokio::process::Command`, captures stderr for diagnostics,
//!    returns [`InputError::BackendFailed`] on non-zero exit.
//!
//! No shell is involved at any point — argv is built as a `Vec<String>`
//! and passed via `Command::arg`/`args`. User-supplied text reaches
//! wtype on stdin (wtype's `-` flag), so even text starting with `-`
//! cannot be misinterpreted as a flag.

use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;

use crate::detect::BackendAvailability;
use crate::error::{InputError, Result};
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

    /// Type free-form text into the focused window via `wtype`.
    ///
    /// Text is piped through stdin (wtype's `-` flag), so leading `-`
    /// characters are NOT misinterpreted as options.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        let wtype = self
            .backends
            .wtype
            .as_ref()
            .ok_or(InputError::BackendMissing("wtype"))?;
        if text.is_empty() {
            return Ok(()); // no-op; wtype would also do nothing
        }
        debug!(bytes = text.len(), "wtype stdin typing");
        let mut child = Command::new(wtype)
            .arg("-") // read text to type from stdin
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await?;
            // Drop stdin to send EOF; wtype exits once input ends.
        }
        let output = child.wait_with_output().await?;
        finish("wtype", output)
    }

    /// Press a key combo in the focused window via `wtype`.
    pub async fn press_keys(&self, combo: &KeyCombo) -> Result<()> {
        let wtype = self
            .backends
            .wtype
            .as_ref()
            .ok_or(InputError::BackendMissing("wtype"))?;
        let argv = combo.encode_wtype();
        debug!(argv = ?argv, "wtype key combo");
        let output = Command::new(wtype)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await?;
        finish("wtype", output)
    }

    /// Move the mouse cursor via `ydotool mousemove`. `absolute=true`
    /// moves to screen coordinates; `absolute=false` is a relative delta.
    pub async fn mouse_move(&self, x: i32, y: i32, absolute: bool) -> Result<()> {
        let ydotool = self
            .backends
            .ydotool
            .as_ref()
            .ok_or(InputError::BackendMissing("ydotool"))?;
        let argv = ydotool_mousemove_argv(x, y, absolute);
        debug!(argv = ?argv, "ydotool mousemove");
        let output = Command::new(ydotool)
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await?;
        finish("ydotool", output)
    }

    /// Click a mouse button (press + release in one shot) via
    /// `ydotool click`.
    pub async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        let ydotool = self
            .backends
            .ydotool
            .as_ref()
            .ok_or(InputError::BackendMissing("ydotool"))?;
        let code = button.ydotool_click_code();
        debug!(button = ?button, code, "ydotool click");
        let output = Command::new(ydotool)
            .args(["click", code])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await?;
        finish("ydotool", output)
    }
}

// ---- private helpers ------------------------------------------------------

fn finish(backend: &'static str, output: std::process::Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(InputError::BackendFailed {
        backend,
        status: output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "<signal>".into()),
        stderr,
    })
}

/// `ydotool mousemove` argv. Extracted as a free function so it's
/// trivial to unit-test the construction without spawning anything.
///
/// Uses `--` to terminate ydotool's option parsing before the numeric
/// args so negative coordinates can never be parsed as flags.
fn ydotool_mousemove_argv(x: i32, y: i32, absolute: bool) -> Vec<String> {
    let mut argv = Vec::with_capacity(5);
    argv.push("mousemove".into());
    if absolute {
        argv.push("-a".into());
    }
    argv.push("--".into());
    argv.push(x.to_string());
    argv.push(y.to_string());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyCombo;
    use std::path::PathBuf;

    fn no_backends() -> BackendAvailability {
        BackendAvailability { wtype: None, ydotool: None }
    }
    fn fake_backends() -> BackendAvailability {
        // Point at /bin/true for wtype / ydotool so the runner can spawn
        // something and check the success path. Real wtype/ydotool need
        // a live Wayland compositor and we won't hit them in unit tests.
        BackendAvailability {
            wtype: Some(PathBuf::from("/bin/true")),
            ydotool: Some(PathBuf::from("/bin/true")),
        }
    }

    #[tokio::test]
    async fn type_text_without_wtype_returns_backend_missing() {
        let r = InputRunner::with_backends(no_backends());
        match r.type_text("hi").await {
            Err(InputError::BackendMissing("wtype")) => {}
            other => panic!("expected BackendMissing(wtype), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn press_keys_without_wtype_returns_backend_missing() {
        let r = InputRunner::with_backends(no_backends());
        let combo = KeyCombo::parse("ctrl+t").unwrap();
        match r.press_keys(&combo).await {
            Err(InputError::BackendMissing("wtype")) => {}
            other => panic!("expected BackendMissing(wtype), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mouse_ops_without_ydotool_return_backend_missing() {
        let r = InputRunner::with_backends(no_backends());
        assert!(matches!(
            r.mouse_move(10, 10, false).await,
            Err(InputError::BackendMissing("ydotool"))
        ));
        assert!(matches!(
            r.mouse_click(MouseButton::Left).await,
            Err(InputError::BackendMissing("ydotool"))
        ));
    }

    #[tokio::test]
    async fn empty_text_is_a_noop() {
        // No backend needed for the empty-text fast path? — Actually we
        // still bail if wtype is missing because we check first.
        let r = InputRunner::with_backends(fake_backends());
        // With fake `/bin/true` for wtype, type_text("") returns Ok and
        // doesn't even spawn.
        r.type_text("").await.unwrap();
    }

    #[tokio::test]
    async fn fake_wtype_success_path() {
        // `/bin/true` exits 0 and ignores stdin; finish() sees status
        // success and returns Ok.
        let r = InputRunner::with_backends(fake_backends());
        r.type_text("hello").await.unwrap();

        let combo = KeyCombo::parse("ctrl+a").unwrap();
        r.press_keys(&combo).await.unwrap();
    }

    #[tokio::test]
    async fn fake_ydotool_success_path() {
        let r = InputRunner::with_backends(fake_backends());
        r.mouse_move(50, -50, false).await.unwrap();
        r.mouse_click(MouseButton::Left).await.unwrap();
    }

    #[test]
    fn ydotool_mousemove_argv_relative() {
        assert_eq!(
            ydotool_mousemove_argv(100, -50, false),
            vec!["mousemove", "--", "100", "-50"]
        );
    }

    #[test]
    fn ydotool_mousemove_argv_absolute() {
        assert_eq!(
            ydotool_mousemove_argv(0, 0, true),
            vec!["mousemove", "-a", "--", "0", "0"]
        );
    }

    #[test]
    fn ydotool_mousemove_argv_double_dash_protects_negative_coords() {
        // The point of `--`: without it, ydotool would see `-50` as a
        // short option and reject it. With it, args after are literal.
        let argv = ydotool_mousemove_argv(-100, -200, false);
        assert!(argv.contains(&"--".to_string()));
        let dash_idx = argv.iter().position(|s| s == "--").unwrap();
        let x_idx = argv.iter().position(|s| s == "-100").unwrap();
        assert!(dash_idx < x_idx, "-- must precede negative coords");
    }
}
