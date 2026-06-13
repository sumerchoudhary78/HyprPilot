//! A flattened accessibility element and the (pure) query matcher.
//!
//! Extents are **window-relative** (AT-SPI `CoordType::Window`): they are the
//! element's box measured from the top-left of its application window, in the
//! pixels the toolkit reports. They are deliberately *not* screen coordinates —
//! on Wayland a client doesn't reliably know its own screen position, so the
//! caller turns these into a click target by adding the window's on-screen
//! origin (which Hyprland knows) and applying the monitor scale.

use serde::{Deserialize, Serialize};

/// One node of an application's accessibility tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Element {
    /// Stable AT-SPI role name, e.g. `push button`, `entry`, `label`.
    pub role: String,
    /// Accessible name — usually the visible label / accessible description.
    pub name: String,
    /// Text content, when the node implements the Text interface and the
    /// content is short enough to be useful inline. `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Window-relative bounding box (pixels): top-left x.
    pub x: i32,
    /// Window-relative bounding box: top-left y.
    pub y: i32,
    /// Window-relative bounding box: width.
    pub w: i32,
    /// Window-relative bounding box: height.
    pub h: i32,
    /// Depth below the application root (root = 0).
    pub depth: u32,
    /// AT-SPI object path — an opaque handle, useful for debugging.
    pub path: String,
}

impl Element {
    /// A box is clickable only if it has positive area. Many container and
    /// off-screen nodes report a zero/degenerate box.
    pub fn is_clickable(&self) -> bool {
        self.w > 0 && self.h > 0
    }

    /// Window-relative centre of the box.
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// Does `el` match the query? Case-insensitive substring against the
/// accessible name or inline text; if `role` is given it must equal the
/// element's role (case-insensitive). An empty `query` matches on role alone.
pub fn matches(el: &Element, query: &str, role: Option<&str>) -> bool {
    let role_ok = role.map(|r| el.role.eq_ignore_ascii_case(r)).unwrap_or(true);
    if !role_ok {
        return false;
    }
    if query.is_empty() {
        return role.is_some();
    }
    let q = query.to_lowercase();
    let name_hit = el.name.to_lowercase().contains(&q);
    let text_hit = el.text.as_deref().is_some_and(|t| t.to_lowercase().contains(&q));
    name_hit || text_hit
}

/// Filter a flat element list by [`matches`].
pub fn filter_elements(els: &[Element], query: &str, role: Option<&str>) -> Vec<Element> {
    els.iter().filter(|e| matches(e, query, role)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(role: &str, name: &str, text: Option<&str>, w: i32, h: i32) -> Element {
        Element {
            role: role.into(),
            name: name.into(),
            text: text.map(Into::into),
            x: 0,
            y: 0,
            w,
            h,
            depth: 1,
            path: "/org/a11y/atspi/accessible/1".into(),
        }
    }

    #[test]
    fn name_substring_case_insensitive() {
        let e = el("push button", "Submit Form", None, 80, 24);
        assert!(matches(&e, "submit", None));
        assert!(matches(&e, "FORM", None));
        assert!(!matches(&e, "cancel", None));
    }

    #[test]
    fn role_filter_must_match() {
        let e = el("push button", "Submit", None, 80, 24);
        assert!(matches(&e, "submit", Some("push button")));
        assert!(matches(&e, "submit", Some("PUSH BUTTON")));
        assert!(!matches(&e, "submit", Some("entry")));
    }

    #[test]
    fn empty_query_matches_on_role_only() {
        let e = el("entry", "", None, 200, 24);
        assert!(matches(&e, "", Some("entry")), "role-only selection");
        assert!(!matches(&e, "", None), "empty query + no role matches nothing");
    }

    #[test]
    fn text_content_is_searched() {
        let e = el("entry", "Email", Some("user@example.com"), 200, 24);
        assert!(matches(&e, "example.com", None));
    }

    #[test]
    fn filter_returns_all_hits() {
        let els = vec![
            el("push button", "OK", None, 40, 20),
            el("push button", "Cancel", None, 40, 20),
            el("label", "OK to proceed?", None, 120, 20),
        ];
        let hits = filter_elements(&els, "ok", None);
        assert_eq!(hits.len(), 2, "button OK + label containing 'ok'");
        let buttons = filter_elements(&els, "ok", Some("push button"));
        assert_eq!(buttons.len(), 1);
    }

    #[test]
    fn clickable_requires_positive_area() {
        assert!(el("push button", "Go", None, 30, 20).is_clickable());
        assert!(!el("filler", "", None, 0, 20).is_clickable());
        assert!(!el("filler", "", None, 30, 0).is_clickable());
    }

    #[test]
    fn center_is_box_midpoint() {
        let mut e = el("push button", "Go", None, 100, 40);
        e.x = 10;
        e.y = 20;
        assert_eq!(e.center(), (60, 40));
    }
}
