//! Rules engine: subscribe to Hyprland's event socket, match each event
//! against [`crate::rules::Rule`]s, and execute the matched rule's
//! [`crate::actions::Action`] list.
//!
//! ## Loop shape
//!
//! ```text
//! event arrives → typed via core::Event::from_raw
//!   ↓
//! reentrance gate? (skip if engine is currently executing actions)
//!   ↓
//! walk rules in order, first match wins
//!   ↓
//! bump reentrance counter, execute actions, schedule decrement after decay
//! ```
//!
//! ## Reentrance
//!
//! Hyprland echoes events when the engine fires a dispatcher. Without
//! protection, a rule on `openwindow` that moves the window to ws 9 would
//! see `movewindow` → maybe trigger another rule → infinite loop.
//!
//! [`ReentranceGuard`] holds an atomic depth counter. While > 0, the
//! engine skips rule matching for incoming events. A short *decay window*
//! (default 100 ms) keeps the counter elevated after action execution
//! finishes, so echo events that arrive slightly later are still caught.
//!
//! This is coarse — legitimate user actions during the decay window are
//! also dropped from rule matching — but it's predictable and prevents
//! the worst failure mode (runaway loops).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tracing::{debug, error, info, trace, warn};

use hyprpilot_core::{Connection, Event, EventStream};

use crate::actions::{self, Action};
use crate::rules::RuleConfig;

const DEFAULT_DECAY_MS: u64 = 100;

#[derive(Clone)]
pub struct ReentranceGuard {
    depth: Arc<AtomicUsize>,
    decay: Duration,
}

impl ReentranceGuard {
    pub fn new(decay: Duration) -> Self {
        Self { depth: Arc::new(AtomicUsize::new(0)), decay }
    }

    pub fn is_inhibited(&self) -> bool {
        self.depth.load(Ordering::SeqCst) > 0
    }

    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::SeqCst)
    }

    /// Bump the counter. The returned guard decrements after `decay` when
    /// dropped (the decrement is scheduled on a detached tokio task).
    pub fn bump(&self) -> BumpGuard {
        self.depth.fetch_add(1, Ordering::SeqCst);
        BumpGuard { depth: self.depth.clone(), decay: self.decay }
    }
}

#[must_use = "drop the guard to schedule the decay-then-decrement"]
pub struct BumpGuard {
    depth: Arc<AtomicUsize>,
    decay: Duration,
}

impl Drop for BumpGuard {
    fn drop(&mut self) {
        let depth = self.depth.clone();
        let decay = self.decay;
        tokio::spawn(async move {
            tokio::time::sleep(decay).await;
            depth.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

/// Pre-parsed rule, keyed by index in the source config. Holds the parsed
/// `Action` list so each event-time match doesn't re-parse strings.
struct CompiledRule {
    label: String,
    on: String,
    when: std::collections::BTreeMap<String, String>,
    actions: Vec<Action>,
}

/// Compile a config: parse every rule's actions ahead of time so action
/// errors surface at startup, not at firing time. Returns the per-rule
/// label + on + when + parsed actions, or an aggregated error string.
pub fn compile(config: &RuleConfig) -> Result<Vec<CompiledRulePublic>, String> {
    let mut out = Vec::with_capacity(config.rules.len());
    for (i, rule) in config.rules.iter().enumerate() {
        let actions = actions::parse_all(&rule.actions)
            .map_err(|(idx, e)| format!("{}: action[{idx}]: {e}", rule.label(i)))?;
        out.push(CompiledRulePublic {
            label: rule.label(i),
            on: rule.on.clone(),
            when: rule.when.clone(),
            actions,
        });
    }
    Ok(out)
}

/// Public view of a compiled rule. Distinct from the internal
/// `CompiledRule` only for visibility — same fields.
#[derive(Debug, Clone)]
pub struct CompiledRulePublic {
    pub label: String,
    pub on: String,
    pub when: std::collections::BTreeMap<String, String>,
    pub actions: Vec<Action>,
}

impl From<CompiledRulePublic> for CompiledRule {
    fn from(p: CompiledRulePublic) -> Self {
        CompiledRule {
            label: p.label,
            on: p.on,
            when: p.when,
            actions: p.actions,
        }
    }
}

/// Run the engine until `shutdown` fires or the event stream closes.
///
/// Errors:
/// - Returns immediately if `compile(config)` fails.
/// - Returns if the event stream fails to connect.
/// - Returns Ok(()) on graceful shutdown or Hyprland closing the socket.
///
/// Per-action failures during rule execution log a warning and continue
/// with the rule's remaining actions — one bad action doesn't stop the
/// engine.
pub async fn run(
    conn: Connection,
    config: RuleConfig,
    shutdown: Arc<Notify>,
) -> anyhow::Result<()> {
    let compiled: Vec<CompiledRule> = compile(&config)
        .map_err(|e| anyhow::anyhow!("rule compile failure: {e}"))?
        .into_iter()
        .map(Into::into)
        .collect();

    if compiled.is_empty() {
        info!("no rules configured; engine idle");
        // Block on shutdown so the caller's task handle stays alive,
        // matching the "engine is running" lifecycle even when empty.
        shutdown.notified().await;
        return Ok(());
    }

    info!(rules = compiled.len(), "rules engine starting");

    let mut stream = EventStream::connect(&conn)
        .await
        .map_err(|e| anyhow::anyhow!("connect to event socket: {e}"))?;
    let guard = ReentranceGuard::new(Duration::from_millis(DEFAULT_DECAY_MS));

    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                info!("rules engine: shutdown signal");
                return Ok(());
            }
            next = stream.next() => {
                let raw = match next {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        warn!("event socket closed by Hyprland; engine exiting");
                        return Ok(());
                    }
                    Err(e) => {
                        error!(error = %e, "event socket read failed; engine exiting");
                        return Err(anyhow::anyhow!("event socket: {e}"));
                    }
                };
                let event = Event::from_raw(raw);
                handle_event(&conn, &compiled, &guard, &event).await;
            }
        }
    }
}

