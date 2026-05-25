//! Text extraction from images via `tesseract` shell-out.
//!
//! Tesseract accepts an image file path or `-` to read from stdin, and an
//! output file path or `-` to write recognized text to stdout. We pipe the
//! image bytes in on stdin and read text from stdout.
//!
//! Two modes:
//!
//! - [`TesseractOcr::extract_text`] — plain text (default tesseract output).
//! - [`TesseractOcr::extract_words`] — word-level positions via tesseract's
//!   `tsv` config. Required by composite tools that need bounding boxes
//!   (e.g. `find_text_position`, `click_text`).
//!
//! Tesseract is chatty on stderr by default (progress lines, "Empty page!!"
//! for blank inputs). We don't filter it — operators inspecting the
//! `BackendFailed.stderr` field benefit from seeing what tesseract said —
//! and a zero exit with empty stdout is reported as the empty string, not
//! [`VisionError::EmptyOutput`] (which is reserved for capture-backend
//! failures producing no bytes).

use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::capture::BBox;
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

/// One tesseract-recognised word with its bounding box and confidence.
/// The bbox is in the coordinate space of the image OCR'd, not the
/// compositor — callers that captured a sub-region must translate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub bbox: BBox,
    /// Tesseract confidence 0..=100. `-1` is also possible from
    /// tesseract for some unreadable cells but we filter those out
    /// in [`TesseractOcr::extract_words`].
    pub confidence: i32,
}

/// A multi-word match returned by [`find_word_runs`]. The bbox is the
/// union of all merged word bboxes; the confidence is the minimum of the
/// merged words (worst-case so callers can threshold on it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextMatch {
    pub text: String,
    pub bbox: BBox,
    pub confidence: i32,
}

/// `tesseract`-backed OCR.
pub struct TesseractOcr {
    binary: PathBuf,
    lang: String,
    psm: Psm,
    min_confidence: i32,
}

impl TesseractOcr {
    /// Probe `$PATH` for `tesseract`. Default config: English (`eng`),
    /// page-segmentation mode `SingleBlock`, min confidence 50.
    pub fn detect() -> Result<Self> {
        let bin = detect::which("tesseract")
            .ok_or(VisionError::BackendMissing("tesseract"))?;
        Ok(Self {
            binary: bin,
            lang: "eng".into(),
            psm: Psm::default(),
            min_confidence: 50,
        })
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

    /// Drop words below this confidence in [`Self::extract_words`].
    /// Values are 0..=100; tesseract's own `-1` sentinel is always dropped.
    pub fn with_min_confidence(mut self, threshold: i32) -> Self {
        self.min_confidence = threshold;
        self
    }

    /// Run tesseract on the supplied image bytes (any format tesseract
    /// recognizes: PNG, JPEG, TIFF, …). Returns the trimmed extracted text.
    /// An image with no detectable text yields an empty `String` (NOT
    /// [`VisionError::EmptyOutput`]) — tesseract exits 0 in that case.
    pub async fn extract_text(&self, image_bytes: &[u8]) -> Result<String> {
        let stdout = self.run(image_bytes, None).await?;
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    }

    /// Run tesseract with `tsv` output mode and parse the rows into
    /// [`Word`]s. Filters by `min_confidence` (see [`Self::with_min_confidence`]).
    /// An image with no detectable text yields an empty `Vec`.
    pub async fn extract_words(&self, image_bytes: &[u8]) -> Result<Vec<Word>> {
        let stdout = self.run(image_bytes, Some("tsv")).await?;
        let text = String::from_utf8_lossy(&stdout);
        Ok(parse_tsv(&text, self.min_confidence))
    }

    /// Spawn tesseract once. If `config` is `Some`, append it as a positional
    /// arg so tesseract writes the matching format (e.g. `tsv` for word
    /// positions); otherwise default plain-text output.
    async fn run(&self, image_bytes: &[u8], config: Option<&str>) -> Result<Vec<u8>> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("-") // input from stdin
            .arg("-") // output to stdout
            .arg("-l")
            .arg(&self.lang)
            .arg("--psm")
            .arg((self.psm as u8).to_string());
        if let Some(c) = config {
            cmd.arg(c);
        }
        let mut child = cmd
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
        Ok(output.stdout)
    }
}

