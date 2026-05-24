//! Push event stream from `.socket2.sock`.
//!
//! Hyprland writes one event per newline-terminated line in the form
//! `name>>data`. Some events have a v2 variant (e.g. `windowtitlev2`) that
//! prefixes the data with the window address.
//!
//! Two layers are provided:
//!
//! - [`RawEvent`] / [`EventStream::next`] — the raw line, just split into
//!   name and payload.
//! - [`Event`] / [`Event::from_raw`] — a typed view with `kind` and a
//!   [`BTreeMap`] of structured `fields` extracted from the payload. Field
//!   names are stable across Hyprland versions for the events we recognize.
//!
//! Unknown event kinds are preserved as `Event { kind, fields: {}, raw }`
//! so callers can still inspect the original line.
//!
//! Window addresses from the event socket arrive without the `0x` prefix
//! that [`crate::types::Client::address`] uses. We normalize on parse so
//! consumers can compare addresses across event-stream and query-API
//! origins.

use std::collections::BTreeMap;

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

// =============================================================================
// Typed event view
// =============================================================================

/// A parsed Hyprland event: its `kind` (event name) and the structured
/// `fields` we recognize. The original [`RawEvent`] is preserved.
///
/// `fields` keys are stable per `kind`. Common shapes:
///
/// | Kind                 | Fields                                                |
/// |----------------------|-------------------------------------------------------|
/// | `openwindow`         | `address`, `workspace`, `class`, `title`              |
/// | `closewindow`        | `address`                                             |
/// | `movewindow`         | `address`, `workspace`                                |
/// | `movewindowv2`       | `address`, `workspace_id`, `workspace`                |
/// | `windowtitle`        | `address`                                             |
/// | `windowtitlev2`      | `address`, `title`                                    |
/// | `activewindow`       | `class`, `title`                                      |
/// | `activewindowv2`     | `address`                                             |
/// | `workspace`          | `name`                                                |
/// | `workspacev2`        | `id`, `name`                                          |
/// | `focusedmon`         | `monitor`, `workspace`                                |
/// | `fullscreen`         | `state` (`"0"` or `"1"`)                              |
/// | `changefloatingmode` | `address`, `floating` (`"0"` or `"1"`)                |
/// | `monitoradded`       | `name`                                                |
/// | `monitorremoved`     | `name`                                                |
/// | `monitoraddedv2`     | `id`, `name`, `description`                           |
/// | `submap`             | `name`                                                |
/// | `pin`                | `address`, `state`                                    |
/// | `urgent`             | `address`                                             |
/// | `configreloaded`     | (no fields)                                           |
///
/// Anything else: `fields` is empty, `raw` carries the original payload
/// for caller inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub kind: String,
    pub fields: BTreeMap<String, String>,
    pub raw: RawEvent,
}

impl Event {
    pub fn from_raw(raw: RawEvent) -> Self {
        let fields = extract_fields(&raw.name, &raw.data);
        Self { kind: raw.name.clone(), fields, raw }
    }

    /// Shortcut: get a field value by key.
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
}

fn extract_fields(name: &str, data: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    if data.is_empty() {
        return fields;
    }
    // Split eagerly into parts. Some events carry titles/descriptions that
    // may contain commas; we rejoin the tail for those by counting expected
    // leading fields.
    let parts: Vec<&str> = data.split(',').collect();

    match name {
        "openwindow" if parts.len() >= 4 => {
            fields.insert("address".into(), normalize_addr(parts[0]));
            fields.insert("workspace".into(), parts[1].to_string());
            fields.insert("class".into(), parts[2].to_string());
            fields.insert("title".into(), parts[3..].join(","));
        }
        "closewindow" => {
            fields.insert("address".into(), normalize_addr(parts[0]));
        }
        "movewindow" if parts.len() >= 2 => {
            fields.insert("address".into(), normalize_addr(parts[0]));
            fields.insert("workspace".into(), parts[1].to_string());
        }
        "movewindowv2" if parts.len() >= 3 => {
            fields.insert("address".into(), normalize_addr(parts[0]));
            fields.insert("workspace_id".into(), parts[1].to_string());
            fields.insert("workspace".into(), parts[2].to_string());
        }
        "windowtitle" => {
            fields.insert("address".into(), normalize_addr(parts[0]));
        }
        "windowtitlev2" if parts.len() >= 2 => {
            fields.insert("address".into(), normalize_addr(parts[0]));
            fields.insert("title".into(), parts[1..].join(","));
        }
        "activewindow" if parts.len() >= 2 => {
            fields.insert("class".into(), parts[0].to_string());
            fields.insert("title".into(), parts[1..].join(","));
        }
        "activewindowv2" => {
            fields.insert("address".into(), normalize_addr(parts[0]));
        }
        "workspace" => {
            fields.insert("name".into(), parts[0].to_string());
        }
        "workspacev2" if parts.len() >= 2 => {
            fields.insert("id".into(), parts[0].to_string());
            fields.insert("name".into(), parts[1].to_string());
        }
        "focusedmon" if parts.len() >= 2 => {
            fields.insert("monitor".into(), parts[0].to_string());
            fields.insert("workspace".into(), parts[1].to_string());
        }
        "fullscreen" => {
            fields.insert("state".into(), parts[0].to_string());
        }
        "changefloatingmode" if parts.len() >= 2 => {
            fields.insert("address".into(), normalize_addr(parts[0]));
            fields.insert("floating".into(), parts[1].to_string());
        }
        "monitoradded" | "monitorremoved" => {
            fields.insert("name".into(), parts[0].to_string());
        }
        "monitoraddedv2" if parts.len() >= 2 => {
            fields.insert("id".into(), parts[0].to_string());
            fields.insert("name".into(), parts[1].to_string());
            if parts.len() >= 3 {
                fields.insert("description".into(), parts[2..].join(","));
            }
        }
        "submap" => {
            fields.insert("name".into(), parts[0].to_string());
        }
        "pin" if parts.len() >= 2 => {
            fields.insert("address".into(), normalize_addr(parts[0]));
            fields.insert("state".into(), parts[1].to_string());
        }
        "urgent" => {
            fields.insert("address".into(), normalize_addr(parts[0]));
        }
        _ => {}
    }

    fields
}

