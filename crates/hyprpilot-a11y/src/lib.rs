//! Accessibility (AT-SPI2) tree reading for HyprPilot.
//!
//! The semantic UI of most Linux apps — button labels, field contents, roles,
//! and bounding boxes — is published over D-Bus via AT-SPI2. Reading it is how
//! a screen reader "sees" without pixels, and it is how HyprPilot avoids OCR
//! for app content: structured data instead of a screenshot + tesseract pass.
//!
//! ## Flow
//!
//! 1. [`A11y::connect`] attaches to the a11y bus (`org.a11y.Bus`).
//! 2. Given a process id (HyprPilot already knows the focused window's pid from
//!    its world-model cache), [`A11y::find`] / [`A11y::snapshot_app`] locate the
//!    matching application accessible by asking the bus for each app's pid, then
//!    walk that app's subtree into a flat [`Element`] list.
//! 3. Extents are **window-relative** ([`atspi::CoordType::Window`]); the caller
//!    turns them into a click target by adding the window's on-screen origin
//!    (from Hyprland) and the monitor scale. See [`element`].
//!
//! ## Limits (honest scope)
//!
//! - Coverage is toolkit-dependent: GTK/Qt/Firefox expose rich trees; Electron
//!   needs `--force-renderer-accessibility`; terminals, games, and canvas/WebGL
//!   expose little or nothing. Vision remains the fallback for those.
//! - The walk is client-side and bounded ([`WalkOpts`]); very large trees are
//!   truncated (logged). The AT-SPI `Collection` interface offers server-side
//!   queries and is the eventual optimization for big apps.

pub mod element;
pub mod error;

pub use element::{filter_elements, matches, Element};
pub use error::{A11yError, Result};

use tracing::{trace, warn};

use atspi::connection::AccessibilityConnection;
use atspi::object_ref::ObjectRefOwned;
use atspi::proxy::accessible::ObjectRefExt;
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::zbus::fdo::DBusProxy;
use atspi::zbus::names::BusName;
use atspi::CoordType;

/// Bounds on a tree walk so a huge app (e.g. a browser tab) can't stall the
/// daemon. Defaults: 1500 nodes, depth 50.
#[derive(Debug, Clone, Copy)]
pub struct WalkOpts {
    /// Stop after collecting this many nodes (truncation is logged).
    pub max_nodes: usize,
    /// Don't descend past this depth below the application root.
    pub max_depth: u32,
}

impl Default for WalkOpts {
    fn default() -> Self {
        Self { max_nodes: 1500, max_depth: 50 }
    }
}

/// Inline text content longer than this is dropped (a node's `name` usually
/// carries the useful label anyway; full document text belongs to OCR/other).
const TEXT_LIMIT: i32 = 200;

/// A live handle to the accessibility bus. Cheap to clone.
#[derive(Clone)]
pub struct A11y {
    conn: AccessibilityConnection,
}

impl A11y {
    /// Connect to the a11y bus. Errors as [`A11yError::Unavailable`] when the
    /// bus isn't running or accessibility is off in the session.
    pub async fn connect() -> Result<Self> {
        let conn = AccessibilityConnection::new()
            .await
            .map_err(|e| A11yError::Unavailable(e.to_string()))?;
        Ok(Self { conn })
    }

    /// Walk the accessibility subtree of the application owning `pid` into a
    /// flat, depth-ordered list (application root first).
    pub async fn snapshot_app(&self, pid: i32, opts: WalkOpts) -> Result<Vec<Element>> {
        let app = self.find_app(pid).await?;
        self.walk(app, opts).await
    }

    /// [`Self::snapshot_app`] followed by [`filter_elements`].
    pub async fn find(
        &self,
        pid: i32,
        query: &str,
        role: Option<&str>,
        opts: WalkOpts,
    ) -> Result<Vec<Element>> {
        let all = self.snapshot_app(pid, opts).await?;
        Ok(filter_elements(&all, query, role))
    }

    /// Find the application root accessible whose D-Bus connection belongs to
    /// `pid`. AT-SPI apps register on the a11y bus; we ask the bus for each
    /// app's unix pid and match.
    async fn find_app(&self, pid: i32) -> Result<ObjectRefOwned> {
        let root = self
            .conn
            .root_accessible_on_registry()
            .await
            .map_err(|e| A11yError::Bus(e.to_string()))?;
        let children = root
            .get_children()
            .await
            .map_err(|e| A11yError::Bus(e.to_string()))?;
        let dbus = DBusProxy::new(self.conn.connection())
            .await
            .map_err(|e| A11yError::Unavailable(e.to_string()))?;
        for child in children {
            let Some(uname) = child.name() else { continue };
            let bus_name: BusName = uname.clone().into();
            if let Ok(p) = dbus.get_connection_unix_process_id(bus_name).await {
                if p as i32 == pid {
                    return Ok(child);
                }
            }
        }
        Err(A11yError::NoApplication(Some(pid)))
    }

    /// Iterative (non-recursive) DFS over the subtree rooted at `root`.
    async fn walk(&self, root: ObjectRefOwned, opts: WalkOpts) -> Result<Vec<Element>> {
        let conn = self.conn.connection();
        let mut out: Vec<Element> = Vec::new();
        let mut stack: Vec<(ObjectRefOwned, u32)> = vec![(root, 0)];
        let mut truncated = false;

        while let Some((objref, depth)) = stack.pop() {
            if out.len() >= opts.max_nodes {
                truncated = true;
                break;
            }
            if depth > opts.max_depth || objref.is_null() {
                continue;
            }
            let path = objref.path_as_str().to_string();
            let acc = match objref.as_accessible_proxy(conn).await {
                Ok(a) => a,
                Err(e) => {
                    trace!(%path, error = %e, "a11y: skip node (proxy build failed)");
                    continue;
                }
            };

            let role = acc
                .get_role()
                .await
                .map(|r| r.name().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            let name = acc.name().await.unwrap_or_default();

            let (mut x, mut y, mut w, mut h) = (0, 0, 0, 0);
            let mut text = None;
            if let Ok(proxies) = acc.proxies().await {
                if let Ok(component) = proxies.component().await {
                    if let Ok((ex, ey, ew, eh)) = component.get_extents(CoordType::Window).await {
                        x = ex;
                        y = ey;
                        w = ew;
                        h = eh;
                    }
                }
                if let Ok(text_iface) = proxies.text().await {
                    if let Ok(n) = text_iface.character_count().await {
                        if n > 0 && n <= TEXT_LIMIT {
                            if let Ok(s) = text_iface.get_text(0, n).await {
                                let s = s.trim();
                                if !s.is_empty() {
                                    text = Some(s.to_string());
                                }
                            }
                        }
                    }
                }
            }

            out.push(Element { role, name, text, x, y, w, h, depth, path });

            if let Ok(children) = acc.get_children().await {
                // Push reversed so siblings pop in natural (document) order.
                for child in children.into_iter().rev() {
                    stack.push((child, depth + 1));
                }
            }
        }

        if truncated {
            warn!(cap = opts.max_nodes, "a11y walk hit the node cap; tree larger than the cap");
        }
        Ok(out)
    }
}
