//! Client used by the CLI (and tests) to talk to a running daemon.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use hyprpilot_core::snapshot::SnapshotRestorePreview;

use crate::protocol::{Request, RequestEnvelope, Response, ResponseEnvelope, RpcError};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("daemon socket not reachable at {path}: {source}")]
    Connect { path: PathBuf, source: std::io::Error },
    #[error("daemon closed the connection unexpectedly")]
    Closed,
    #[error("I/O failure talking to daemon: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to encode RPC request: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("daemon returned response with mismatched id (expected {expected}, got {got})")]
    IdMismatch { expected: u64, got: u64 },
    #[error("daemon error [{code}]: {message}")]
    Rpc { code: String, message: String },
}

pub struct DaemonClient {
    stream: BufReader<UnixStream>,
    next_id: AtomicU64,
    path: PathBuf,
}

impl DaemonClient {
    pub async fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| ClientError::Connect { path: path.to_path_buf(), source: e })?;
        Ok(Self {
            stream: BufReader::new(stream),
            next_id: AtomicU64::new(1),
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Send a typed request and decode the success payload as `T`.
    pub async fn call<T: DeserializeOwned>(&mut self, request: Request) -> Result<T, ClientError> {
        let value = self.call_raw(request).await?;
        serde_json::from_value(value).map_err(ClientError::Encode)
    }

    /// Send a request expecting no payload (e.g. dispatchers).
    pub async fn call_void(&mut self, request: Request) -> Result<(), ClientError> {
        let _ = self.call_raw(request).await?;
        Ok(())
    }

    /// Fetch the rich [`SnapshotRestorePreview`] for the named snapshot.
    /// Thin wrapper over `call::<SnapshotRestorePreview>(SnapshotPreview)`.
    pub async fn snapshot_preview(
        &mut self,
        name: impl Into<String>,
    ) -> Result<SnapshotRestorePreview, ClientError> {
        self.call(Request::SnapshotPreview { name: name.into() }).await
    }

    async fn call_raw(&mut self, request: Request) -> Result<serde_json::Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let env = RequestEnvelope { id, request };
        let mut line = serde_json::to_string(&env)?;
        line.push('\n');
        self.stream.get_mut().write_all(line.as_bytes()).await?;
        self.stream.get_mut().flush().await?;

        let mut buf = String::new();
        let n = self.stream.read_line(&mut buf).await?;
        if n == 0 {
            return Err(ClientError::Closed);
        }
        let env: ResponseEnvelope = serde_json::from_str(buf.trim_end())?;
        if env.id != id {
            return Err(ClientError::IdMismatch { expected: id, got: env.id });
        }
        match env.response {
            Response::Ok { result } => Ok(result),
            Response::Err { error: RpcError { code, message } } => {
                Err(ClientError::Rpc { code, message })
            }
        }
    }
}
