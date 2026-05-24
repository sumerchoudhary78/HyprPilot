//! Screen capture via `grim` shell-out.
//!
//! `grim` writes the captured image to stdout when given `-` as the output
//! file. We pipe its stdout into a `Vec<u8>`. Errors are taken from
//! `grim`'s stderr verbatim — `grim` writes concise diagnostics already.

use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::detect;
use crate::error::{Result, VisionError};

/// A rectangle on the global compositor coordinate space. `(x, y)` is the
/// top-left corner; `w` and `h` are the dimensions in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    /// Encode in `grim -g` geometry form: `"X,Y WxH"`.
    pub fn grim_geometry(&self) -> String {
        format!("{},{} {}x{}", self.x, self.y, self.w, self.h)
    }

    /// Reject degenerate regions before we pay the grim spawn cost.
    pub fn validate(&self) -> Result<()> {
        if self.w == 0 || self.h == 0 {
            return Err(VisionError::InvalidRegion {
                w: self.w as i32,
                h: self.h as i32,
            });
        }
        Ok(())
    }
}

/// Image format requested from `grim`. Defaults to PNG for lossless capture;
/// JPEG is offered for size-sensitive uses (full-screen grabs streamed over
/// MCP).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpeg,
    Ppm,
}

impl ImageFormat {
    /// Matches `grim -t <flag>`.
    pub fn grim_flag(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Ppm => "ppm",
        }
    }

    /// MIME type for MCP `image` content blocks.
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Ppm => "image/x-portable-pixmap",
        }
    }
}

/// `grim`-backed screen capture.
///
/// Construct once via [`GrimCapture::detect`] (probes `$PATH`); the same
/// instance is safe to use for multiple captures. Each `full` / `region`
/// call spawns a fresh `grim` subprocess and collects its stdout.
pub struct GrimCapture {
    binary: PathBuf,
    include_cursor: bool,
}

impl GrimCapture {
    /// Probe `$PATH` for `grim`. Returns [`VisionError::BackendMissing`] if
    /// not found.
    pub fn detect() -> Result<Self> {
        let bin = detect::which("grim").ok_or(VisionError::BackendMissing("grim"))?;
        Ok(Self { binary: bin, include_cursor: false })
    }

    /// Include the mouse cursor in captures (`grim -c`).
    pub fn with_cursor(mut self, on: bool) -> Self {
        self.include_cursor = on;
        self
    }

    /// Capture the whole compositor (or a single monitor if `monitor` is
    /// `Some`). Monitor names match `hyprctl monitors` output, e.g. `eDP-1`.
    pub async fn full(
        &self,
        monitor: Option<&str>,
        format: ImageFormat,
    ) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.binary);
        if self.include_cursor {
            cmd.arg("-c");
        }
        if let Some(name) = monitor {
            cmd.arg("-o").arg(name);
        }
        cmd.arg("-t").arg(format.grim_flag()).arg("-");
        run(&mut cmd, "grim").await
    }

    /// Capture a rectangular region of the compositor.
    pub async fn region(&self, region: Region, format: ImageFormat) -> Result<Vec<u8>> {
        region.validate()?;
        let mut cmd = Command::new(&self.binary);
        if self.include_cursor {
            cmd.arg("-c");
        }
        cmd.arg("-g")
            .arg(region.grim_geometry())
            .arg("-t")
            .arg(format.grim_flag())
            .arg("-");
        run(&mut cmd, "grim").await
    }
}

async fn run(cmd: &mut Command, name: &'static str) -> Result<Vec<u8>> {
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        return Err(VisionError::BackendFailed {
            backend: name,
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    if output.stdout.is_empty() {
        return Err(VisionError::EmptyOutput);
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_geometry_encoding() {
        assert_eq!(
            Region { x: 0, y: 0, w: 100, h: 50 }.grim_geometry(),
            "0,0 100x50"
        );
        assert_eq!(
            Region { x: -10, y: 200, w: 1920, h: 1080 }.grim_geometry(),
            "-10,200 1920x1080"
        );
    }

    #[test]
    fn region_rejects_zero_dimensions() {
        assert!(matches!(
            Region { x: 0, y: 0, w: 0, h: 100 }.validate(),
            Err(VisionError::InvalidRegion { .. })
        ));
        assert!(matches!(
            Region { x: 0, y: 0, w: 100, h: 0 }.validate(),
            Err(VisionError::InvalidRegion { .. })
        ));
        assert!(Region { x: 0, y: 0, w: 100, h: 100 }.validate().is_ok());
    }

    #[test]
    fn image_format_grim_flag() {
        assert_eq!(ImageFormat::Png.grim_flag(), "png");
        assert_eq!(ImageFormat::Jpeg.grim_flag(), "jpeg");
        assert_eq!(ImageFormat::Ppm.grim_flag(), "ppm");
    }

    #[test]
    fn image_format_mime() {
        assert_eq!(ImageFormat::Png.mime(), "image/png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
    }
}