async fn handle_event(
    conn: &Connection,
    rules: &[CompiledRule],
    guard: &ReentranceGuard,
    event: &Event,
) {
    if guard.is_inhibited() {
        trace!(kind = %event.kind, depth = guard.depth(), "matching inhibited (reentrance)");
        return;
    }
    for rule in rules {
        if rule.on != event.kind {
            continue;
        }
        if !predicate_matches(&rule.when, &event.fields) {
            continue;
        }
        // First match wins.
        info!(
            rule = %rule.label,
            event = %event.kind,
            actions = rule.actions.len(),
            "rule matched, executing"
        );
        let _bump = guard.bump();
        for (i, action) in rule.actions.iter().enumerate() {
            match action.execute(conn).await {
                Ok(()) => debug!(rule = %rule.label, action_index = i, "ok"),
                Err(e) => warn!(
                    rule = %rule.label,
                    action_index = i,
                    error = %e,
                    "action failed; continuing with remaining actions"
                ),
            }
        }
        return;
    }
}

fn predicate_matches(
    when: &std::collections::BTreeMap<String, String>,
    fields: &std::collections::BTreeMap<String, String>,
) -> bool {
    when.iter()
        .all(|(k, v)| fields.get(k).map(String::as_str) == Some(v.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn guard_inhibits_during_bump_lifetime() {
        let g = ReentranceGuard::new(Duration::from_millis(50));
        assert!(!g.is_inhibited());
        {
            let _b = g.bump();
            assert!(g.is_inhibited());
        }
        // Decrement is scheduled; still inhibited briefly.
        assert!(g.is_inhibited());
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!g.is_inhibited(), "should have decayed");
    }

    #[tokio::test]
    async fn guard_depth_increments() {
        let g = ReentranceGuard::new(Duration::from_millis(50));
        let _b1 = g.bump();
        let _b2 = g.bump();
        assert_eq!(g.depth(), 2);
    }

    #[test]
    fn compile_validates_action_strings() {
        let cfg: RuleConfig = toml::from_str(
            r#"
            [[rule]]
            on = "openwindow"
            do = ["definitely_not_a_verb"]
            "#,
        )
        .unwrap();
        let err = compile(&cfg).unwrap_err();
        assert!(err.contains("action[0]"));
        assert!(err.contains("definitely_not_a_verb"));
    }

    #[test]
    fn compile_ok_for_valid_rules() {
        let cfg: RuleConfig = toml::from_str(
            r#"
            [[rule]]
            name = "slack"
            on = "openwindow"
            when = { class = "Slack" }
            do = ["move_to_workspace_silent special:scratch", "pin"]
            "#,
        )
        .unwrap();
        let c = compile(&cfg).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].label, "slack");
        assert_eq!(c[0].actions.len(), 2);
    }
}
