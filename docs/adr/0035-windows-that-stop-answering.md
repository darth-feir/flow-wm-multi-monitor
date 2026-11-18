# A window that stops answering never wedges the daemon

**Status:** Accepted (stage 41, 2026-11-16). Changes how
`registry::win32::set_foreground_window_impl` pushes (it now runs on a
bounded thread and skips `AttachThreadInput` to a thread that does not
answer), adds the states `Tiling(Hung)` / `Floating(Hung)` beside `Hidden`
(ADR-0027's restore seat brings a tile back), and a responsiveness check in
front of every synchronous Win32 call that would wait on another process.

## Context

**The incident (host, Windows 11 build 26300, 2026-11-13).** Times are local
(the daemon log is UTC, six hours later).

| time | source | event |
|---|---|---|
| 08:57:39 | daemon log | `registered window ... class="Ghost"`, `(unknown) is elevated; left unmanaged`: an application stopped answering five seconds earlier and DWM put its stand-in up; in the same second `restored window: isize=67309302` |
| 08:57:45.156 | keymap log | `dispatch move-to-monitor left`, answered (exit 0) |
| 08:57:46.715 | keymap log | `dispatch focus-monitor right`: no answer |
| 08:57:56.806 | keymap log | `gate: alive=0 (was 1) lastError=121` (the pipe timed out; the process was alive) |
| 08:58:04 | keymap log | `FlowWM daemon is gone`; the daemon logged nothing more; the user rebooted |

The daemon does everything on one thread (threading-model). Any call on that
thread that waits on another process's window thread waits as long as that
thread does, and a frozen program may never answer.

Related symptoms on the host: several times a day
`set_foreground_window: SetForegroundWindow failed for hwnd N (Win32 error
0x0)` followed by `remove_from_layout_and_refocus: SetForegroundWindow
failed for hwnd N` after a hide or a minimize, and twice
`cancel_and_wait: animation worker did not acknowledge within 500ms` and
`SetWindowPos failed ... Access is denied` (for a `CredentialUIBroker.exe`
window, registered as an ordinary window).

## The probe (VM guest, Windows 11 22621)

`guest\probe41.ps1` in the VM harness, daemon stopped: a WinForms stand-in
(`guest\testwin41.ps1`) whose UI thread sleeps on command is activated and
frozen; every call runs on a thread of its own, joined for 1.5 s.

- `IsHungAppWindow` turns true 5.05 s after the freeze (documented: no
  message pumped for 5 s while not waiting for input). Half a second later a
  `Ghost` window (owned by `dwm.exe`, band 1, title "... (Not Responding)",
  same rect) takes the foreground; it reports `IsHungAppWindow` true itself.
  The frozen window stays visible and uncloaked.
- With the window reported hung:

| call on the frozen window | measured | decision |
|---|---|---|
| `IsHungAppWindow`, `GetWindowTextW`, `InternalGetWindowText`, `GetClassName`, `GetWindowRect`, `GetWindowPlacement`, `DwmGetWindowAttribute`, `GetWindowBand`, `GetWindowThreadProcessId`, `IsWindowVisible` | return at once (0-1 ms) | reads stay as they are (`GetWindowText` of another process's window reads the stored title, documented) |
| `SendMessageTimeoutW` `SMTO_ABORTIFHUNG` | returns at once (fails) | the responsiveness probe |
| `PostMessageW` (`close_window`, `post_syscommand`), `ShowWindowAsync` (`minimize_window`) | return at once | already asynchronous; unchanged |
| `SetWindowPos` with `SWP_ASYNCWINDOWPOS` | returns at once (the move lands when the thread answers, after the synchronous ones) | not used: see below |
| `ShowWindow(SW_SHOWNOACTIVATE)` on a visible window | returns at once | still gated: restoring a minimized or maximized window sends messages |
| `SetWindowPos` move, z-order (`insert_below`), `HWND_TOPMOST`, `HWND_NOTOPMOST` | **blocked** until the thread answered (8-13 s) | gated by `IsHungAppWindow` |
| `SetWindowPlacement` | **blocked** | gated |
| `BringWindowToTop` | **blocked** | gated (and probed, below) |
| `SetWindowPos` on another, live window while the foreground is frozen | returns at once | |
| foreground push with the `Ghost` in front: `AttachThreadInput` to its thread | fails with error 5 at once | `Ghost` reports hung: no attach |
| `AttachThreadInput` to the frozen thread, `SetForegroundWindow`, `BringWindowToTop` (live target) | return at once | |

- One second into the freeze (not yet reported hung): `IsHungAppWindow`
  false; `SendMessageTimeoutW` with 200 ms timed out; a synchronous move
  blocked until the thread answered (10.8 s); and the daemon's push
  sequence, attached to the freezing thread, returned from
  `SetForegroundWindow` at once but its `BringWindowToTop` on a **live**
  target held 9.3 s, until the frozen thread answered (with the input queues
  attached the activation waits on that thread; the mechanism is inferred,
  the hold is measured).

**The incident reproduced** (`test-stage41.ps1` case A on the stage 40
build): stand-in focused and frozen, `focus-monitor left` answered,
`focus-monitor right` (whose target is the remembered focus of that screen,
the frozen window) never answered, and every later command and `flow ping`
got `daemon is busy serving another client` until the freeze ended. The
blocked call is the push's `BringWindowToTop` on the frozen target (the
probe's table); case P shows the second path, the attach to a foreground
that is freezing.

## Considered Options

- **Where the check comes from.**
  - **(a) `IsHungAppWindow` (documented) before every call that would wait,
    plus a 50 ms `SendMessageTimeoutW` probe where an attach or a raise is
    about to happen.** Chosen. It is the signal Windows itself uses for
    ghosting.
  - (b) A timeout on every call. Rejected: Win32 has no timeout for
    `SetWindowPos`; only another thread could bound it, and a call on the
    main thread cannot be abandoned.
- **`SWP_ASYNCWINDOWPOS`.** Not used. Measured: an asynchronous move lands
  after any synchronous one sent later, so it can undo a later placement
  (the reason `move_window_keep_size` is synchronous, ADR-0023); the
  animator paces frames with `DwmFlush` and expects each frame applied; the
  floating layer and book mode read the z-order back on the next pass. Every
  site that touches a foreign window was reviewed (table below); none can
  take a posted result, so all stay synchronous behind the gate.
- **What happens to a hung window.**
  - **(a) Parked: out of the layout, left where it is, restore seat
    recorded; back to the seat when it answers (no view follow).** Chosen;
    the minimize / hide path of ADR-0027, without the foreground push (the
    `Ghost` holds the foreground, and the user may be waiting on that
    program; a push from the poll has no foreground right anyway).
  - (b) Keep its slot and only skip its moves. Rejected: every scroll would
    slide the neighbours over the frozen window.
  - (c) `Ignored::Hung`. Rejected: an ignored window has no way back to a
    seat, and its restore is the point.
- **The guard rail.**
  - **(a) The push runs on a `foreground-push` thread; the main thread
    waits 1 s and abandons it.** Chosen. Nothing the main thread holds is
    needed by the push, and an abandoned push only detaches afterwards (it
    checks a shared state before `SetForegroundWindow` and the raise).
  - (b) A watchdog thread over the main loop. Rejected: it can report a
    stuck main thread but cannot answer the pipe or undo the call.

## Decision

The call inventory: every call that can wait on another process's window
thread.

| site | call | can block on a foreign thread? | decision |
|---|---|---|---|
| `win32::push_now` (every foreground push) | `AttachThreadInput` + `SetForegroundWindow` | yes, when the foreground thread does not answer (measured, P) | no attach when the foreground is hung or misses a 50 ms `WM_NULL`; push on the bounded thread |
| `win32::push_now` | `BringWindowToTop` (target) | yes (measured) | target refused when hung (`ForegroundPush::NotResponding`); not raised when it misses the 50 ms probe |
| `animation::backend::win32::place_window` (animator frames, teleports, rescue, fail-back) | `SetWindowPos` (sync) | yes (measured) | skipped when hung; frames stay synchronous (pacing) |
| `win32::set_topmost`, `insert_below`, `insert_below_quiet` (floating layer, piles, fullscreen) | `SetWindowPos` z-order (sync) | yes (measured) | skipped when hung |
| `win32::move_window_keep_size` (pre-place) | `SetWindowPos` (sync) | yes | skipped when hung (it already required `answers_within`) |
| `win32::show_window_restored`, `maximize_window` (hotplug, adoption, fail-back) | `ShowWindow` | yes when the state changes (sends messages) | skipped when hung |
| `win32::set_window_placement` (fail-back) | `SetWindowPlacement` | yes (measured) | skipped when hung (error) |
| `win32::answers_within` (ADR-0023) | `SendMessageTimeoutW` `SMTO_ABORTIFHUNG` | bounded | unchanged |
| `close_window`, `post_syscommand`, `minimize_window` | `PostMessageW`, `ShowWindowAsync` | no (measured) | unchanged |
| `get_window_text`, `get_class_name`, rect / placement / DWM / band reads | reads | no (measured) | unchanged |
| `borders::Border::set_geometry` | `SetWindowPos` on the daemon's own overlay, inserted after the target | no message to the target's thread (inferred: only the moved window is sent `WM_WINDOWPOSCHANGING`); case A ran with the frozen window ringed | unchanged |
| the animator worker | waits only in its own `SetWindowPos` (above) | a call made before the hang is reported stays blocked until the program answers | residual, see *Consequences* |

The not-responding state:

- `FlowWM::reconcile_hung` (`daemon::hung`) runs on the foreground sync
  tick (250 ms) and before every command that is not a query or `ping`; a
  foreground event for a parked window brings it back at once.
- Parked: `window 0x.. (exe) is not responding (IsHungAppWindow); parked
  where it is` (info), state `Tiling::Hung` / `Floating::Hung`, mode `hung`.
  Back: `window 0x.. (exe) answers again; back from not responding`.
- Every gated wrapper logs `<call>: hwnd 0x.. is not responding; left
  alone` at debug; the push logs `set_foreground_window: hwnd 0x.. not
  pushed, it is not responding (IsHungAppWindow)` and `foreground 0x..
  (exe "class") is not responding; hwnd 0x.. pushed without
  AttachThreadInput` at debug.
- Guard rail: `foreground push to hwnd 0x.. (exe "class") abandoned: no
  answer within 1000 ms; the foreground was hwnd 0x.. (exe "class")`
  (warn, and the IPC request's error), later `foreground push: the abandoned
  push to hwnd 0x.. returned (...)` (info). Four abandoned pushes still
  waiting: new pushes are refused with a warning.
- `FLOW_TEST_BYPASS_HUNG_GATE=1` turns the gate and the probe off (test
  only, logged as a warning) so the VM suite can show the guard rail.

**The refused successor push.** The WARN pair was not reproduced in the
guest, on either build (case D): a focused tile hiding itself, hidden by
another process, minimized, and hidden while a window of the other screen
was the previous foreground each handed the foreground to the successor
without a refused push. The new trace lines show what the push meets at a
hide: the first time, *no foreground window at all* (the hidden window has
let go and Windows has not activated the next one yet), so the push is the
direct call; later ones found the hiding window still the foreground and
attached to it. The push now logs, at debug, the foreground it found, its
owner, whether the attach succeeded, and the foreground at the moment
`SetForegroundWindow` returned FALSE, which is what the host log needs to
name the cause. No retry and no injected input were added (ADR-0033), and
the cause is not fixed: it is not known. Inferred, not measured: on the
host the direct call in the no-foreground gap is refused by the foreground
lock (no input reached flowd, error code 0), and Windows then activates the
next window in z-order itself, which is the successor, so "focus lands on
it a moment later". To be read from the host's debug log.
*Resolved by ADR-0037 (stage 43):* the guest had no user input, so the
foreground lock never applied; with a real key press before each action
the pair is reproduced (a focused fullscreen column hiding itself: the
direct push in the no-foreground gap is refused, error 0). The inference
above holds for that gap; a second cause is a push past an untracked
foreground the user chose.

**The elevated target.** The host's denied `SetWindowPos` was for a
`CredentialUIBroker.exe` window that the elevation gate (ADR-0025,
`TokenElevation`) let through. Case E could not raise that dialog in the
guest; the gate is unchanged and the case is left open.

## Consequences

- A frozen program no longer takes the daemon with it: commands, new
  windows and closes keep working, the frozen window stays where it froze,
  and it comes back to its column when it answers.
- While a tile is parked the strip's focus is its successor but the
  foreground is the `Ghost`; the border marks the successor.
- Each push costs up to two `WM_NULL` round trips (well under 1 ms for an
  idle thread; at most 50 ms each for a busy one).
- Residual, inferred: a call made in the first five seconds of a freeze,
  before Windows reports it, still waits. On the main thread that is a
  teleport or a z-order change racing a freeze (the push is covered by the
  probe and the guard rail); on the animator thread it stalls animations
  (the main thread waits 500 ms, `cancel_and_wait`) until the program
  answers.
- `IsHungAppWindow` itself is not a guarantee of anything the program
  does next; a window that alternates between answering and not answering
  moves in and out of its strip at the poll's pace.

## Tests

Unit tests: `registry/core.rs`
(`a_window_that_stops_answering_round_trips_through_hung`), `daemon/hooks.rs`
(`a_tile_that_stops_answering_is_parked_and_comes_back_to_its_column`,
`a_float_that_stops_answering_keeps_its_rect`), `registry/push_guard.rs`
(`a_push_past_the_bound_is_abandoned_and_the_next_one_gets_a_fresh_thread`).
VM: `guest\probe41.ps1` (the call table) and `test-stage41.ps1` (A parked
and commands answer, B back in its column, P before the hang is reported,
C the guard rail, D the successor push after a hide or minimize, E the
credential dialog probe) in the VM harness.
