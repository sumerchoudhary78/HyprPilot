//! Integration smoke test: capture a region from the live compositor.
//!
//! Skipped when `grim` is not installed or no Wayland display is available,
//! so cargo test still works on non-Wayland CI hosts.

use hyprpilot_vision::{BackendAvailability, GrimCapture, ImageFormat, Region, TesseractOcr};

fn skippable() -> bool {
    !BackendAvailability::detect().has_grim() || std::env::var_os("WAYLAND_DISPLAY").is_none()
}

fn ocr_skippable() -> bool {
    skippable() || !BackendAvailability::detect().has_tesseract()
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

#[tokio::test]
async fn ocr_empty_region_returns_empty_string() {
    if ocr_skippable() {
        eprintln!("skip: grim+tesseract+wayland not all present");
        return;
    }
    // A 32x32 region from the top-left is almost certainly blank
    // (compositor gap / wallpaper). Tesseract should exit 0 with no text.
    let cap = GrimCapture::detect().expect("detect grim");
    let png = cap
        .region(Region { x: 0, y: 0, w: 32, h: 32 }, ImageFormat::Png)
        .await
        .expect("capture");
    let ocr = TesseractOcr::detect().expect("detect tesseract");
    let text = ocr.extract_text(&png).await.expect("OCR");
    assert!(
        text.is_empty() || text.len() < 32,
        "expected near-empty text on blank region, got: {text:?}"
    );
}

#[tokio::test]
async fn ocr_full_screen_runs_without_error() {
    if ocr_skippable() {
        eprintln!("skip: grim+tesseract+wayland not all present");
        return;
    }
    // Full-screen capture + OCR. Doesn't assert specific text (depends on
    // what's onscreen), only that the pipeline returns without error.
    let cap = GrimCapture::detect().expect("detect grim");
    let png = cap.full(None, ImageFormat::Png).await.expect("capture");
    let ocr = TesseractOcr::detect().expect("detect tesseract");
    let _text = ocr.extract_text(&png).await.expect("OCR");
}
