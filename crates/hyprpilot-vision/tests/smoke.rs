//! Integration smoke test: capture a region from the live compositor.
//!
//! Skipped when `grim` is not installed or no Wayland display is available,
//! so cargo test still works on non-Wayland CI hosts.

use hyprpilot_vision::{BackendAvailability, GrimCapture, ImageFormat, Region};

fn skippable() -> bool {
    !BackendAvailability::detect().has_grim() || std::env::var_os("WAYLAND_DISPLAY").is_none()
}

#[tokio::test]
async fn capture_small_region_is_a_png() {
    if skippable() {
        eprintln!("skip: grim missing or no WAYLAND_DISPLAY");
        return;
    }
    let cap = GrimCapture::detect().expect("detect grim");
    let bytes = cap
        .region(Region { x: 0, y: 0, w: 64, h: 64 }, ImageFormat::Png)
        .await
        .expect("region capture");
    // PNG magic: 89 50 4E 47 0D 0A 1A 0A.
    assert!(bytes.len() > 8, "image too small: {} bytes", bytes.len());
    assert_eq!(
        &bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "first 8 bytes are not PNG magic"
    );
}

#[tokio::test]
async fn invalid_region_is_typed_error() {
    if skippable() {
        eprintln!("skip: grim missing or no WAYLAND_DISPLAY");
        return;
    }
    let cap = GrimCapture::detect().expect("detect grim");
    let err = cap
        .region(Region { x: 0, y: 0, w: 0, h: 100 }, ImageFormat::Png)
        .await
        .expect_err("zero-width region must error");
    assert!(matches!(
        err,
        hyprpilot_vision::VisionError::InvalidRegion { .. }
    ));
}

#[tokio::test]
async fn detect_returns_grim_when_present() {
    if !BackendAvailability::detect().has_grim() {
        eprintln!("skip: grim not installed");
        return;
    }
    let cap = GrimCapture::detect();
    assert!(cap.is_ok(), "detect failed despite grim on PATH");
}
