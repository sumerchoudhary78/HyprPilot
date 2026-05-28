//! Native Wayland mouse transport via `wlr-virtual-pointer-unstable-v1`.
//!
//! Replaces the ydotool path for mouse motion/clicks. Hyprland (and any
//! wlroots-based compositor) accepts `zwlr_virtual_pointer_v1` requests
//! directly into the compositor input pipeline — libinput's pointer
//! acceleration on synthetic devices never enters the picture. That's the
//! root cause of issue #25's 2x drift, and this transport closes it.
//!
//! ## Threading
//!
//! `wayland-client` is synchronous, and the event queue must be driven
//! from the thread that owns the connection. We park a single OS thread
//! on `Connection` + `EventQueue`, and the async-facing [`VirtualPointer`]
//! handle pushes work to it over an mpsc channel and awaits a oneshot
//! reply. Output geometry (used as the absolute-motion extent) is kept
//! current by pumping queued `wl_output` events on every command.
//!
//! ## Lifecycle
//!
//! [`VirtualPointer::try_new`] is best-effort: if the compositor doesn't
//! advertise the protocol, or `$WAYLAND_DISPLAY` is unset, it logs a
//! warning and returns `None`. Callers fall back to the legacy
//! ydotool path in that case.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use tokio::sync::oneshot;
use tracing::{debug, warn};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_output, wl_pointer, wl_registry, wl_seat},
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::error::{InputError, Result};
use crate::keys::MouseButton;

// ---- evdev button codes ---------------------------------------------------

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113; // X1 / back
const BTN_EXTRA: u32 = 0x114; // X2 / forward

pub(crate) fn evdev_button(b: MouseButton) -> u32 {
    match b {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::X1 => BTN_SIDE,
        MouseButton::X2 => BTN_EXTRA,
    }
}

// ---- output geometry ------------------------------------------------------

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputGeom {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Union bounding box of all advertised outputs, used as the
/// `(x_extent, y_extent)` argument to `motion_absolute`. Always at least
/// `(1, 1)` so the protocol never sees a zero extent.
pub(crate) fn union_extents<'a, I: IntoIterator<Item = &'a OutputGeom>>(outs: I) -> (u32, u32) {
    let mut max_x = 0i32;
    let mut max_y = 0i32;
    for o in outs {
        max_x = max_x.max(o.x.saturating_add(o.width));
        max_y = max_y.max(o.y.saturating_add(o.height));
    }
    (max_x.max(1) as u32, max_y.max(1) as u32)
}

// ---- command protocol -----------------------------------------------------

enum Cmd {
    MoveAbs {
        x: i32,
        y: i32,
        reply: oneshot::Sender<Result<()>>,
    },
    MoveRel {
        dx: i32,
        dy: i32,
        reply: oneshot::Sender<Result<()>>,
    },
    Click {
        button: u32,
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown,
}

/// Async-facing handle. Cheap to clone-by-Arc if needed; held by
/// `InputRunner` for the daemon's life.
pub struct VirtualPointer {
    tx: mpsc::Sender<Cmd>,
}

impl VirtualPointer {
    /// Try to connect to the compositor and bind the protocol. Returns
    /// `None` on any failure (no Wayland display, protocol unsupported,
    /// no seat) — caller logs and falls back.
    pub fn try_new() -> Option<Self> {
        let (init_tx, init_rx) = mpsc::channel::<std::result::Result<mpsc::Sender<Cmd>, String>>();
        let spawn = thread::Builder::new()
            .name("hyprpilot-vp".into())
            .spawn(move || run_worker(init_tx));
        if let Err(e) = spawn {
            warn!(error = %e, "failed to spawn virtual-pointer worker thread");
            return None;
        }
        match init_rx.recv() {
            Ok(Ok(tx)) => Some(Self { tx }),
            Ok(Err(reason)) => {
                warn!(
                    reason = %reason,
                    "wlr-virtual-pointer-v1 unavailable; mouse will fall back to ydotool"
                );
                None
            }
            Err(_) => {
                warn!("virtual-pointer worker died before reporting readiness");
                None
            }
        }
    }

    pub async fn move_to(&self, x: i32, y: i32) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::MoveAbs { x, y, reply: tx })
            .map_err(|_| InputError::VirtualPointer("worker thread is gone".into()))?;
        rx.await
            .map_err(|_| InputError::VirtualPointer("worker dropped reply channel".into()))?
    }

    pub async fn move_by(&self, dx: i32, dy: i32) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::MoveRel { dx, dy, reply: tx })
            .map_err(|_| InputError::VirtualPointer("worker thread is gone".into()))?;
        rx.await
            .map_err(|_| InputError::VirtualPointer("worker dropped reply channel".into()))?
    }

    pub async fn click(&self, button: MouseButton) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Click {
                button: evdev_button(button),
                reply: tx,
            })
            .map_err(|_| InputError::VirtualPointer("worker thread is gone".into()))?;
        rx.await
            .map_err(|_| InputError::VirtualPointer("worker dropped reply channel".into()))?
    }
}

