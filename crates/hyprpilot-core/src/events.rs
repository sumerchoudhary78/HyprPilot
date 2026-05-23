//! Push event stream from `.socket2.sock`.
//!
//! Hyprland writes one event per newline-terminated line in the form
//! `name>>data`. Some events have a v2 variant (e.g. `windowtitlev2`) that
//! prefixes the data with the window address.

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;

use crate::error::{Error, Result};
use crate::ipc::Connection;

/// One line off the event socket, split into name and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    pub name: String,
    pub data: String,
}

/// A streaming reader for `.socket2.sock`.
///
/// The compositor pushes asynchronously; iterate with [`EventStream::next`] in
/// a loop. Returns `Ok(None)` only when the compositor closes the connection,
/// which is effectively a Hyprland shutdown.
pub struct EventStream {
    reader: BufReader<UnixStream>,
}

impl EventStream {
    pub async fn connect(conn: &Connection) -> Result<Self> {
        let stream = UnixStream::connect(conn.instance().event_socket())
            .await
            .map_err(Error::Io)?;
        Ok(Self { reader: BufReader::new(stream) })
    }

    pub async fn next(&mut self) -> Result<Option<RawEvent>> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await.map_err(Error::Io)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        let (name, data) = trimmed.split_once(">>").unwrap_or((trimmed, ""));
        Ok(Some(RawEvent { name: name.to_string(), data: data.to_string() }))
    }
}
