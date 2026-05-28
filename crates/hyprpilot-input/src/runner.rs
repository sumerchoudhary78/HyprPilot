//! [`InputRunner`] — public facade over a swappable input backend.
//!
//! Today the only production backend is the wtype + ydotool shell-out
//! path. v0.7 adds a [`crate::libei::LibeiBackend`] stub behind the
//! `libei` cargo feature, selectable via `HYPRPILOT_INPUT_BACKEND=libei`.
//! Dispatch is an internal enum — callers see only the concrete
//! `InputRunner` methods, whose signatures are stable across backend
//! swaps.
//!
//! No shell is involved at any point: argv is built as a `Vec<String>`
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
use crate::virtual_pointer::VirtualPointer;

/// Env var selecting the input backend at daemon startup.
///
/// - unset / `wtype` — default wtype + ydotool path.
/// - `libei` — only honoured when compiled with `--features libei`;
///   otherwise [`InputError::InvalidBackendSelection`].
pub const BACKEND_ENV: &str = "HYPRPILOT_INPUT_BACKEND";

enum BackendImpl {
    WtypeYdotool(WtypeYdotoolBackend),
    #[cfg(feature = "libei")]
    Libei(crate::libei::LibeiBackend),
}

impl BackendImpl {
    fn name(&self) -> &'static str {
        match self {
            BackendImpl::WtypeYdotool(_) => "wtype",
            #[cfg(feature = "libei")]
            BackendImpl::Libei(_) => "libei",
        }
    }
}

pub struct InputRunner {
    pub(crate) backends: BackendAvailability,
    backend: BackendImpl,
    /// Native wlr-virtual-pointer transport for mouse motion/clicks.
    /// `None` when the compositor doesn't advertise the protocol or no
    /// Wayland display is reachable; mouse falls back to ydotool in that
    /// case. Constructed only by [`Self::detect`] / [`Self::detect_with`]
    /// — `with_backends` stays VP-free so unit tests don't need a live
    /// Wayland server.
    virtual_pointer: Option<VirtualPointer>,
}

impl InputRunner {
    /// Construct with explicit detection results. Selects the wtype path
    /// unconditionally and does *not* attempt a Wayland connection —
    /// used by tests and any caller that wants to bypass the env-var
    /// selector. Production code paths go through [`Self::detect`] so
    /// the virtual-pointer transport is initialised.
    pub fn with_backends(backends: BackendAvailability) -> Self {
        let backend = BackendImpl::WtypeYdotool(WtypeYdotoolBackend {
            backends: backends.clone(),
        });
        Self {
            backends,
            backend,
            virtual_pointer: None,
        }
    }

    /// Probe `$PATH` and pick a backend.
    ///
    /// Reads [`BACKEND_ENV`] to decide between `wtype` (default) and
    /// `libei` (gated on cargo feature + env var). On any selection
    /// error it logs and falls back to the wtype path — the daemon
    /// should remain runnable.
    ///
    /// Also attempts to attach a [`VirtualPointer`] for mouse — silent
    /// fallback to ydotool if the compositor doesn't support it.
    pub fn detect() -> Self {
        let backends = BackendAvailability::detect();
        Self::detect_with(backends).unwrap_or_else(|err| {
            tracing::warn!(
                error = %err,
                "falling back to wtype backend after selection failure"
            );
            Self::with_backends(BackendAvailability::detect()).attach_virtual_pointer()
        })
    }

    /// Same as [`Self::detect`] but surfaces backend-selection errors so
    /// callers (and tests) can assert on them.
    pub fn detect_with(backends: BackendAvailability) -> Result<Self> {
        let selection = std::env::var(BACKEND_ENV).ok();
        let runner = match selection.as_deref() {
            None | Some("") | Some("wtype") => Self::with_backends(backends),
            Some("libei") => Self::new_libei(backends)?,
            Some(other) => return Err(InputError::InvalidBackendSelection(other.to_string())),
        };
        Ok(runner.attach_virtual_pointer())
    }

