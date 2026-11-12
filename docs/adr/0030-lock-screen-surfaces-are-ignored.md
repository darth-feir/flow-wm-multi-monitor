# Lock-screen surfaces are ignored

**Status:** Accepted (stage 36, 2026-11-10). Extends the bundled rules
(`default-flow-rules.toml`) and the shell-class list of the floating layer
(`daemon/top_layer.rs`, `SHELL_CLASSES`; stage 29b, *Shell Surfaces Are
Never Learned*). Rule precedence is unchanged
(`docs/src/dev-guide/classification.md`).

## Context

On the host, locking the session shifted the lock-screen picture sideways,
most of the way off the screen. The daemon log at the lock (2026-11-07,
18:39:17Z):

| window | exe | what the daemon did |
|---|---|---|
| `LockScreenBackstopFrame` | explorer.exe | registered as a fresh float |
| `LockScreenInputOcclusionFrame` | explorer.exe | registered as a fresh float, then `WARN set_foreground_window: SetForegroundWindow failed` for it |
| `Windows.UI.Core.CoreWindow` | LockApp.exe | registered `Ignored(Fullscreen)` |

Windows creates the two explorer frames on the interactive desktop when the
session locks, then shows LockApp's window over them. No bundled rule, no
registry gate and no shell class named the frames, so with the user's
`default_action = "float"` the creation pipeline treated them as a new app:
natural size, centred on the focused screen's work area, registered in the
floating layer (topmost), animated, and the foreground pushed to them. The
frames stayed tracked until a later lock two days on. LockApp's window was
left alone only because its rect happened to equal the primary screen
(`win32::is_fullscreen`); its entry never left `flow query all`, because
LockApp hides that window at unlock and does not destroy it, and a hidden
ignored window stays in the registry. A LockApp window that does not cover
the primary screen exactly (another primary, a scaling change) would have
been floated too.

## Considered Options

- **(a) Bundled ignore rules plus shell classes.** Chosen.
- **(b) A creation gate in the registry (never register these windows).**
  Rejected: it would be a second, hidden classification path beside the
  rules. A rule is visible in the defaults file, can be outranked by the
  user, and goes through the same logging and query output as every other
  ignored window.
- **(c) Pause tiling while the session is locked
  (`WTS_SESSION_LOCK`).** Rejected for this fix: the frames are created
  before or with the notification, and the daemon would still have to know
  which windows are the lock screen to leave them alone afterwards. It
  would also be new machinery for a problem two rules solve.
- **(d) Give lock surfaces precedence over user rules.** Rejected: the
  bundled rules are defaults by design (user > learned > default); a user who
  writes a rule for explorer.exe gets what that rule says.

## Decision

- **Rules.** Two bundled rules in `default-flow-rules.toml`, section *Lock
  screen*:
  `match = { exe = "LockApp.exe" }` -> `ignore`, and
  `match = { exe = "explorer.exe", class_regex = '^LockScreen' }` -> `ignore`.
  The exe match is case-insensitive; the class regex is case-sensitive and
  matches both frames. They are default rules, so they are consulted after
  user and learned rules and before `default_action`, whatever that is.
- **Precedence, unchanged.** A user rule that matches these windows wins:
  `exe = "explorer.exe"` with no class floats (or tiles) the lock frames
  too. A user rule scoped by class (`class = "CabinetWClass"`) does not touch
  them. This is documented, not guarded.
- **Shell classes.** `LockScreenBackstopFrame` and
  `LockScreenInputOcclusionFrame` join `SHELL_CLASSES`; LockApp's
  `Windows.UI.Core.CoreWindow` is already there. So: a learned rule keyed on
  them is never handed to the pipeline (`learned_rules_for_pipeline`), a
  `set-window` while one holds the foreground is not aimed at it and records
  nothing (`os_foreground_for_dispatch`, `record_learned_transition`), the
  floating layer leaves its decision alone
  when one takes the foreground, and one is never made a member of the layer
  (never topmost), even when a user rule floats it.
- **No `set-window` target.** The foreground hook records an ignored window
  that takes the foreground as the registry's focus, and `set-window` falls
  back to that focus when the foreground is a shell surface; the VM showed
  it then adopted the lock frame as a tile. `resolve_set_window_target` now
  skips a registry focus whose class is a shell class too: `set-window`
  answers "no focused window" and nothing moves.
- **Going away.** A lock surface (`registry::classification::is_lock_surface`:
  LockApp.exe, or explorer.exe with a class starting `LockScreen`) that is
  ignored and stops being user-visible (hidden, cloaked or iconic) is dropped
  from the registry on the hide / cloak event
  (`WindowRegistry::drop_hidden_lock_surface`). Shown again, it registers
  anew through the usual show / uncloak / foreground recovery. Other hidden
  ignored windows keep their entry as before: dropping every one was judged
  too wide a change for this stage.
- **Logging.** A lock surface registered as ignored logs one debug line
  (`lock surface 0x.. (exe "class") ignored: ...`) besides the usual info
  `registered window` line; dropping one logs at debug. Nothing at WARN.

## Consequences

- The lock screen is left entirely to Windows: no move, no animation, no
  foreground push, no topmost flag, nothing learned.
- A user rule matching `exe = "explorer.exe"` without a class brings the
  bug back for the frames (they are floated or tiled), though they still
  never become topmost and are still never learned. Scope such a rule by
  class.
- An ignored lock frame is no longer in the layout's way, but stays in
  `flow query all` while the session is locked; a destroyed frame leaves as
  any destroyed window does.
- The ordinary explorer windows are untouched: the class regex needs the
  `LockScreen` prefix.

## Tests

Unit tests: `registry/classification.rs`
(`embedded_default_rules_ignore_lock_screen_surfaces`: both frames and
LockApp are `Ignored(ExplicitRule)` under `default_action` float and tile,
an ordinary explorer window falls through;
`user_explorer_rule_outranks_the_lock_screen_rules`), `daemon/dispatch.rs`
(`learned_rules_for_pipeline_skips_lock_screen_surfaces`,
`set_window_on_a_focused_shell_surface_is_refused`),
`registry/core.rs` (`drop_hidden_lock_surface_only_drops_ignored_lock_surfaces`),
and the shell-class test in `daemon/top_layer.rs` that walks
`SHELL_CLASSES`. VM: `test-stage36.ps1` in the VM harness with
`guest\testwin36.ps1`, a Win32 stand-in (the guest shows no LockApp lock
screen): one exe that registers a window class of any name and creates
`WS_POPUP` windows shown without activation, run as copies named
`explorer.exe` (classes `LockScreenBackstopFrame`,
`LockScreenInputOcclusionFrame`, and an ordinary class) and `LockApp.exe`
(class `Windows.UI.Core.CoreWindow`, on the primary screen's rect and 10 px
off it). The real lock screen was not exercised.
