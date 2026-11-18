# The successor push follows the real foreground

**Status:** Accepted (stage 43, 2026-11-17). Changes the removal pipeline
(`remove_from_layout_and_refocus`): before the strip's successor is pushed,
the foreground Windows reports is read once and decides whether there is
anything to push; a minimized or hidden successor is passed over. Resolves
the "refused successor push: cause unknown" note of ADR-0035.

## Context

**Measured on the host** (read-only analysis of `flowd-*.log`, 2026-11-07
to 2026-11-17, the builds installed in that period): 74 occurrences of the WARN
pair

```
set_foreground_window: SetForegroundWindow failed for hwnd N (Win32 error 0x0)
remove_from_layout_and_refocus: SetForegroundWindow failed for hwnd N
```

with no visible effect (the focus ended on a sensible window anyway). The
line before the pair: 48 times `reconcile_visibility: ... → Hidden`, 10
times `minimized window`, 6 times an `AttachThreadInput` failure in the same
push, 4 times a `registered window` (explorer, Teams, SearchHost), once a
`removed window` (mstsc). Of the 27 hidden windows that could be
identified, 12 had been made a fullscreen column earlier (Teams, SSMS,
mstsc). The host daemon ran at info level throughout (no debug line in any
log), so the stage 41 trace lines are not available from the host.

**Reproduced in the VM** (`test-stage43.ps1`, stand-ins
`guest\testwin43.ps1`, WinEvent recorder `guest\probe43-events.ps1`) on the
stage 42 build. The deciding condition is **user input**: Windows enforces
the foreground lock only within `ForegroundLockTimeout` (200 s on host and
guest) of the user's last input. The earlier VM attempts (stage 41 case D,
and this suite's first two runs) drove the guest without any input, so
every push was allowed and the WARN never appeared. With one real key press
(Hyper-V keyboard, Ctrl) before each action, the stage 42 build gave:

