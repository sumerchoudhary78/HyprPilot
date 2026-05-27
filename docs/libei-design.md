# libei migration — design doc

> **STATUS (2026-05-27): SHELVED — blocked upstream, and unnecessary on
> Hyprland.** The v0.7 scaffold (backend trait + env-var selector + empty
> stub) stays in the tree, but M1–M6 are **not** being implemented. Two
> findings killed it:
>
> 1. **No portal.** `org.freedesktop.portal.RemoteDesktop` is not
>    implemented by `xdg-desktop-portal-hyprland`
>    ([upstream #252](https://github.com/hyprwm/xdg-desktop-portal-hyprland/issues/252),
>    open since Aug 2024). Confirmed locally: `busctl` reports "No such
>    interface"; `strings` on xdph 1.3.12 finds zero RemoteDesktop/EIS
>    symbols. No repo upgrade provides it.
> 2. **It wouldn't help anyway.** The only community RemoteDesktop impl
>    (`xdg-desktop-portal-hypr-remote`) bridges EIS back down to
>    `zwp_virtual_keyboard_v1` — the *same* protocol wtype uses, which
>    Hyprland filters out of its bind matcher. So libei-via-portal would
>    **not** make `super+T` trigger binds, the entire reason for the
>    migration.
>
> **What we did instead (v0.8):** route `press_keys` through **ydotool**
> (uinput→libinput), which Hyprland's bind matcher *does* honor — verified
> live: synthesized `super+T` fires the `bind`. `type_text` stays on wtype.
> See [[libei-dead-use-ydotool]] memory and the input crate's `keys.rs` /
> `runner.rs`.
>
> **Revisit this doc only if** xdph ships a real libinput-backed
> RemoteDesktop (not a virtual-keyboard bridge). The premise below assumes
> that; it does not hold today.

---

Original design (v0.7 scaffold → intended v0.8 implementation):

## Why libei

HyprPilot today synthesizes keyboard input via `wtype`, which talks to
the wlroots protocol `zwp_virtual_keyboard_v1`. Hyprland accepts those
events and delivers them to the focused client, but its global-bind
matcher **filters virtual-keyboard events out before the bind table is
consulted**. So:

```
hyprpilot input shortcut super+T   # via wtype
  -> the focused window sees Super+T as a literal keypress
  -> Hyprland's `bind = SUPER, T, exec, kitty` does NOT fire
```

This is observed and reproducible on Hyprland 0.45+. The current
workaround we recommend in CLAUDE.md is:

```
hyprctl dispatch exec kitty       # bypasses the bind table entirely
```

which is fine for `exec` binds but useless for any bind whose payload
is a dispatcher Hyprland exposes only via the bind table (e.g. user
chord that runs a script).

libei (the *Emulated Input* protocol exposed via
`org.freedesktop.portal.RemoteDesktop`) routes synthesized events
through `libinput`. From Hyprland's perspective they're
indistinguishable from a real keyboard — they hit the bind matcher
**and** the focused client. Same protocol GNOME and KDE accelerate
RDP-style remote-control over.

## Portal handshake (the part we cannot skip)

The user-visible behaviour is: HyprPilot pops a portal dialog on first
use of input synthesis after daemon start; the user clicks
"Allow"; HyprPilot keeps the session for as long as the daemon
runs. No prompt for subsequent operations within that session.

The wire dance, via `xdg-desktop-portal-hyprland`:

1. **CreateSession** on `org.freedesktop.portal.RemoteDesktop`.
   Returns a session handle (D-Bus object path).
2. **SelectDevices** on that handle with the device bitmask
   (`KEYBOARD | POINTER` = `0b011` for v0.8 M2–M4).
3. **Start** — this is when the portal raises the dialog asking the
   user to approve. Blocks (in async terms) until the user responds.
4. **ConnectToEIS** — returns an `int` file descriptor (the EIS
   socket). The daemon hands this fd to `reis` and from there speaks
   the EIS protocol directly.
5. EIS handshake: version negotiation, then `ei_seat`-bound device
   announcements (keyboard / pointer / pointer-absolute as needed),
   then we're free to send events.

After step 4 the D-Bus connection can be parked — the EIS fd is the
hot path. We do still need to keep the D-Bus session object reachable;
dropping it (or closing the D-Bus connection that owns it) tears down
the portal session and forces a re-prompt next time.

## Parity matrix

| `InputRunner` method | wtype / ydotool today | libei target (v0.8) | Notes |
|---|---|---|---|
| `type_text(&str)` | `wtype -` | EIS `ei_keyboard.key` events per char, with modifier hold | Char→keysym→keycode mapping needs an xkb table; reis has helpers. Latency per-char will be higher than wtype's batched write — we may need to coalesce. |
| `press_keys(&KeyCombo)` | `wtype -M ctrl -M shift -k t` | EIS modifier-down → key-down → key-up → modifier-up | This is the one that *exists* to fix the global-bind issue. M3 milestone. |
| `mouse_move(x, y, absolute)` | `ydotool mousemove [-a] -- x y` | EIS `ei_pointer.motion` (relative) or `ei_pointer_absolute.motion_absolute` | Absolute path requires the portal to publish an `ei_device` with the `ei_pointer_absolute` interface. Not all portals expose it. |
| `mouse_click(MouseButton)` | `ydotool click 0xCN` | EIS `ei_button.button` press + release | BTN_LEFT/RIGHT/MIDDLE map cleanly; X1/X2 are `BTN_SIDE`/`BTN_EXTRA` per Linux input-event-codes.h. |
| `shortcut` (via Hyprland `sendshortcut` dispatcher) | unaffected — Hyprland-native | unchanged | Not in scope. `sendshortcut` already triggers binds because it's the compositor running it. |

Honest assessment of the hard bits:
- **Modifier latching.** EIS expects explicit modifier press/release
  bracketing each key. The wtype `-M ctrl -k t` shorthand bundles
  that; libei needs us to build it manually. Easy to get wrong on
  parse-then-emit because BTreeSet iteration order isn't kernel-order.
- **Char → keycode for `type_text`.** wtype calls into libxkbcommon
  internally. We'll need the same — either pull `xkbcommon` directly
  or pay reis to do it (check whether reis 0.6 exposes a helper; if
  not, this is its own sub-task).
- **No focus model.** EIS events are seat-wide. They land on whatever
  window has keyboard focus *right now* — same model as wtype, just
  through a different pipe. If the user is staring at the screen and
  focus changes mid-`type_text`, characters split between windows.
  Already true today; not a regression.

## Daemon-lifecycle implications (new vs wtype/ydotool)

This is the substantive new operational requirement. Today:

- `wtype` is a one-shot subprocess; needs `$WAYLAND_DISPLAY`. Done.
- `ydotool` talks to `ydotoold` over a socket; the user is expected
  to start `ydotoold` separately (a systemd unit, usually).

With libei the daemon itself becomes the portal client. That means:

1. **The daemon must run inside the user's graphical session.** Same
   `$WAYLAND_DISPLAY` and `$DBUS_SESSION_BUS_ADDRESS` the user's apps
   see. A root-owned system daemon will not work — the portal is
   per-user. We already recommend launching `hyprpilotd` from the
   user's shell or a `systemd --user` unit; document this hardening.
2. **The portal session is long-lived.** First call into `type_text`
   triggers `CreateSession + Start`, which raises the dialog. We
   **must not** re-create the session for subsequent calls — that
   re-prompts the user and ruins the UX. Cache the session on
   `LibeiBackend` for the daemon's lifetime.
3. **Re-prompt on user logout / compositor restart.** When the
   compositor goes away, the portal session dies. The daemon should
   detect the D-Bus error and lazily re-establish on next call
   (one new prompt; acceptable).
4. **Headless daemons / CI need a workaround.** No portal → libei is
   unusable. The wtype path is the fallback. The cargo feature stays
   default-off so headless builds don't pull `reis` for nothing.

## Failure modes

| Failure | Surface |
|---|---|
| Portal not running | `LibeiBackend::detect` → `BackendMissing("libei portal")` |
| User clicks Deny on the dialog | `BackendFailed { backend: "libei", stderr: "portal denied" }` |
| EIS fd closed mid-session | Auto-reconnect (one prompt) or surface as `DaemonNotReachable("libei")`; TBD |
| Char → keycode lookup miss for `type_text` | `InvalidCombo` (we already have it) with the char in the message |
| Modifier the portal refuses to claim | `BackendFailed` with the modifier name |

## Tests we'll need

Unit-testing the EIS handshake is infeasible — it talks to a real
portal. The bar:

- **Unit tests** for the keysym/keycode mapping table (pure function).
- **Unit tests** for the modifier-bracketing event sequence
  (assert: down-events precede the key, up-events follow it, in
  reverse press order). Pure-function-able if we tease the event
  builder out.
- **Integration test, gated.** A `#[ignore]`-by-default test that
  expects a real portal and a real compositor; CI doesn't run it.
  Locally: `cargo test -p hyprpilot-input --features libei -- --ignored libei_smoke`.
- **Manual parity script.** A short doc in `docs/libei-design.md`
  with the five `hyprpilot input ...` commands to run by hand before
  declaring v0.8 shippable.

## v0.8 milestones

- **M1 — portal session.** `LibeiBackend::detect` does
  `CreateSession + SelectDevices(keyboard|pointer) + Start +
  ConnectToEIS` and stashes the EIS context. No event emission yet.
  Behind the same `libei` cargo feature. Returns
  `NotImplemented { op: "type_text" }` for everything user-visible.
- **M2 — `type_text`.** keysym mapping + char-by-char emission.
  First milestone where libei actually moves characters.
- **M3 — `press_keys` with modifiers.** Modifier bracketing.
  **This is the milestone that makes `super+T` trigger Hyprland
  binds — the whole point of the migration.**
- **M4 — `mouse_move` + `mouse_click`.** Probe for
  `ei_pointer_absolute`; fall back to relative motion deltas if the
  portal doesn't publish it.
- **M5 — parity tests.** Manual smoke script + the integration
  test gated on real portal.
- **M6 — default-on.** Flip the cargo feature default to `["libei"]`
  and the env var default to `libei`. wtype/ydotool stay as a
  fallback, selectable via `HYPRPILOT_INPUT_BACKEND=wtype`. README
  and CLAUDE.md updated.

## Open questions

- Does `reis 0.6` expose char→keycode helpers, or do we pull
  `xkbcommon` ourselves? (Check before M2.)
- Do we want `ei_pointer_absolute` (cleaner mouse_move) hard-required
  or graceful-degrade to relative? Pragma: degrade, log once.
- Portal-Hyprland is still pre-1.0 — what's the failure mode when
  the user runs Hyprpilot on a Hyprland version older than the one
  that ships RemoteDesktop support? Probably "device list is empty";
  we should surface that distinctly from "portal missing".
- Should `BACKEND_ENV` become a profile-config key instead of just an
  env var once M6 lands? Env var is fine for v0.7; revisit before
  default-on.