    /// Try to bring up the wlr-virtual-pointer transport for mouse. On
    /// failure the runner keeps `virtual_pointer == None` and mouse calls
    /// transparently fall back to ydotool. Idempotent: safe to call again
    /// (replaces any existing handle).
    fn attach_virtual_pointer(mut self) -> Self {
        match VirtualPointer::try_new() {
            Some(vp) => {
                tracing::info!("mouse transport: wlr-virtual-pointer-v1 (native Wayland)");
                self.virtual_pointer = Some(vp);
            }
            None => {
                tracing::info!("mouse transport: ydotool (wlr-virtual-pointer unavailable)");
            }
        }
        self
    }

    #[cfg(feature = "libei")]
    fn new_libei(backends: BackendAvailability) -> Result<Self> {
        let libei = crate::libei::LibeiBackend::detect()?;
        Ok(Self {
            backends,
            backend: BackendImpl::Libei(libei),
        })
    }

    #[cfg(not(feature = "libei"))]
    fn new_libei(_backends: BackendAvailability) -> Result<Self> {
        Err(InputError::InvalidBackendSelection(
            "libei (rebuild with `--features libei`)".into(),
        ))
    }

    pub fn backends(&self) -> &BackendAvailability {
        &self.backends
    }

    /// Name of the active typing/keyboard backend — `"wtype"` or
    /// `"libei"`. Mouse uses its own transport — see
    /// [`Self::mouse_backend_name`].
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Which transport mouse motion/clicks go through. Reflects the
    /// runtime state of [`VirtualPointer`] attachment, not a compile-time
    /// feature: a daemon started against a compositor that doesn't
    /// advertise wlr-virtual-pointer-v1 reports `"ydotool"` even though
    /// the code path was built in.
    pub fn mouse_backend_name(&self) -> &'static str {
        if self.virtual_pointer.is_some() {
            "wlr-virtual-pointer"
        } else {
            "ydotool"
        }
    }

    pub async fn type_text(&self, text: &str) -> Result<()> {
        match &self.backend {
            BackendImpl::WtypeYdotool(b) => b.type_text(text).await,
            #[cfg(feature = "libei")]
            BackendImpl::Libei(_) => Err(InputError::NotImplemented {
                backend: "libei",
                op: "type_text",
            }),
        }
    }

    pub async fn press_keys(&self, combo: &KeyCombo) -> Result<()> {
        match &self.backend {
            BackendImpl::WtypeYdotool(b) => b.press_keys(combo).await,
            #[cfg(feature = "libei")]
            BackendImpl::Libei(_) => Err(InputError::NotImplemented {
                backend: "libei",
                op: "press_keys",
            }),
        }
    }

    /// Move the pointer. Prefers the native wlr-virtual-pointer
    /// transport — events go straight to the compositor, untouched by
    /// libinput pointer-accel. Falls back to ydotool (uinput → libinput)
    /// only when the protocol isn't available; that path still suffers
    /// from issue #25's 2x drift on most Hyprland setups.
    pub async fn mouse_move(&self, x: i32, y: i32, absolute: bool) -> Result<()> {
        if let Some(vp) = &self.virtual_pointer {
            return if absolute {
                vp.move_to(x, y).await
            } else {
                vp.move_by(x, y).await
            };
        }
        match &self.backend {
            BackendImpl::WtypeYdotool(b) => b.mouse_move(x, y, absolute).await,
            #[cfg(feature = "libei")]
            BackendImpl::Libei(_) => Err(InputError::NotImplemented {
                backend: "libei",
                op: "mouse_move",
            }),
        }
    }

    /// Click a mouse button. Same transport preference as
    /// [`Self::mouse_move`].
    pub async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        if let Some(vp) = &self.virtual_pointer {
            return vp.click(button).await;
        }
        match &self.backend {
            BackendImpl::WtypeYdotool(b) => b.mouse_click(button).await,
            #[cfg(feature = "libei")]
            BackendImpl::Libei(_) => Err(InputError::NotImplemented {
                backend: "libei",
                op: "mouse_click",
            }),
        }
    }
}

