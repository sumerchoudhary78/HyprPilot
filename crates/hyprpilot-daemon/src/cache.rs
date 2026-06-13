//! In-memory world-model cache, kept warm by Hyprland's `.socket2` event
//! stream so repeated queries don't each pay a control-socket round-trip.
//!
//! ## Why
//!
//! Hyprland is the source of truth, but it pushes every state change over the
//! event socket. A long-lived daemon can therefore *remember* the window
//! model and only re-query when something actually changed — the same way a
//! human operating the WM doesn't re-scan the screen before every action.
//!
//! ## Shape
//!
//! Each query result lives in a [`Cached<T>`] carrying the last value, a
//! `dirty` flag, and the time it was last refreshed. A read returns the cached
//! clone when it's clean and younger than [`MAX_AGE`]; otherwise it re-queries
//! Hyprland, stores the result, and clears the flag. The [`run`] task owns its
//! own [`EventStream`] connection — independent of the rules engine, so the
//! cache works even with no rules file — and flips `dirty` flags as events
//! arrive. It applies *every* event unconditionally; unlike the rules engine
//! it has no reentrance guard, so self-induced changes still invalidate.
//!
//! ## What is and isn't cached
//!
//! - Cached + event-invalidated: clients, workspaces, monitors, active
//!   workspace, binds.
//! - Derived, never stored separately: the active window is whichever client
//!   has `focusHistoryID == 0`, so it is always consistent with `clients`.
//! - Cached once, never expired: `version` (constant for a Hyprland session).
//! - Never cached: cursor position — it moves continuously with no
//!   corresponding event, so [`crate::server`] queries it live.
//!
//! ## Staleness bounds
//!
//! Event coverage is complete for the mutations we parse (see
//! [`hyprpilot_core::events`]). [`MAX_AGE`] is a belt-and-suspenders against an
//! event we don't recognize: even with no matching event, a value older than
//! `MAX_AGE` is treated as dirty and re-queried. Geometry is the reason events
//! alone are insufficient — the event socket never carries window `at`/`size`,
//! so structural events mark `clients` dirty and the next read re-queries the
//! full geometry.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tracing::{debug, trace, warn};

use hyprpilot_core::types::{ActiveWorkspace, Bind, Client, Monitor, Version, Workspace};
use hyprpilot_core::{Connection, Error as CoreError, Event, EventStream};

/// Values older than this are re-queried even if no event marked them dirty.
/// A safety net for events we don't parse, not the primary invalidation path.
const MAX_AGE: Duration = Duration::from_secs(5);

struct Cached<T> {
    value: Option<T>,
    dirty: bool,
    refreshed_at: Option<Instant>,
}

impl<T> Default for Cached<T> {
    fn default() -> Self {
        // Start dirty so the first read fetches.
        Self { value: None, dirty: true, refreshed_at: None }
    }
}

impl<T> Cached<T> {
    fn set(&mut self, value: T) {
        self.value = Some(value);
        self.dirty = false;
        self.refreshed_at = Some(Instant::now());
    }
}

