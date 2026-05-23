//! Low-level transport: locate the live Hyprland instance and talk to its
//! control socket.

use std::env;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::error::{Error, Result};

/// A discovered Hyprland instance.
#[derive(Debug, Clone)]
pub struct Instance {
    /// Instance signature — the directory name under `$XDG_RUNTIME_DIR/hypr/`.
    pub signature: String,
    /// Resolved `$XDG_RUNTIME_DIR`.
    pub runtime_dir: PathBuf,
}

impl Instance {
    /// Discover the live instance.
    ///
    /// Prefers `$HYPRLAND_INSTANCE_SIGNATURE`. Falls back to scanning
    /// `$XDG_RUNTIME_DIR/hypr/` and picking the most recently modified
    /// directory whose `.socket.sock` exists.
    pub fn discover() -> Result<Self> {
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or(Error::NoRuntimeDir)?;

        let hypr_dir = runtime_dir.join("hypr");

        if let Some(sig) = env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
            let sig = sig.to_string_lossy().into_owned();
            let inst = Self { signature: sig, runtime_dir };
            if !inst.control_socket().exists() {
                return Err(Error::SocketMissing(inst.control_socket()));
            }
            return Ok(inst);
        }

        let mut latest: Option<(String, SystemTime)> = None;
        let entries = std::fs::read_dir(&hypr_dir)
            .map_err(|_| Error::NoInstance(hypr_dir.clone()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.join(".socket.sock").exists() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if latest.as_ref().map_or(true, |(_, t)| mtime > *t) {
                latest = Some((name, mtime));
            }
        }

        let signature = latest.ok_or(Error::NoInstance(hypr_dir))?.0;
        Ok(Self { signature, runtime_dir })
    }

    pub fn control_socket(&self) -> PathBuf {
        self.runtime_dir.join("hypr").join(&self.signature).join(".socket.sock")
    }

    pub fn event_socket(&self) -> PathBuf {
        self.runtime_dir.join("hypr").join(&self.signature).join(".socket2.sock")
    }
}

/// Async client to Hyprland's control socket.
///
/// One control-socket connection = one request. The server closes the socket
/// after responding. [`Connection`] opens a fresh stream per call.
#[derive(Debug, Clone)]
pub struct Connection {
    instance: Instance,
    request_timeout: Duration,
}

impl Connection {
    /// Connect to the auto-discovered instance with a 2-second I/O timeout.
    pub fn new() -> Result<Self> {
        Ok(Self::with_instance(Instance::discover()?))
    }

    pub fn with_instance(instance: Instance) -> Self {
        Self { instance, request_timeout: Duration::from_secs(2) }
    }

    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    pub fn set_timeout(&mut self, d: Duration) {
        self.request_timeout = d;
    }

    /// Send a raw command string and return the full text response.
    ///
    /// The caller is responsible for the wire format. Most callers should use
    /// [`Connection::query`] or [`Connection::dispatch`] instead.
    pub async fn send_raw(&self, cmd: &str) -> Result<String> {
        let path = self.instance.control_socket();
        let fut = async {
            let mut stream = UnixStream::connect(&path).await?;
            stream.write_all(cmd.as_bytes()).await?;
            stream.shutdown().await?;
            let mut buf = Vec::with_capacity(4096);
            stream.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&buf).into_owned())
        };
        match timeout(self.request_timeout, fut).await {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => Err(Error::Io(e)),
            Err(_) => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Hyprland control socket request timed out",
            ))),
        }
    }

    /// Run a JSON-returning query (`j/<name>`) and deserialize.
    pub async fn query<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<T> {
        let raw = self.send_raw(&format!("j/{name}")).await?;
        serde_json::from_str(&raw).map_err(Error::Json)
    }

    /// Run a dispatcher.
    ///
    /// `args` is the verb plus its arguments, e.g. `"focuswindow address:0x123"`.
    /// Returns `Ok(())` only on `"ok"`. Anything else becomes [`Error::Rejected`]
    /// or [`Error::UnknownDispatcher`].
    pub async fn dispatch(&self, args: &str) -> Result<()> {
        let raw = self.send_raw(&format!("dispatch {args}")).await?;
        let trimmed = raw.trim();
        if trimmed == "ok" {
            return Ok(());
        }
        let verb = args.split_whitespace().next().unwrap_or("").to_string();
        if trimmed.starts_with("Invalid dispatcher") {
            return Err(Error::UnknownDispatcher(verb));
        }
        Err(Error::Rejected { verb, message: trimmed.to_string() })
    }

    /// Apply a live `keyword` (config) override, e.g.
    /// `("general:gaps_in", "5")`.
    pub async fn keyword(&self, key: &str, value: &str) -> Result<()> {
        let raw = self.send_raw(&format!("keyword {key} {value}")).await?;
        let trimmed = raw.trim();
        if trimmed == "ok" {
            return Ok(());
        }
        Err(Error::Rejected {
            verb: format!("keyword {key}"),
            message: trimmed.to_string(),
        })
    }
}