/// Hyprland sends event-socket addresses without the `0x` prefix that the
/// query API uses. Normalize so consumers can compare across the two.
fn normalize_addr(s: &str) -> String {
    if s.starts_with("0x") {
        s.to_string()
    } else {
        format!("0x{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, data: &str) -> RawEvent {
        RawEvent { name: name.to_string(), data: data.to_string() }
    }

    #[test]
    fn parses_openwindow() {
        let e = Event::from_raw(raw(
            "openwindow",
            "56216dce3400,2,kitty,Edit src/main.rs",
        ));
        assert_eq!(e.kind, "openwindow");
        assert_eq!(e.field("address"), Some("0x56216dce3400"));
        assert_eq!(e.field("workspace"), Some("2"));
        assert_eq!(e.field("class"), Some("kitty"));
        assert_eq!(e.field("title"), Some("Edit src/main.rs"));
    }

    #[test]
    fn openwindow_title_with_commas_is_rejoined() {
        let e = Event::from_raw(raw(
            "openwindow",
            "0xabc,3,foo,A title, with commas, in it",
        ));
        assert_eq!(e.field("title"), Some("A title, with commas, in it"));
    }

    #[test]
    fn address_normalized_without_prefix() {
        let e = Event::from_raw(raw("closewindow", "56216dce3400"));
        assert_eq!(e.field("address"), Some("0x56216dce3400"));
    }

    #[test]
    fn address_left_alone_if_already_prefixed() {
        let e = Event::from_raw(raw("closewindow", "0x56216dce3400"));
        assert_eq!(e.field("address"), Some("0x56216dce3400"));
    }

    #[test]
    fn parses_workspace_and_workspacev2() {
        let e = Event::from_raw(raw("workspace", "3"));
        assert_eq!(e.field("name"), Some("3"));

        let e = Event::from_raw(raw("workspacev2", "3,3"));
        assert_eq!(e.field("id"), Some("3"));
        assert_eq!(e.field("name"), Some("3"));
    }

    #[test]
    fn parses_activewindow_with_comma_in_title() {
        let e = Event::from_raw(raw("activewindow", "kitty,Edit, save, run"));
        assert_eq!(e.field("class"), Some("kitty"));
        assert_eq!(e.field("title"), Some("Edit, save, run"));
    }

    #[test]
    fn parses_movewindowv2() {
        let e = Event::from_raw(raw("movewindowv2", "abc,3,3"));
        assert_eq!(e.field("address"), Some("0xabc"));
        assert_eq!(e.field("workspace_id"), Some("3"));
        assert_eq!(e.field("workspace"), Some("3"));
    }

    #[test]
    fn parses_changefloatingmode() {
        let e = Event::from_raw(raw("changefloatingmode", "abc,1"));
        assert_eq!(e.field("address"), Some("0xabc"));
        assert_eq!(e.field("floating"), Some("1"));
    }

    #[test]
    fn unknown_event_preserves_raw() {
        let e = Event::from_raw(raw("brand_new_event", "some,data,here"));
        assert!(e.fields.is_empty());
        assert_eq!(e.raw.name, "brand_new_event");
        assert_eq!(e.raw.data, "some,data,here");
    }

    #[test]
    fn configreloaded_has_no_fields() {
        let e = Event::from_raw(raw("configreloaded", ""));
        assert!(e.fields.is_empty());
    }
}