impl Drop for VirtualPointer {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

// ---- worker ---------------------------------------------------------------

struct State {
    pointer: ZwlrVirtualPointerV1,
    /// Per-output geometry, keyed by `WlOutput` proxy. Updated from
    /// `wl_output` events; flushed into `extents` whenever we read it.
    outputs: HashMap<wl_output::WlOutput, OutputGeom>,
    /// Monotonically increasing event timestamp (ms). Some compositors
    /// require strict monotonicity within a virtual pointer's lifetime.
    time_counter: u32,
}

impl State {
    fn next_time(&mut self) -> u32 {
        // Saturate at u32::MAX. Wrapping would only matter after ~49.7
        // days of continuous use and only if the compositor cares; for
        // our purposes saturate-and-pin is fine.
        self.time_counter = self.time_counter.saturating_add(1);
        self.time_counter
    }

    fn extents(&self) -> (u32, u32) {
        union_extents(self.outputs.values())
    }
}

fn run_worker(init_tx: mpsc::Sender<std::result::Result<mpsc::Sender<Cmd>, String>>) {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            let _ = init_tx.send(Err(format!("wayland connect: {e}")));
            return;
        }
    };

    let (globals, mut queue) = match registry_queue_init::<State>(&conn) {
        Ok(g) => g,
        Err(e) => {
            let _ = init_tx.send(Err(format!("registry init: {e}")));
            return;
        }
    };
    let qh = queue.handle();

    let seat: wl_seat::WlSeat = match globals.bind(&qh, 1..=9, ()) {
        Ok(s) => s,
        Err(e) => {
            let _ = init_tx.send(Err(format!("no wl_seat: {e}")));
            return;
        }
    };

    let manager: ZwlrVirtualPointerManagerV1 = match globals.bind(&qh, 1..=2, ()) {
        Ok(m) => m,
        Err(e) => {
            let _ = init_tx.send(Err(format!(
                "compositor does not advertise zwlr_virtual_pointer_manager_v1: {e}"
            )));
            return;
        }
    };

    let pointer = manager.create_virtual_pointer(Some(&seat), &qh, ());

    // Bind every output so wl_output events flow into State and we can
    // compute extents.
    let mut outputs = HashMap::new();
    globals.contents().with_list(|globs| {
        for g in globs {
            if g.interface == wl_output::WlOutput::interface().name {
                let version = g.version.min(4);
                let out = globals
                    .registry()
                    .bind::<wl_output::WlOutput, _, _>(g.name, version, &qh, ());
                outputs.insert(out, OutputGeom::default());
            }
        }
    });

    let mut state = State {
        pointer,
        outputs,
        time_counter: 0,
    };

    // Roundtrip once to collect initial output geometry/mode events.
    if let Err(e) = queue.roundtrip(&mut state) {
        let _ = init_tx.send(Err(format!("initial roundtrip: {e}")));
        return;
    }
    debug!(extents = ?state.extents(), outputs = state.outputs.len(), "virtual-pointer ready");

    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    if init_tx.send(Ok(cmd_tx)).is_err() {
        // Caller went away before we could hand the channel back; tear
        // down quietly.
        state.pointer.destroy();
        let _ = conn.flush();
        return;
    }

    // Main loop. Each iteration drains pending wayland events (cheap),
    // then blocks until the next command. wl_output changes during idle
    // are picked up before the *next* command, which is the only place
    // we use extents.
    loop {
        let _ = queue.dispatch_pending(&mut state);

        match cmd_rx.recv() {
            Ok(Cmd::Shutdown) | Err(_) => break,
            Ok(Cmd::MoveAbs { x, y, reply }) => {
                let (ex, ey) = state.extents();
                let xx = (x.max(0) as u32).min(ex);
                let yy = (y.max(0) as u32).min(ey);
                let t = state.next_time();
                state.pointer.motion_absolute(t, xx, yy, ex, ey);
                state.pointer.frame();
                let result = roundtrip(&mut queue, &mut state);
                let _ = reply.send(result);
            }
            Ok(Cmd::MoveRel { dx, dy, reply }) => {
                let t = state.next_time();
                state.pointer.motion(t, dx as f64, dy as f64);
                state.pointer.frame();
                let result = roundtrip(&mut queue, &mut state);
                let _ = reply.send(result);
            }
            Ok(Cmd::Click { button, reply }) => {
                let t = state.next_time();
                state
                    .pointer
                    .button(t, button, wl_pointer::ButtonState::Pressed);
                state.pointer.frame();
                let t2 = state.next_time();
                state
                    .pointer
                    .button(t2, button, wl_pointer::ButtonState::Released);
                state.pointer.frame();
                let result = roundtrip(&mut queue, &mut state);
                let _ = reply.send(result);
            }
        }
    }

    state.pointer.destroy();
    let _ = conn.flush();
}

