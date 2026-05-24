//! Declarative rule engine config.
//!
//! Rules are loaded from a TOML file (default
//! `$XDG_CONFIG_HOME/hyprpilot/rules.toml`) at daemon startup. Each rule
//! pairs an event kind with a set of equality predicates and a list of
//! actions to execute on match.
//!
//! Example:
//!
//! ```toml
//! [[rule]]
//! name = "slack to scratchpad"
//! on = "openwindow"
//! when = { class = "Slack" }
//! do = [
//!   "move_to_workspace_silent special:scratch",
//! ]
//!
//! [[rule]]
//! name = "browser fullscreen on workspace 1"
//! on = "openwindow"
//! when = { class = "google-chrome", workspace = "1" }
//! do = ["fullscreen 1"]
//! ```
//!
//! Semantics:
//! - **First match wins.** Rules are evaluated top-to-bottom; the first
//!   rule whose `on` matches the event kind and whose `when` predicates
//!   all hold against the event's `fields` map fires its actions. No later
//!   rule fires for the same event.
//! - **Equality only.** v0.4 doesn't support regex, ranges, or negation.
//!   A `when` of `{}` matches any event of the same `kind`.
//! - **Origin-tagged reentrance.** While the engine is executing rule
//!   actions, incoming events are skipped for rule matching (the engine
//!   doesn't fire rules in response to its own actions). See the engine
//!   itself, not this module, for that mechanism.
//!
//! Action grammar is documented in the engine; this module is types +
//! loader only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rule {
    /// Optional human-readable name. Shown in logs and in
    /// `rules list` / `rules_list` output. If absent, callers should
    /// display the rule's index in the config.
    #[serde(default)]
    pub name: Option<String>,

    /// Event kind to match (e.g. `"openwindow"`, `"workspace"`,
    /// `"changefloatingmode"`). Matched against [`crate::Event::kind`].
    pub on: String,

    /// Field-equality predicates. All entries must hold against the
    /// event's `fields` map for the rule to fire. An empty `when` matches
    /// any event of the same `kind`.
    #[serde(default)]
    pub when: BTreeMap<String, String>,

    /// Actions to execute on match, in order. Strings in dispatcher form
    /// — e.g. `"move_to_workspace_silent special:scratch"`, `"pin"`,
    /// `"exec foo --bar"`. Parsed and dispatched by the engine.
    #[serde(rename = "do")]
    pub actions: Vec<String>,
}

impl Rule {
    /// Display label for logging / introspection.
    pub fn label(&self, index: usize) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("rule[{index}] on={}", self.on))
    }

    /// Does this rule's `when` predicate hold against the given event
    /// fields? Returns true iff every `when` (key, value) entry is
    /// present-and-equal in `fields`.
    pub fn predicate_matches(&self, fields: &BTreeMap<String, String>) -> bool {
        self.when
            .iter()
            .all(|(k, v)| fields.get(k).map(String::as_str) == Some(v.as_str()))
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    /// All rules in declaration order. TOML uses `[[rule]]` table arrays;
    /// the serde rename keeps that natural surface.
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

impl RuleConfig {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }
}

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("rules file I/O ({path}): {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("rules file parse ({path}): {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
}

/// Default location: `$XDG_CONFIG_HOME/hyprpilot/rules.toml`, falling
/// back to `$HOME/.config/hyprpilot/rules.toml`.
pub fn default_path() -> PathBuf {
    let base = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        PathBuf::from(".")
    };
    base.join("hyprpilot").join("rules.toml")
}

pub fn load_from(path: &Path) -> Result<RuleConfig, LoadError> {
    let raw = std::fs::read_to_string(path).map_err(|e| LoadError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    toml::from_str(&raw).map_err(|e| LoadError::Parse {
        path: path.to_path_buf(),
        source: e,
    })
}

/// Load the default rules file. `Ok(None)` if the file is absent (the
/// expected "no rules configured" path). Errors only on parse failures or
/// real I/O problems.
pub fn load_default() -> Result<Option<RuleConfig>, LoadError> {
    let path = default_path();
    if !path.exists() {
        return Ok(None);
    }
    load_from(&path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn rule(on: &str, when: &[(&str, &str)], actions: &[&str]) -> Rule {
        Rule {
            name: None,
            on: on.to_string(),
            when: when.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            actions: actions.iter().map(|a| a.to_string()).collect(),
        }
    }

    #[test]
    fn predicate_empty_matches_everything() {
        let r = rule("openwindow", &[], &["pin"]);
        assert!(r.predicate_matches(&fields(&[])));
        assert!(r.predicate_matches(&fields(&[("class", "Slack")])));
    }

    #[test]
    fn predicate_single_equality() {
        let r = rule("openwindow", &[("class", "Slack")], &["pin"]);
        assert!(r.predicate_matches(&fields(&[("class", "Slack"), ("workspace", "2")])));
        assert!(!r.predicate_matches(&fields(&[("class", "Chrome")])));
        assert!(!r.predicate_matches(&fields(&[("workspace", "2")])));
    }

    #[test]
    fn predicate_all_must_hold() {
        let r = rule(
            "openwindow",
            &[("class", "Chrome"), ("workspace", "1")],
            &["pin"],
        );
        assert!(r.predicate_matches(&fields(&[("class", "Chrome"), ("workspace", "1")])));
        assert!(!r.predicate_matches(&fields(&[("class", "Chrome"), ("workspace", "2")])));
        assert!(!r.predicate_matches(&fields(&[("class", "Chrome")])));
    }

    #[test]
    fn label_uses_name_when_present() {
        let mut r = rule("openwindow", &[], &[]);
        r.name = Some("slack rule".into());
        assert_eq!(r.label(0), "slack rule");
    }

    #[test]
    fn label_falls_back_to_index_and_kind() {
        let r = rule("workspace", &[], &[]);
        assert_eq!(r.label(3), "rule[3] on=workspace");
    }

    #[test]
    fn parses_toml_with_two_rules() {
        let toml_src = r#"
            [[rule]]
            name = "slack"
            on = "openwindow"
            when = { class = "Slack" }
            do = ["move_to_workspace_silent special:scratch"]

            [[rule]]
            on = "workspace"
            when = { name = "9" }
            do = ["exec notify-send 'on 9'", "pin"]
        "#;
        let cfg: RuleConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.len(), 2);
        assert_eq!(cfg.rules[0].name.as_deref(), Some("slack"));
        assert_eq!(cfg.rules[0].on, "openwindow");
        assert_eq!(cfg.rules[0].when.get("class").unwrap(), "Slack");
        assert_eq!(cfg.rules[0].actions.len(), 1);
        assert_eq!(cfg.rules[1].name, None);
        assert_eq!(cfg.rules[1].actions.len(), 2);
    }

    #[test]
    fn empty_toml_parses_as_no_rules() {
        let cfg: RuleConfig = toml::from_str("").unwrap();
        assert!(cfg.is_empty());
    }

    #[test]
    fn missing_file_returns_none() {
        let p = std::env::temp_dir().join(format!(
            "hyprpilot-rules-nonexistent-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        // Direct call to load_from on nonexistent should error;
        // load_default would short-circuit to Ok(None) via path.exists().
        let r = load_from(&p);
        assert!(matches!(r, Err(LoadError::Io { .. })));
    }

    #[test]
    fn load_from_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "hyprpilot-rules-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.toml");
        std::fs::write(
            &path,
            r#"
            [[rule]]
            on = "openwindow"
            when = { class = "kitty" }
            do = ["pin"]
            "#,
        )
        .unwrap();
        let cfg = load_from(&path).unwrap();
        assert_eq!(cfg.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
