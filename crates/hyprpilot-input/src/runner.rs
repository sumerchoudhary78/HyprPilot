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
}

impl InputRunner {
    /// Construct with explicit detection results. Selects the wtype path
    /// unconditionally — used by tests and any caller that wants to
    /// bypass the env-var selector.
    pub fn with_backends(backends: BackendAvailability) -> Self {
        let backend = BackendImpl::WtypeYdotool(WtypeYdotoolBackend {
            backends: backends.clone(),
        });
        Self { backends, backend }
    }

    /// Probe `$PATH` and pick a backend.
    ///
    /// Reads [`BACKEND_ENV`] to decide between `wtype` (default) and
    /// `libei` (gated on cargo feature + env var). On any selection
    /// error it logs and falls back to the wtype path — the daemon
    /// should remain runnable.
    pub fn detect() -> Self {
        let backends = BackendAvailability::detect();
        Self::detect_with(backends).unwrap_or_else(|err| {
            tracing::warn!(
                error = %err,
                "falling back to wtype backend after selection failure"
            );
            Self::with_backends(BackendAvailability::detect())
        })
    }

    /// Same as [`Self::detect`] but surfaces backend-selection errors so
    /// callers (and tests) can assert on them.
    pub fn detect_with(backends: BackendAvailability) -> Result<Self> {
        let selection = std::env::var(BACKEND_ENV).ok();
        match selection.as_deref() {
            None | Some("") | Some("wtype") => Ok(Self::with_backends(backends)),
            Some("libei") => Self::new_libei(backends),
            Some(other) => Err(InputError::InvalidBackendSelection(other.to_string())),
        }
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

    /// Name of the active backend — `"wtype"` or `"libei"`. Useful for
    /// startup logging and diagnostics.
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
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

    pub async fn mouse_move(&self, x: i32, y: i32, absolute: bool) -> Result<()> {
        match &self.backend {
            BackendImpl::WtypeYdotool(b) => b.mouse_move(x, y, absolute).await,
            #[cfg(feature = "libei")]
            BackendImpl::Libei(_) => Err(InputError::NotImplemented {
                backend: "libei",
                op: "mouse_move",
            }),
        }
    }

    pub async fn mouse_click(&self, button: MouseButton) -> Result<()> {
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