fn roundtrip(
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
) -> Result<()> {
    queue
        .roundtrip(state)
        .map(|_| ())
        .map_err(|e| InputError::VirtualPointer(e.to_string()))
}

// ---- Dispatch impls -------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: <wl_registry::WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // registry_queue_init drives the registry itself; we don't
        // currently react to hot-plug globals.
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: <wl_seat::WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: <wl_output::WlOutput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let entry = state.outputs.entry(proxy.clone()).or_default();
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                entry.x = x;
                entry.y = y;
            }
            wl_output::Event::Mode { width, height, flags, .. } => {
                let is_current = match flags {
                    WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                    _ => false,
                };
                if is_current {
                    entry.width = width;
                    entry.height = height;
                }
            }
            // Done / Scale / Name / Description don't change our state.
            _ => {}
        }
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_button_codes_match_linux_headers() {
        assert_eq!(evdev_button(MouseButton::Left), 0x110);
        assert_eq!(evdev_button(MouseButton::Right), 0x111);
        assert_eq!(evdev_button(MouseButton::Middle), 0x112);
        assert_eq!(evdev_button(MouseButton::X1), 0x113);
        assert_eq!(evdev_button(MouseButton::X2), 0x114);
    }

    #[test]
    fn union_extents_single_monitor_at_origin() {
        let outs = [OutputGeom {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        assert_eq!(union_extents(outs.iter()), (1920, 1080));
    }

    #[test]
    fn union_extents_two_monitors_side_by_side() {
        let outs = [
            OutputGeom {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            OutputGeom {
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
            },
        ];
        assert_eq!(union_extents(outs.iter()), (4480, 1440));
    }

    #[test]
    fn union_extents_stacked_monitors() {
        let outs = [
            OutputGeom {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            OutputGeom {
                x: 0,
                y: 1080,
                width: 1920,
                height: 1080,
            },
        ];
        assert_eq!(union_extents(outs.iter()), (1920, 2160));
    }

    #[test]
    fn union_extents_offset_negative_origin() {
        // Hyprland can place a monitor at negative coords. Union should
        // still return a positive extent matching the rightmost edge.
        let outs = [OutputGeom {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        // Rightmost edge is 0, so extent is clamped to 1.
        assert_eq!(union_extents(outs.iter()), (1, 1080));
    }

    #[test]
    fn union_extents_empty_returns_minimum() {
        let outs: [OutputGeom; 0] = [];
        assert_eq!(union_extents(outs.iter()), (1, 1));
    }
}