impl<T: Clone> Cached<T> {
    /// Cloned value if it is present, clean, and younger than [`MAX_AGE`].
    fn get(&self) -> Option<T> {
        if self.dirty {
            return None;
        }
        match (&self.value, self.refreshed_at) {
            (Some(v), Some(at)) if at.elapsed() < MAX_AGE => Some(v.clone()),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Inner {
    clients: Cached<Vec<Client>>,
    workspaces: Cached<Vec<Workspace>>,
    monitors: Cached<Vec<Monitor>>,
    active_workspace: Cached<ActiveWorkspace>,
    binds: Cached<Vec<Bind>>,
    version: Cached<Version>,
}

/// Cheap-to-clone handle to the shared world model.
#[derive(Clone)]
pub struct StateCache {
    conn: Connection,
    inner: Arc<RwLock<Inner>>,
}

impl StateCache {
    pub fn new(conn: Connection) -> Self {
        Self { conn, inner: Arc::new(RwLock::new(Inner::default())) }
    }

    // ---- reads (refresh-on-demand) ------------------------------------

    pub async fn clients(&self) -> Result<Vec<Client>, CoreError> {
        if let Some(v) = self.read(|i| &i.clients) {
            return Ok(v);
        }
        let v = self.conn.clients().await?;
        self.write(|i| &mut i.clients, v.clone());
        Ok(v)
    }

    pub async fn workspaces(&self) -> Result<Vec<Workspace>, CoreError> {
        if let Some(v) = self.read(|i| &i.workspaces) {
            return Ok(v);
        }
        let v = self.conn.workspaces().await?;
        self.write(|i| &mut i.workspaces, v.clone());
        Ok(v)
    }

    pub async fn monitors(&self) -> Result<Vec<Monitor>, CoreError> {
        if let Some(v) = self.read(|i| &i.monitors) {
            return Ok(v);
        }
        let v = self.conn.monitors().await?;
        self.write(|i| &mut i.monitors, v.clone());
        Ok(v)
    }

    pub async fn active_workspace(&self) -> Result<ActiveWorkspace, CoreError> {
        if let Some(v) = self.read(|i| &i.active_workspace) {
            return Ok(v);
        }
        let v = self.conn.active_workspace().await?;
        self.write(|i| &mut i.active_workspace, v.clone());
        Ok(v)
    }

    pub async fn binds(&self) -> Result<Vec<Bind>, CoreError> {
        if let Some(v) = self.read(|i| &i.binds) {
            return Ok(v);
        }
        let v = self.conn.binds().await?;
        self.write(|i| &mut i.binds, v.clone());
        Ok(v)
    }

    /// Version is constant for a session: fetched once, never expired.
    pub async fn version(&self) -> Result<Version, CoreError> {
        {
            let g = self.inner.read().expect("cache lock poisoned");
            if let Some(v) = &g.version.value {
                return Ok(v.clone());
            }
        }
        let v = self.conn.version().await?;
        self.write(|i| &mut i.version, v.clone());
        Ok(v)
    }

    /// The active window is whichever client has `focusHistoryID == 0`,
    /// derived from [`Self::clients`] so it is always consistent with the
    /// cached client list. `None` when no window is focused.
    pub async fn active_window(&self) -> Result<Option<Client>, CoreError> {
        Ok(active_from(self.clients().await?))
    }

    // ---- lock helpers --------------------------------------------------

    fn read<T: Clone>(&self, pick: impl Fn(&Inner) -> &Cached<T>) -> Option<T> {
        let g = self.inner.read().expect("cache lock poisoned");
        pick(&g).get()
    }

    fn write<T>(&self, pick: impl Fn(&mut Inner) -> &mut Cached<T>, value: T) {
        let mut g = self.inner.write().expect("cache lock poisoned");
        pick(&mut g).set(value);
    }

    /// Flip dirty flags (and apply cheap in-place updates) for one event.
    fn apply(&self, ev: &Event) {
        let mut g = self.inner.write().expect("cache lock poisoned");
        apply_event(&mut g, ev);
    }
}

/// The focused window: `focusHistoryID == 0`. `None` if nothing is focused.
fn active_from(clients: Vec<Client>) -> Option<Client> {
    clients.into_iter().find(|c| c.focus_history_id == 0)
}

/// Pure event → cache-invalidation logic, separated from locking so it can be
/// unit-tested without a live Hyprland connection.
fn apply_event(inner: &mut Inner, ev: &Event) {
    match ev.kind.as_str() {
        // Window count / geometry / fullscreen state changed.
        "openwindow" | "closewindow" | "fullscreen" | "movewindow" | "movewindowv2" => {
            inner.clients.dirty = true;
            inner.workspaces.dirty = true;
            inner.active_workspace.dirty = true;
        }
        // Focus order changed → the focusHistoryID==0 derivation must refetch.
        "activewindow" | "activewindowv2" => {
            inner.clients.dirty = true;
        }
        // Title changes are frequent (terminals, browsers). When the v2 event
        // carries address+title, patch it in place so a cosmetic change
        // doesn't force a full client re-query; otherwise mark dirty.
        "windowtitlev2" => match (ev.field("address"), ev.field("title")) {
            (Some(addr), Some(title)) => {
                if let Some(list) = inner.clients.value.as_mut() {
                    if let Some(c) = list.iter_mut().find(|c| c.address == addr) {
                        c.title = title.to_string();
                    }
                }
            }
            _ => inner.clients.dirty = true,
        },
        "windowtitle" => {
            inner.clients.dirty = true;
        }
        // Workspace switch: counts + active workspace + focus all move.
        "workspace" | "workspacev2" => {
            inner.workspaces.dirty = true;
            inner.active_workspace.dirty = true;
            inner.clients.dirty = true;
        }
        "focusedmon" => {
            inner.monitors.dirty = true;
            inner.active_workspace.dirty = true;
            inner.clients.dirty = true;
        }
        "changefloatingmode" | "pin" => {
            inner.clients.dirty = true;
        }
        "monitoradded" | "monitorremoved" | "monitoraddedv2" => {
            inner.monitors.dirty = true;
            inner.workspaces.dirty = true;
            inner.clients.dirty = true;
        }
        "configreloaded" => {
            inner.binds.dirty = true;
            inner.monitors.dirty = true;
        }
        // urgent, submap, layer/screencast events, … carry no cached field.
        _ => {}
    }
}

/// Seed the cache, then keep it warm from the event socket until the stream
/// closes (Hyprland shutdown). Best-effort: a failed seed or a transient query
/// error just leaves the affected value dirty for the next read.
pub async fn run(cache: StateCache) -> anyhow::Result<()> {
    // Warm the common reads so the first client request is instant.
    seed(&cache).await;

    let mut stream = EventStream::connect(&cache.conn)
        .await
        .map_err(|e| anyhow::anyhow!("cache: connect to event socket: {e}"))?;
    debug!("state cache: event subscription live");

    loop {
        match stream.next().await {
            Ok(Some(raw)) => {
                let ev = Event::from_raw(raw);
                trace!(kind = %ev.kind, "cache event");
                cache.apply(&ev);
            }
            Ok(None) => {
                warn!("cache: event socket closed by Hyprland; cache task exiting");
                return Ok(());
            }
            Err(e) => {
                warn!(error = %e, "cache: event socket read failed; cache task exiting");
                return Ok(());
            }
        }
    }
}

async fn seed(cache: &StateCache) {
    if let Err(e) = cache.clients().await {
        debug!(error = %e, "cache seed: clients");
    }
    if let Err(e) = cache.workspaces().await {
        debug!(error = %e, "cache seed: workspaces");
    }
    if let Err(e) = cache.monitors().await {
        debug!(error = %e, "cache seed: monitors");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprpilot_core::events::RawEvent;

    fn ev(name: &str, data: &str) -> Event {
        Event::from_raw(RawEvent { name: name.to_string(), data: data.to_string() })
    }

    fn client(addr: &str, fh: i32, title: &str) -> Client {
        serde_json::from_value(serde_json::json!({
            "address": addr,
            "mapped": true,
            "at": [0, 0],
            "size": [100, 100],
            "workspace": { "id": 1, "name": "1" },
            "floating": false,
            "monitor": 0,
            "class": "test",
            "title": title,
            "pid": 1,
            "xwayland": false,
            "focusHistoryID": fh,
        }))
        .expect("valid client fixture")
    }

    #[test]
    fn cached_starts_dirty() {
        let c: Cached<i32> = Cached::default();
        assert!(c.dirty);
        assert_eq!(c.get(), None);
    }

    #[test]
    fn cached_get_after_set_is_fresh() {
        let mut c: Cached<i32> = Cached::default();
        c.set(7);
        assert!(!c.dirty);
        assert_eq!(c.get(), Some(7));
    }

    #[test]
    fn cached_dirty_overrides_value() {
        let mut c: Cached<i32> = Cached::default();
        c.set(7);
        c.dirty = true;
        assert_eq!(c.get(), None, "dirty must suppress a present value");
    }

    #[test]
    fn cached_expires_by_age() {
        let mut c: Cached<i32> = Cached::default();
        c.set(7);
        // Backdate beyond MAX_AGE.
        c.refreshed_at = Some(Instant::now() - (MAX_AGE + Duration::from_secs(1)));
        assert_eq!(c.get(), None, "stale value must be treated as a miss");
    }

    #[test]
    fn open_and_close_window_dirty_structure() {
        for kind in ["openwindow", "closewindow", "fullscreen", "movewindowv2"] {
            let mut inner = Inner::default();
            // Mark clean first so we can observe the flip.
            inner.clients.set(vec![]);
            inner.workspaces.set(vec![]);
            inner.active_workspace.dirty = false;
            apply_event(&mut inner, &ev(kind, "0xabc,1,kitty,t"));
            assert!(inner.clients.dirty, "{kind} should dirty clients");
            assert!(inner.workspaces.dirty, "{kind} should dirty workspaces");
            assert!(inner.active_workspace.dirty, "{kind} should dirty active_workspace");
        }
    }

    #[test]
    fn focus_change_dirties_only_clients() {
        let mut inner = Inner::default();
        inner.clients.set(vec![]);
        inner.monitors.set(vec![]);
        apply_event(&mut inner, &ev("activewindowv2", "abc"));
        assert!(inner.clients.dirty, "focus change must refetch for focusHistoryID");
        assert!(!inner.monitors.dirty, "focus change must not touch monitors");
    }

    #[test]
    fn windowtitlev2_patches_in_place_without_dirtying() {
        let mut inner = Inner::default();
        inner.clients.set(vec![client("0xabc", 0, "old"), client("0xdef", 1, "other")]);
        assert!(!inner.clients.dirty);
        apply_event(&mut inner, &ev("windowtitlev2", "abc,New Title"));
        assert!(!inner.clients.dirty, "in-place title patch must not force a refetch");
        let list = inner.clients.value.as_ref().unwrap();
        assert_eq!(list[0].title, "New Title");
        assert_eq!(list[1].title, "other", "only the matching window changes");
    }

    #[test]
    fn windowtitle_v1_dirties_clients() {
        let mut inner = Inner::default();
        inner.clients.set(vec![client("0xabc", 0, "t")]);
        apply_event(&mut inner, &ev("windowtitle", "abc"));
        assert!(inner.clients.dirty, "v1 title event lacks the title, so refetch");
    }

    #[test]
    fn configreloaded_dirties_binds_and_monitors() {
        let mut inner = Inner::default();
        inner.binds.set(vec![]);
        inner.monitors.set(vec![]);
        inner.clients.set(vec![]);
        apply_event(&mut inner, &ev("configreloaded", ""));
        assert!(inner.binds.dirty);
        assert!(inner.monitors.dirty);
        assert!(!inner.clients.dirty, "config reload should not invalidate clients");
    }

    #[test]
    fn unknown_event_leaves_cache_alone() {
        let mut inner = Inner::default();
        inner.clients.set(vec![]);
        inner.monitors.set(vec![]);
        apply_event(&mut inner, &ev("urgent", "abc"));
        apply_event(&mut inner, &ev("submap", "resize"));
        assert!(!inner.clients.dirty);
        assert!(!inner.monitors.dirty);
    }

    #[test]
    fn active_from_picks_focus_history_zero() {
        let clients = vec![client("0xa", 2, "a"), client("0xb", 0, "b"), client("0xc", 1, "c")];
        assert_eq!(active_from(clients).unwrap().address, "0xb");
    }

    #[test]
    fn active_from_none_when_unfocused() {
        let clients = vec![client("0xa", 2, "a"), client("0xb", 1, "b")];
        assert!(active_from(clients).is_none());
    }
}
