//! Match a key combo against the live keybind list, so `run_bind` can execute
//! the action a chord is bound to.
//!
//! This is the "operate by keymap" path: instead of synthesising a keypress
//! (ydotool, timing-sensitive, needs the dangerous-input gate) or hunting for
//! a button to click, look up what the user already bound a chord to and
//! dispatch that action directly in the compositor. The agent is bounded to
//! actions the user configured — it can't invent new ones.

use std::collections::BTreeSet;

use hyprpilot_core::types::Bind;
use hyprpilot_input::keys::KeyCombo;

/// The all-caps modifier-name set for a combo, e.g. `{"CTRL","SHIFT"}` —
/// the same spelling Hyprland reports in [`Bind::mods`].
fn combo_mod_set(combo: &KeyCombo) -> BTreeSet<String> {
    combo.modifiers.iter().map(|m| m.hyprctl_name().to_string()).collect()
}

/// Does `bind` fire for `combo` in submap `submap`?
///
/// Keyboard binds only (mouse binds excluded). Modifiers are compared as
/// sets (order-independent); the key is compared case-insensitively because
/// Hyprland's reported key case varies (`T` vs `t`).
pub fn bind_matches(bind: &Bind, combo: &KeyCombo, submap: &str) -> bool {
    if bind.mouse || bind.submap != submap || !bind.key.eq_ignore_ascii_case(&combo.key) {
        return false;
    }
    let bind_mods: BTreeSet<String> = bind.mods.iter().cloned().collect();
    bind_mods == combo_mod_set(combo)
}

/// First bind that fires for `combo` in `submap` (Hyprland resolves a chord to
/// one action per submap; first-match mirrors that).
pub fn resolve<'a>(binds: &'a [Bind], combo: &KeyCombo, submap: &str) -> Option<&'a Bind> {
    binds.iter().find(|b| bind_matches(b, combo, submap))
}

/// The dispatch string a bind maps to: `"<dispatcher> <arg>"`, or just the
/// dispatcher when it takes no argument (e.g. `killactive`).
pub fn bind_action(bind: &Bind) -> String {
    if bind.arg.is_empty() {
        bind.dispatcher.clone()
    } else {
        format!("{} {}", bind.dispatcher, bind.arg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(mods: &[&str], key: &str, dispatcher: &str, arg: &str, submap: &str, mouse: bool) -> Bind {
        serde_json::from_value(serde_json::json!({
            "mods": mods,
            "modmask": 0,
            "key": key,
            "dispatcher": dispatcher,
            "arg": arg,
            "submap": submap,
            "mouse": mouse,
        }))
        .expect("valid bind fixture")
    }

    fn combo(s: &str) -> KeyCombo {
        KeyCombo::parse(s).unwrap()
    }

    #[test]
    fn matches_simple_chord() {
        let b = bind(&["SUPER"], "1", "workspace", "1", "", false);
        assert!(bind_matches(&b, &combo("super+1"), ""));
        assert_eq!(bind_action(&b), "workspace 1");
    }

    #[test]
    fn modifiers_compared_as_set_not_order() {
        // decode_mods order is SHIFT, CTRL; combo order is by BTreeSet.
        let b = bind(&["SHIFT", "CTRL"], "t", "exec", "kitty", "", false);
        assert!(bind_matches(&b, &combo("ctrl+shift+t"), ""));
    }

    #[test]
    fn key_match_is_case_insensitive() {
        let b = bind(&["SUPER"], "T", "exec", "kitty", "", false);
        assert!(bind_matches(&b, &combo("super+t"), ""), "Hyprland may report `T`");
    }

    #[test]
    fn modifier_mismatch_rejected() {
        let b = bind(&["SUPER"], "1", "workspace", "1", "", false);
        assert!(!bind_matches(&b, &combo("ctrl+1"), ""));
        assert!(!bind_matches(&b, &combo("1"), ""), "no-modifier combo must not match");
    }

    #[test]
    fn mouse_binds_excluded() {
        let b = bind(&["SUPER"], "mouse:272", "movewindow", "", "", true);
        assert!(!bind_matches(&b, &combo("super+mouse:272"), ""));
    }

    #[test]
    fn submap_must_match() {
        let b = bind(&["SUPER"], "h", "movewindow", "l", "resize", false);
        assert!(!bind_matches(&b, &combo("super+h"), ""), "global lookup must not hit a submap bind");
        assert!(bind_matches(&b, &combo("super+h"), "resize"));
    }

    #[test]
    fn resolve_finds_first_match_else_none() {
        let binds = vec![
            bind(&["SUPER"], "1", "workspace", "1", "", false),
            bind(&["SUPER"], "2", "workspace", "2", "", false),
        ];
        assert_eq!(resolve(&binds, &combo("super+2"), "").unwrap().arg, "2");
        assert!(resolve(&binds, &combo("super+9"), "").is_none());
    }

    #[test]
    fn action_without_arg_is_just_dispatcher() {
        let b = bind(&["SUPER"], "q", "killactive", "", "", false);
        assert_eq!(bind_action(&b), "killactive");
    }
}
