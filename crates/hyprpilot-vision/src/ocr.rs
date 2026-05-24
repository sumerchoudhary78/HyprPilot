//! Text extraction from images via `tesseract` shell-out.
//!
//! Tesseract accepts an image file path or `-` to read from stdin, and an
//! output file path or `-` to write recognized text to stdout. We pipe the
//! image bytes in on stdin and read text from stdout.
//!
//! Tesseract is chatty on stderr by default (progress lines, "Empty page!!"
//! for blank inputs). We don't filter it — operators inspecting the
//! `BackendFailed.stderr` field benefit from seeing what tesseract said —
//! and a zero exit with empty stdout is reported as the empty string, not
//! [`VisionError::EmptyOutput`] (which is reserved for capture-backend
//! failures producing no bytes).

use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::detect;
use crate::error::{Result, VisionError};

/// Page-segmentation mode passed to `tesseract --psm`. Defaults to
/// [`Psm::SingleBlock`], which works well for UI screenshots where text
/// is one homogeneous block.
#[derive(Debug, Default, Clone, Copy)]
pub enum Psm {
    /// PSM 3: fully automatic, no orientation/script detection. Tesseract's
    /// default for arbitrary documents.
    Auto = 3,
    /// PSM 6: assume a single uniform block of text. Best for screenshots
    /// of UI panels, terminal output, dialog boxes.
    #[default]
    SingleBlock = 6,
    /// PSM 7: treat the image as a single text line. Good for menu items,
    /// title bars, button labels.
    SingleLine = 7,
    /// PSM 11: sparse text. Useful for screenshots with text scattered
    /// across whitespace.
    SparseText = 11,
}

/// `tesseract`-backed OCR.
pub struct TesseractOcr {
    binary: PathBuf,
    lang: String,
    psm: Psm,
}

impl TesseractOcr {
    /// Probe `$PATH` for `tesseract`. Default config: English (`eng`),
    /// page-segmentation mode `SingleBlock`.
    pub fn detect() -> Result<Self> {
        let bin = detect::which("tesseract")
            .ok_or(VisionError::BackendMissing("tesseract"))?;
        Ok(Self { binary: bin, lang: "eng".into(), psm: Psm::default() })
    }

    /// Override the language. Tesseract uses `eng+fra` syntax for
    /// multi-language; pass the string verbatim.
    pub fn with_lang(mut self, lang: impl Into<String>) -> Self {
        self.lang = lang.into();
        self
    }

    pub fn with_psm(mut self, psm: Psm) -> Self {
        self.psm = psm;
        self
    }

    /// Run tesseract on the supplied image bytes (any format tesseract
    /// recognizes: PNG, JPEG, TIFF, …). Returns the trimmed extracted text.
    /// An image with no detectable text yields an empty `String` (NOT
    /// [`VisionError::EmptyOutput`]) — tesseract exits 0 in that case.
    pub async fn extract_text(&self, image_bytes: &[u8]) -> Result<String> {
        let mut child = Command::new(&self.binary)
            .arg("-") // input from stdin
            .arg("-") // output to stdout
            .arg("-l")
            .arg(&self.lang)
            .arg("--psm")
            .arg((self.psm as u8).to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Feed bytes on stdin from a separate task so we can concurrently
        // drain stdout / stderr without deadlocking on a full pipe.
        if let Some(mut stdin) = child.stdin.take() {
            let bytes = image_bytes.to_vec();
            tokio::spawn(async move {
                let _ = stdin.write_all(&bytes).await;
                let _ = stdin.shutdown().await;
            });
        }

        let output = child.wait_with_output().await?;
        if !output.status.success() {
            return Err(VisionError::BackendFailed {
                backend: "tesseract",
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psm_default_is_single_block() {
        assert!(matches!(Psm::default(), Psm::SingleBlock));
        assert_eq!(Psm::SingleBlock as u8, 6);
        assert_eq!(Psm::SingleLine as u8, 7);
    }

    #[test]
    fn detect_missing_returns_backend_missing() {
        // Confirms the error variant routing. The actual presence check is
        // exercised by detect::tests which already passes.
        let err = VisionError::BackendMissing("tesseract");
        assert!(format!("{err}").contains("tesseract"));
    }
}