/// Parse tesseract's TSV output. Columns (tab-separated):
///
/// `level  page  block  para  line  word  left  top  width  height  conf  text`
///
/// Only word-level rows (`level == 5`) with confidence >= `min_confidence`
/// and non-empty text contribute. Tesseract emits `-1` for confidence on
/// some rows; those are always dropped regardless of the threshold.
fn parse_tsv(tsv: &str, min_confidence: i32) -> Vec<Word> {
    let mut out = Vec::new();
    for (i, line) in tsv.lines().enumerate() {
        // Skip header.
        if i == 0 && line.starts_with("level\t") {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 12 {
            continue;
        }
        let Ok(level) = cols[0].parse::<i32>() else { continue };
        if level != 5 {
            continue;
        }
        let conf: i32 = cols[10].parse::<f32>().map(|c| c as i32).unwrap_or(-1);
        if conf < 0 || conf < min_confidence {
            continue;
        }
        let text = cols[11].trim();
        if text.is_empty() {
            continue;
        }
        let Ok(x) = cols[6].parse::<i32>() else { continue };
        let Ok(y) = cols[7].parse::<i32>() else { continue };
        let Ok(w) = cols[8].parse::<i32>() else { continue };
        let Ok(h) = cols[9].parse::<i32>() else { continue };
        out.push(Word {
            text: text.to_string(),
            bbox: BBox { x, y, w, h },
            confidence: conf,
        });
    }
    out
}

/// Find runs of consecutive words whose joined text matches `query`.
///
/// `query` is tokenized on whitespace. For each starting index `i`, we
/// concatenate words `i..i+k` (where `k = query token count`) and compare.
/// "Save File" matches `["Save", "File"]` at index `i` and emits one
/// [`TextMatch`] with the union of the two bboxes.
///
/// Word order matters; we don't re-order or skip intermediate words. If the
/// query has N tokens but tesseract split one of them differently (e.g.
/// hyphenation, OCR artefacts), there will be no match — caller can retry
/// with shorter queries.
fn same_row(a: &BBox, b: &BBox) -> bool {
    // Y-spans intersect, even by 1px.
    a.y < b.y.saturating_add(b.h) && b.y < a.y.saturating_add(a.h)
}

pub fn find_word_runs(words: &[Word], query: &str, case_sensitive: bool) -> Vec<TextMatch> {
    let query_tokens: Vec<&str> = query.split_whitespace().collect();
    if query_tokens.is_empty() || words.len() < query_tokens.len() {
        return Vec::new();
    }
    let normalize = |s: &str| -> String {
        if case_sensitive { s.to_string() } else { s.to_ascii_lowercase() }
    };
    let want: Vec<String> = query_tokens.iter().map(|t| normalize(t)).collect();

    let mut matches = Vec::new();
    let last = words.len().saturating_sub(want.len());
    for i in 0..=last {
        let mut ok = true;
        for (k, want_tok) in want.iter().enumerate() {
            if &normalize(&words[i + k].text) != want_tok {
                ok = false;
                break;
            }
            // Reject runs that cross rows. Without this, "Save" on line N
            // and "File" on line N+1 yield one tall bbox whose centre
            // clicks empty space between the lines.
            if k > 0 && !same_row(&words[i + k - 1].bbox, &words[i + k].bbox) {
                ok = false;
                break;
            }
        }
        if !ok {
            continue;
        }
        let run = &words[i..i + want.len()];
        let bbox = run
            .iter()
            .skip(1)
            .fold(run[0].bbox, |acc, w| acc.union(&w.bbox));
        let confidence = run.iter().map(|w| w.confidence).min().unwrap_or(0);
        let text = run
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        matches.push(TextMatch { text, bbox, confidence });
    }
    matches
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
        let err = VisionError::BackendMissing("tesseract");
        assert!(format!("{err}").contains("tesseract"));
    }

    /// A handcrafted slice of tesseract's TSV format. Real tesseract emits a
    /// dozen+ non-word level rows (page, block, para, line) before each word
    /// row; we keep one of each here so the level-filter is exercised.
    fn sample_tsv() -> &'static str {
        "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
         1\t1\t0\t0\t0\t0\t0\t0\t1000\t500\t-1\t\n\
         2\t1\t1\t0\t0\t0\t10\t10\t900\t100\t-1\t\n\
         3\t1\t1\t1\t0\t0\t10\t10\t900\t100\t-1\t\n\
         4\t1\t1\t1\t1\t0\t10\t10\t900\t100\t-1\t\n\
         5\t1\t1\t1\t1\t1\t10\t10\t80\t30\t95\tSave\n\
         5\t1\t1\t1\t1\t2\t100\t10\t80\t30\t90\tFile\n\
         5\t1\t1\t1\t1\t3\t200\t10\t100\t30\t30\tlow_conf\n\
         5\t1\t1\t1\t1\t4\t320\t10\t60\t30\t-1\t\n\
         5\t1\t1\t1\t1\t5\t400\t10\t80\t30\t75\tCancel\n"
    }

    #[test]
    fn parse_tsv_filters_to_word_level() {
        let words = parse_tsv(sample_tsv(), 50);
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        // low_conf (conf 30 < 50) and the conf=-1 empty row should both drop.
        assert_eq!(texts, vec!["Save", "File", "Cancel"]);
    }

    #[test]
    fn parse_tsv_bbox_values() {
        let words = parse_tsv(sample_tsv(), 50);
        assert_eq!(words[0].bbox, BBox { x: 10, y: 10, w: 80, h: 30 });
        assert_eq!(words[0].confidence, 95);
        assert_eq!(words[1].bbox, BBox { x: 100, y: 10, w: 80, h: 30 });
        assert_eq!(words[2].bbox, BBox { x: 400, y: 10, w: 80, h: 30 });
    }

    #[test]
    fn parse_tsv_confidence_threshold_lets_through_low_when_relaxed() {
        let words = parse_tsv(sample_tsv(), 10);
        let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["Save", "File", "low_conf", "Cancel"]);
    }

    #[test]
    fn parse_tsv_min_confidence_excludes_everything_when_high() {
        let words = parse_tsv(sample_tsv(), 99);
        assert!(words.is_empty());
    }

    #[test]
    fn parse_tsv_ignores_malformed_rows() {
        // Missing columns, non-numeric coords.
        let tsv = "level\tpage\tblock\tpara\tline\tword\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t1\t1\t1\t1\tnotanum\t10\t80\t30\t95\tBroken\n\
                   short\trow\n\
                   5\t1\t1\t1\t1\t2\t10\t10\t80\t30\t95\tOk\n";
        let words = parse_tsv(tsv, 0);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "Ok");
    }

    fn word(text: &str, x: i32, y: i32, w: i32, h: i32, conf: i32) -> Word {
        Word { text: text.into(), bbox: BBox { x, y, w, h }, confidence: conf }
    }

    #[test]
    fn find_word_runs_single_word_case_insensitive() {
        let words = vec![word("Save", 0, 0, 50, 20, 95), word("File", 60, 0, 50, 20, 90)];
        let m = find_word_runs(&words, "save", false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "Save");
        assert_eq!(m[0].bbox, BBox { x: 0, y: 0, w: 50, h: 20 });
        assert_eq!(m[0].confidence, 95);
    }

    #[test]
    fn find_word_runs_case_sensitive_rejects_mismatched_case() {
        let words = vec![word("Save", 0, 0, 50, 20, 95)];
        let m = find_word_runs(&words, "save", true);
        assert!(m.is_empty());
        let m = find_word_runs(&words, "Save", true);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn find_word_runs_rejects_run_across_rows() {
        // "Save" on row 1, "File" on row 2. A naive merge would emit a tall
        // bbox whose centre clicks the gap between rows.
        let words = vec![
            word("Save", 10, 0, 50, 20, 95),
            word("File", 10, 40, 40, 20, 80), // y=40, well below row 1 (y=0..20)
        ];
        let m = find_word_runs(&words, "Save File", false);
        assert!(m.is_empty(), "multi-row merge must be suppressed");
    }

    #[test]
    fn find_word_runs_accepts_run_with_slight_y_drift() {
        // OCR sometimes nudges baseline-aligned words by a pixel or two.
        // Y-overlap (even 1px) still counts as same-row.
        let words = vec![
            word("Save", 10, 0, 50, 20, 95),
            word("File", 70, 3, 40, 18, 80), // y=3, overlaps row 1 (y=0..20)
        ];
        let m = find_word_runs(&words, "Save File", false);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn find_word_runs_multi_word_merges_bboxes() {
        let words = vec![
            word("Save", 10, 0, 50, 20, 95),
            word("File", 70, 0, 40, 20, 80),
        ];
        let m = find_word_runs(&words, "Save File", false);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].text, "Save File");
        assert_eq!(m[0].bbox, BBox { x: 10, y: 0, w: 100, h: 20 });
        // Confidence is the worst of the two.
        assert_eq!(m[0].confidence, 80);
    }

    #[test]
    fn find_word_runs_multiple_matches() {
        let words = vec![
            word("OK", 0, 0, 20, 20, 95),
            word("filler", 30, 0, 20, 20, 80),
            word("OK", 60, 0, 20, 20, 90),
        ];
        let m = find_word_runs(&words, "ok", false);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].bbox.x, 0);
        assert_eq!(m[1].bbox.x, 60);
    }

    #[test]
    fn find_word_runs_returns_empty_for_no_match() {
        let words = vec![word("Foo", 0, 0, 10, 10, 95)];
        let m = find_word_runs(&words, "Bar", false);
        assert!(m.is_empty());
    }

    #[test]
    fn find_word_runs_returns_empty_for_empty_query() {
        let words = vec![word("Foo", 0, 0, 10, 10, 95)];
        let m = find_word_runs(&words, "   ", false);
        assert!(m.is_empty());
    }

    #[test]
    fn find_word_runs_query_longer_than_words_is_empty() {
        let words = vec![word("a", 0, 0, 10, 10, 95)];
        let m = find_word_runs(&words, "a b c", false);
        assert!(m.is_empty());
    }
}