// ---- wtype + ydotool backend ---------------------------------------------

struct WtypeYdotoolBackend {
    backends: BackendAvailability,
}

impl WtypeYdotoolBackend {
    async fn type_text(&self, text: &str) -> Result<()> {
        let wtype = self
            .backends
            .wtype
            .as_ref()
            .ok_or(InputError::BackendMissing("wtype"))?;
        if text.is_empty() {
            return Ok(());
        }
        debug!(bytes = text.len(), "wtype stdin typing");
        let mut child = Command::new(wtype)
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await?;
        }
        let output = child.wait_with_output().await?;
        finish("wtype", output)
    }

    /// Press a key combo. Prefers ydotool: its events go through
    /// uinput→libinput, so Hyprland's global bind matcher sees the chord
    /// (`super+T` fires a `bind`). wtype's virtual-keyboard events are
    /// filtered out of the bind matcher and reach only the focused client.
    ///
    /// Falls back to wtype when ydotool/ydotoold is unavailable, or when the
    /// keysym has no Linux input-event-code in our table — the chord still
    /// reaches the focused window, it just won't trigger compositor binds.
    async fn press_keys(&self, combo: &KeyCombo) -> Result<()> {
        if let Some(ydotool) = self.backends.ydotool.as_ref() {
            if self.backends.ydotoold_reachable() {
                match combo.encode_ydotool_key() {
                    Ok(argv) => return self.run_ydotool_key(ydotool, &argv).await,
                    Err(e) => debug!(
                        error = %e,
                        "keysym has no evdev code; falling back to wtype for press_keys"
                    ),
                }
            }
        }
        self.press_keys_wtype(combo).await
    }

    async fn run_ydotool_key(&self, ydotool: &std::path::Path, argv: &[String]) -> Result<()> {
        debug!(argv = ?argv, "ydotool key combo");
        let output = Command::new(ydotool)
            .arg("key")
            .args(argv)
            .env("YDOTOOL_SOCKET", &self.backends.ydotoold_socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await?;
        finish("ydotool", output)
    }

    async fn press_keys_wtype(&self, combo: &KeyCombo) -> Result<()> {
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

    async fn mouse_move(&self, x: i32, y: i32, absolute: bool) -> Result<()> {
        let ydotool = self
            .backends
            .ydotool
            .as_ref()
            .ok_or(InputError::BackendMissing("ydotool"))?;
        if !self.backends.ydotoold_reachable() {
            return Err(InputError::DaemonNotReachable("ydotoold"));
        }
        let argv = ydotool_mousemove_argv(x, y, absolute);
        debug!(argv = ?argv, "ydotool mousemove");
        let output = Command::new(ydotool)
            .args(&argv)
            .env("YDOTOOL_SOCKET", &self.backends.ydotoold_socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await?;
        finish("ydotool", output)
    }

    async fn mouse_click(&self, button: MouseButton) -> Result<()> {
        let ydotool = self
            .backends
            .ydotool
            .as_ref()
            .ok_or(InputError::BackendMissing("ydotool"))?;
        if !self.backends.ydotoold_reachable() {
            return Err(InputError::DaemonNotReachable("ydotoold"));
        }
        let code = button.ydotool_click_code();
        debug!(button = ?button, code, "ydotool click");
        let output = Command::new(ydotool)
            .args(["click", code])
            .env("YDOTOOL_SOCKET", &self.backends.ydotoold_socket)
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
    use std::sync::Mutex;

    // Env vars are process-global; serialise the tests that touch
    // BACKEND_ENV so they don't race each other under `cargo test`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn no_backends() -> BackendAvailability {
        BackendAvailability {
            wtype: None,
            ydotool: None,
            ydotoold_socket: PathBuf::from("/bin/true"),
        }
    }
    fn fake_backends() -> BackendAvailability {
        BackendAvailability {
            wtype: Some(PathBuf::from("/bin/true")),
            ydotool: Some(PathBuf::from("/bin/true")),
            ydotoold_socket: PathBuf::from("/bin/true"),
        }
    }
    fn ydotool_present_no_daemon() -> BackendAvailability {
        BackendAvailability {
            wtype: Some(PathBuf::from("/bin/true")),
            ydotool: Some(PathBuf::from("/bin/true")),
            ydotoold_socket: PathBuf::from(
                "/tmp/hyprpilot-test-nonexistent-ydotoold-socket-zzz",
            ),
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
        let r = InputRunner::with_backends(fake_backends());
        r.type_text("").await.unwrap();
    }

    #[tokio::test]
    async fn fake_wtype_success_path() {
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

    #[tokio::test]
    async fn mouse_ops_without_ydotoold_socket_return_daemon_not_reachable() {
        let r = InputRunner::with_backends(ydotool_present_no_daemon());
        assert!(matches!(
            r.mouse_move(10, 10, false).await,
            Err(InputError::DaemonNotReachable("ydotoold"))
        ));
        assert!(matches!(
            r.mouse_click(MouseButton::Left).await,
            Err(InputError::DaemonNotReachable("ydotoold"))
        ));
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
        let argv = ydotool_mousemove_argv(-100, -200, false);
        assert!(argv.contains(&"--".to_string()));
        let dash_idx = argv.iter().position(|s| s == "--").unwrap();
        let x_idx = argv.iter().position(|s| s == "-100").unwrap();
        assert!(dash_idx < x_idx, "-- must precede negative coords");
    }

    #[test]
    fn default_backend_is_wtype() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var(BACKEND_ENV).ok();
        std::env::remove_var(BACKEND_ENV);
        let r = InputRunner::detect_with(fake_backends()).unwrap();
        assert_eq!(r.backend_name(), "wtype");
        if let Some(v) = prev {
            std::env::set_var(BACKEND_ENV, v);
        }
    }

    #[test]
    fn unknown_backend_selection_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var(BACKEND_ENV).ok();
        std::env::set_var(BACKEND_ENV, "garbage");
        let r = InputRunner::detect_with(fake_backends());
        match r {
            Err(InputError::InvalidBackendSelection(s)) => assert_eq!(s, "garbage"),
            Err(e) => panic!("expected InvalidBackendSelection, got error {e:?}"),
            Ok(_) => panic!("expected InvalidBackendSelection, got Ok"),
        }
        match prev {
            Some(v) => std::env::set_var(BACKEND_ENV, v),
            None => std::env::remove_var(BACKEND_ENV),
        }
    }

    #[cfg(not(feature = "libei"))]
    #[test]
    fn libei_selection_without_feature_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var(BACKEND_ENV).ok();
        std::env::set_var(BACKEND_ENV, "libei");
        let r = InputRunner::detect_with(fake_backends());
        assert!(matches!(r, Err(InputError::InvalidBackendSelection(_))));
        match prev {
            Some(v) => std::env::set_var(BACKEND_ENV, v),
            None => std::env::remove_var(BACKEND_ENV),
        }
    }

    #[cfg(feature = "libei")]
    #[tokio::test]
    async fn libei_selection_with_feature_returns_not_implemented() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var(BACKEND_ENV).ok();
        std::env::set_var(BACKEND_ENV, "libei");
        let r = InputRunner::detect_with(fake_backends())
            .expect("libei backend should construct under feature flag");
        assert_eq!(r.backend_name(), "libei");
        match r.type_text("hi").await {
            Err(InputError::NotImplemented {
                backend: "libei",
                op: "type_text",
            }) => {}
            other => panic!("expected NotImplemented(libei, type_text), got {other:?}"),
        }
        match r.mouse_click(MouseButton::Left).await {
            Err(InputError::NotImplemented {
                backend: "libei",
                op: "mouse_click",
            }) => {}
            other => panic!("expected NotImplemented(libei, mouse_click), got {other:?}"),
        }
        match prev {
            Some(v) => std::env::set_var(BACKEND_ENV, v),
            None => std::env::remove_var(BACKEND_ENV),
        }
    }
}