| case | what happened | stage 42 build |
|---|---|---|
| B2 a focused fullscreen column hides itself | at the push **no window held the foreground** (the hidden window had let go, Windows had not activated the next one yet); the direct `SetForegroundWindow` was refused, error 0; Windows then activated a tile of the other screen and the stage 30 handoff sent the focus back to the successor | **the WARN pair**, focus right in the end |
| D2 a focused tile closes | the same gap (`no foreground window; direct push`) | the WARN pair in one run of two (a race with Windows' own activation) |
| G an untracked window (a tool window) of the user holds the foreground when the last-focused tile hides | the registry focus still named the hidden tile (`set_focused` ignores untracked windows), so the removal counted it as the user's focus and pushed the successor past the user's window | the push **succeeded: the user's window lost the foreground** |
| G2 the same, the tool window elevated | the attach to its thread was denied (UIPI) and the push still went through | the user's window lost the foreground |
| C the successor is minimized before flowd handles the hide (flowd held still for 600 ms) | the successor was pushed while minimizing | a push to a minimizing window, then a second push |
| A the app replaces its window (a new window takes the foreground, the old one hides 50 ms later) | events arrive in order; the new window's foreground was handled first, so nothing was pushed | correct, no WARN |
| B1 / F the other screen's window took the foreground before the tile hid | no push | correct |

The fullscreen pattern of the host (a fullscreen column, often mstsc on the
other screen, goes away) is case B2: a fullscreen window is the
foreground, so its hide always opens the no-foreground gap, and the next
window in z-order Windows activates is usually on the other screen.

The two 2026-11-17 host occurrences that looked like a window swap
(`registered window: HWND(0xc023a)` then `window isize=787002 → Hidden`, and
`0xc90ba0` then `13175712`) are one window each: the hex and decimal handles
are the same number. A Windows Terminal window was registered, took the
foreground and hid itself within a second, which is case B2 / D2 (the
no-foreground gap), not a swap.

Inferred, not measured: that a share of the remaining host pairs is case G
under a foreground that refuses the push (a push past a window the user
chose, which Windows correctly refused); the host logs carry no debug
lines to tell the two apart.

## Considered Options

- **(a) Read the foreground once before the push and decide from it.**
  Chosen; the rule is below.
- (b) Retry the push. Rejected: the refusal is Windows' foreground lock
  doing its job (flowd did not receive the last input); a retry is a fight
  with it, and in case G the push was wrong to begin with.
- (c) `SendInput` (a synthetic key) to gain the foreground right. Rejected:
  injected input reaches the user's foreground window and is a band-aid
  around the lock (ADR-0033).
- (d) `AttachThreadInput` to whatever window holds the foreground now and
  push again. Rejected: that is how case G stole the user's window; in the
  no-foreground gap there is nothing to attach to.
- (e) Per-app rules (mstsc, Windows Terminal, Teams). Rejected: the
  mechanism is the foreground state, not the app.
- (f) Make `on_focus_changed` drop the registry focus whenever an untracked
  window takes the foreground. Rejected for now: commands issued from an
  untracked dialog of an app act on the app's last tracked window today,
  and border colouring, `focus-*` and the drift poll all read that focus.
  The removal reads the live foreground instead, which is the only place
  the stale focus did harm.

## Decision

### 1. The foreground decides the push

`hooks::successor_push_decision(removed, successor, monitor, seen,
recent_loss)` (pure, unit-tested) on `ForegroundSeen { hwnd, shell,
visible, monitor, already_focused }` read by `FlowWM::push_successor`:

| foreground | verdict |
|---|---|
| none | push (`no foreground window`) |
| the removed window | push (`the removed window still holds the foreground`) |
| the successor | nothing to push; registry focus = successor |
| a shell surface (shell class or shell band, ADR-0033/0034: Start, search, flyouts, taskbar, desktop) | push (`a shell surface holds the foreground`); not the user's pick of a window |
| hidden or cloaked | push (`the foreground window is not visible`) |
| a managed window of **another** monitor whose foreground event was not handled yet, or that the removed window lost the foreground to a moment ago (destroy / minimize, stage 30) | push (`Windows handed the foreground to another screen`): stage 30's rule, the focus stays on the screen the tile left |
| anything else: a window of the same monitor, a float, a window flowd does not manage (a dialog, a tool window, an elevated window), or another screen's window the user went to before the removal | **skipped**: that window is the user's (or the app's) choice |

A skipped push adopts the foreground: the registry focus names it when it
is tracked; an untracked one only clears the registry focus when that still
named the removed window. A tile or float of the same monitor becomes the
handoff's successor.

### 2. A minimized or hidden successor is passed over

`FlowWM::visible_successor`: when the strip's successor is minimized,
hidden, cloaked or gone at that moment (its own event is still queued), the
nearest tile that is not takes its place, in the strip's own successor
order (`nearest_usable_successor`: the other rows of its column, then the
columns to the left nearest first, then to the right), and the strip's
focus follows it. With no visible tile left nothing is pushed and the stage
30 handoff treats the strip as empty (focus on the desktop). The successor
is not restored: a minimize is the app's or the user's, and the existing
focus commands skip hidden windows the same way (`resolve_focus_target_with`).

### 3. Log lines

All at debug unless noted.

- `successor push: 0x.. pushed (<reason>)` with the reason of the table.
- `successor push skipped: the foreground is already 0x.. (exe "class") on
  monitor N; 0x.. not pushed` (`monitor ?` when the window is on none).
- `successor 0x.. already holds the foreground`.
- `successor 0x.. is minimized or hidden; 0x.. on monitor N takes its
  place` / `... and no other tile of monitor N is visible; nothing is
  pushed`.
- A refused push is judged from the foreground read right after it:
  another window holds it: `successor push refused (Failed(0)): the
  foreground is 0x.. (exe "class") on monitor N; left there` (adopted,
  unless the push was the stage 30 handoff, which sends it back); no window
  holds it: `successor push refused (...): no window holds the foreground
  (the foreground lock); Windows activates the next window itself, the
  handoff keeps it on monitor N` (the measured gap of case B2).
- **WARN** only when the removed window itself still holds the foreground
  and the attached push was refused, which no measurement explains:
  `remove_from_layout_and_refocus: successor push to 0x.. refused (...); the
  foreground was 0x.. (exe "class") when it started (<reason>) and is 0x..
  (exe "class") now`.
- The push runs the quiet form of the foreground push
  (`registry::win32::push_foreground_window_quiet`), so its own
  `set_foreground_window: SetForegroundWindow failed` line is debug; the
  stage 41 trace lines (foreground found, owner, attach result, foreground
  when `SetForegroundWindow` returned FALSE) are unchanged and precede the
  verdict.

No query field or state changed.

### Documented vs inferred

Documented: `SetForegroundWindow`'s conditions (the caller must be the
foreground process, have received the last input event, or the lock must
be off; `ForegroundLockTimeout` turns the lock off after that long without
input). Measured: the no-foreground gap after a hide and its refusal under
recent input (B2, D2); the push past an untracked foreground (G, G2); the
push to a minimizing successor (C). Inferred: that the host's remaining
pairs are the same two mechanisms (no host debug log exists to confirm).

## Consequences

- The fullscreen and close cases keep their focus behaviour (successor,
  stage 30 handoff); their refusal in the gap is a debug line, not a WARN.
- A window the user is in that flowd does not track (dialogs, tool windows,
  elevated windows) keeps the foreground when a tile it covers closes or
  hides; before, flowd took it away (or the push was refused with the WARN
  pair).
- After a refusal in the gap, a window Windows activates on the same screen
  keeps the focus even if it is not the strip's successor (the handoff only
  sends back another screen's window).
- Residual: when the removed window itself still holds the foreground and
  the attached push is refused the WARN stays, with both foregrounds named.

## Tests

Unit tests in `daemon/hooks.rs` (`successor_push_tests`: the verdict table
and `nearest_usable_successor`). VM: `test-stage43.ps1` with
`guest\stage43-lib.ps1`, `guest\testwin43.ps1` and
`guest\probe43-events.ps1` in the VM harness (cases A, B1, B2, C, D, D2, E,
F, G, G2, Z; a real Ctrl press through the Hyper-V keyboard before each
action puts the guest under the foreground lock, and `Freeze43` /
`Thaw43` hold flowd still with `NtSuspendProcess` so that case C's
minimize is queued behind the hide).
