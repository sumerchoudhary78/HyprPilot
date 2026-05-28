//! Live integration test for the `wlr-virtual-pointer-v1` transport.
//!
//! `#[ignore]`'d by default — needs a real Hyprland (or other wlroots)
//! session reachable via `$WAYLAND_DISPLAY` plus `hyprctl` on `$PATH`.
//! Run with:
//!
//! ```sh
//! cargo test -p hyprpilot-input --test virtual_pointer_live -- --ignored
//! ```
//!
//! Combined into a single test on purpose: cargo runs `#[tokio::test]`
//! cases in parallel, and two independent `VirtualPointer` instances
//! racing each other corrupts the readback signal we use to verify
//! coordinates. One test = serial by construction.
//!
//! **Safety**: synthesises pointer motion only — no clicks, so nothing
//! can be activated even under focus-follows-mouse. Per
//! `live-input-testing-safety` memory, don't run on a workspace whose
//! contents you care about: the cursor *will* be yanked around for ~1s.

use std::process::Command;

use hyprpilot_input::VirtualPointer;

fn cursorpos() -> (i32, i32) {
    let out = Command::new("hyprctl")
        .arg("cursorpos")
        .output()
        .expect("run hyprctl cursorpos");
    let s = String::from_utf8_lossy(&out.stdout);
    let trimmed = s.trim();
    let (x, y) = trimmed.split_once(',').expect("expected `X, Y` from hyprctl");
    (
        x.trim().parse().expect("parse x"),
        y.trim().parse().expect("parse y"),
    )
}

/// Anchor the regression on issue #25: ydotool's uinput→libinput path
/// applies pointer-accel and lands at ~2x the requested coord. The
/// wlr-virtual-pointer transport bypasses libinput entirely, so the
/// cursor should land within rounding of the request.
#[tokio::test]
#[ignore]
async fn move_absolute_lands_pixel_exact_no_libinput_accel_drift() {
    let vp = VirtualPointer::try_new().expect(
        "wlr-virtual-pointer-v1 unavailable; this test requires a wlroots compositor \
         (and rules out the path that #25 lives on)",
    );

    // Four spread-out targets covering corners of a single-monitor
    // setup. Avoid (0, 0) — some compositors swallow exact-origin moves.
    let targets = [(200, 200), (700, 400), (1000, 500), (500, 800)];

    for target in targets {
        vp.move_to(target.0, target.1).await.expect("move_to");
        // Let the compositor commit a frame before reading back.
        std::thread::sleep(std::time::Duration::from_millis(40));
        let actual = cursorpos();
        let dx = (actual.0 - target.0).abs();
        let dy = (actual.1 - target.1).abs();
        // ±2 px tolerance covers extent→pixel rounding. A 2x drift
        // would manifest as delta ≈ target itself.
        assert!(
            dx <= 2 && dy <= 2,
            "cursor landed at {actual:?}, expected ~{target:?} (delta {dx}, {dy}). \
             Delta near `target` itself means the ydotool fallback is active and \
             issue #25's libinput-accel bug is in effect."
        );
    }
}
