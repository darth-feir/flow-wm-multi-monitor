# An app that has a full screen of its own is asked for it, and its own switches are followed

**Status:** Accepted. Adds a rule option to `flow-rules.toml` (`fullscreen =
"frame" | "app"`, default `"frame"`), makes a rule's `action` optional, ships a
bundled rule for Remote Desktop, and gives the keymap a hook re-install for
Remote Desktop's full screen. `set-window fullscreen` for every other window
is unchanged.

`set-window fullscreen` (Mod+F) sizes a window's frame over its monitor's
screen rect and keeps it there (stage 10, `daemon::fullscreen`). For a window
under a `fullscreen = "app"` rule it instead:

- **enters** by posting `WM_SYSCOMMAND SC_MAXIMIZE`, what the title-bar
  maximize button sends, and waiting (15 ms polls, at most 2 s) until the
  window covers its monitor without caption or thick frame; then it takes
  the same bookkeeping as any fullscreen window (`FlowWM::fullscreen`, the
  workspace's floating space at the screen rect, topmost, no border, `mode:
  fullscreen` in `flow query all`) and records the window in
  `FlowWM::app_fullscreen` with the captioned frame's invisible borders. If
  the app does not get there the command fails and nothing moves;
- **leaves** (Mod+F again, `set-window tile|float|cycle`, `tiling off`,
  `flow stop`, the fail-back) by posting `SC_RESTORE` when the window is
  still chromeless, waiting for the chrome to come back, putting the
  borders back (the measured ones, or the remembered ones when DWM still
  reports none), and then taking the usual exit path into its old slot;
- **adopts** the app's own switch: a watched window (one whose rule says
  `"app"`; the hook callback forwards its `EVENT_OBJECT_LOCATIONCHANGE`, and
  `STATECHANGE` reaches the same check) that is an active tile or float and
  turns chromeless over its monitor enters fullscreen without a request; one
  in app fullscreen whose chrome comes back leaves it and is re-seated
  without one. The decision is the pure `app_fullscreen_step`.

While a window is in app fullscreen the daemon never places its frame: the
batch of its floating space skips it while its target equals the rect last
sent (the app's own rect is recorded as sent on entry). Moves that are
real (a workspace parking it, `move-to-monitor`) are still sent, as pure
moves with zero invisible borders, so the window lands exactly where the
app put it, shifted.

## Context

Remote Desktop (`mstsc.exe`, frame class `TscShellContainerClass`) shows
scrollbars (on its `UIMainClass` child) whenever its client area is smaller
than the remote desktop. Measured in the dev VM against a 1920x1080 session
(notes `flowwm-vm/notes/rdp-target.md`): tiled at 936 px, scrollbars; frame
fullscreen (client 1918x1048, caption kept), scrollbars; `ShowWindow
(SW_MAXIMIZE)` (client 1920x1009), scrollbars. Only `SC_MAXIMIZE` makes it
switch to its own full screen: caption and thick frame removed, rect
0,0,1920,1080, connection bar, no scrollbars. FlowWM did not notice that
switch either and kept reporting the window tiled at 936 px. The user wants
Mod+F to give that real full screen while the window stays a member of the
strip like any other Mod+F window.

## Considered Options

- **Strip the caption and size the frame ourselves.** Rejected: mstsc
  decides scrollbars from its own mode, not from its client size alone, and
  changing another app's style behind its back leaves it inconsistent on the
  way back.
- **`ShowWindow(SW_MAXIMIZE)` or `smart sizing:i:1`.** Rejected: the first
  keeps the caption and the taskbar (measured above); the second is a
  per-file client setting that scales the remote image instead.
- **Detect "apps that have a full-screen mode" automatically.** Rejected:
  there is no way to tell before asking, and an ordinary window answers
  `SC_MAXIMIZE` with a plain maximize, which the strip must not get.
- **A per-app rule option, with the app's own switches followed (chosen).**
  Explicit, off by default, and the app keeps owning its rect.

Smaller choices:

- **`action` became optional.** An options-only rule (`match` +
  `fullscreen`) adds a property without deciding classification, so the
  bundled Remote Desktop rule does not change whether mstsc tiles or floats.
  Classification skips rules without an action.
- **The option is looked up apart from the action**, first rule over user,
  learned and bundled rules that matches *and sets it*. A learned rule only
  records an action, so `set-window tile` on mstsc (which writes a learned
  exe + class rule) never hides the bundled option, and a user rule can
  still turn it off with `fullscreen = "frame"`.
- **Posted, then polled.** `PostMessage` keeps a hung app from stalling the
  daemon; the bounded poll makes the toggle synchronous for the rest of the
  daemon, so the hook events of the switch itself find a consistent state.
  Measured: the round trip (Mod+F out of the full screen) answers in about
  50 ms.
- **Leaving is judged by the chrome alone.** A workspace switch parks the
  fullscreen window off its monitor; judging by "covers the monitor" would
  read that as the app leaving.
- **Remembered borders.** Right after the switch back DWM still reports a
  chromeless frame for a moment; using that measure seated the window 7 px
  inside its slot (seen in the VM). The captioned frame's borders from before
  the switch stand in when the live measure is still zero.
- **The keymap re-installs its keyboard hook.** In its own full screen mstsc
  installs a low-level keyboard hook that sends the Windows key combinations
  to the remote session (`.rdp` `keyboardhook:i:2`, its default), in front
  of AutoHotkey's, so Mod+F could not leave the full screen (measured: the
  remote session opened Feedback Hub). `flow.ahk` polls every 250 ms and
  calls `InstallKeybdHook(true, true)` once each time a Remote Desktop full
  screen becomes the active window, which puts its hook in front again.
  Chords the bind table does not hold pass through to mstsc and the session
  (measured with Win+I). The alternative, `keyboardhook:i:0` in the user's
  `.rdp` files, also works but sends every Windows combination to the local
  machine.

## Consequences

- **Remote Desktop gets a real full screen from Mod+F** (no scrollbars,
  connection bar shown) and stays a member of its workspace: Mod+F, a
  workspace switch, `move-to-monitor`, focus moves and `flow query` behave
  as for any fullscreen window, and leaving puts it back into its slot at
  its width.
- **The title-bar maximize and the connection bar's restore are FlowWM
  fullscreen in and out**, where before FlowWM kept the window "tiled at
  936" while mstsc filled the screen.
- **Leaving waits for the app**, up to 2 s on the daemon thread, only for a
  window under the rule (and once per such window in `flow stop`).
- **A window entering fullscreen comes back with its border inset.** The
  exit path now records the tile slot before the targets are built, so the
  frame is inset for its ring like any tile; before, every fullscreen exit
  (frame or app) seated the window 2 px larger than its slot until the next
  relayout. A one-line consequence for all fullscreen windows.
- **The rules schema changed**: `action` is no longer required and
  `fullscreen` is new. Existing rule files parse unchanged.
- **Not changed.** The frame fullscreen of every other window, the
  classification of mstsc, and how generic tiles react to a user maximize.
- **Tests.** `config::types` parses the option and options-only rules;
  `registry::classification` covers the lookup order (user / learned /
  bundled, a user `frame` override, the bundled rule) and that classify
  skips options-only rules; `daemon::fullscreen` drives the transitions
  through a scripted `AppFullscreenProbe` (enter by request, a refused
  request, adoption, following the app out with stale borders, parking by
  a pure offset, close). The VM driver `flowwm-vm/test-stage26.ps1` runs
  them against a real Remote Desktop session (cases F1 to F10, K2 to K4).

## Amendment (stage 27, 2026-10-27): the keymap in Remote Desktop's full screen

On the host, with real sessions, two things went wrong with the hook
re-install above: Win+F *into* the full screen opened the local Start menu
(the daemon had entered it), and one Mod+F *inside* it did nothing for about
80 s (it reached the session). Both were reproduced in the VM
(`flowwm-vm/test-stage27.ps1`) and traced to two causes.

**The Start menu.** A Win chord that AutoHotkey suppresses leaves its hook a
note to mask the Win release (`{vkE8}`), or Windows reads Win down + Win up
as a tap and opens Start. `InstallKeybdHook(true, true)` drops that note.
The 250 ms poll re-installed the hook as soon as the full screen was active,
which is usually while the Win key of Win+F is still held, so the release
went out unmasked. Measured with Win held 100 to 600 ms after F: the Start
menu opened in every valid try with the re-install (3 of 3, and once in an
earlier session), never with the re-install removed (0 of 4), never with the
fix (0 of 3 plus S2). With Win released before the poll it did not open,
and in a fresh session mstsc instead swallowed the release (the Win key
stayed down for Windows until the next Win press).

**The swallowed chord.** In its full screen mstsc's low-level hook takes the
Win key and sends it to the session; Windows never sees it go down, and
AutoHotkey's hook, which keeps its modifier state in step with Windows',
reads Win as up by the time F arrives: the keymap sees "f", not "#f", and
the session gets Win+F (Feedback Hub). The hook order was not the deciding
factor: with the hook in front (a key trace in the keymap showed both
events arriving, Win as physically down then up again before F) the chord
still failed, with or without a periodic re-install, in a fresh session and
an old one. Stage 26's K2 case passed only because its entry had left the
Win key stuck down for Windows (the swallowed release above). None of the
mstsc events tried (connection bar hover and click, pin twice, a click in
the session, Alt+Tab twice, switching away and back) made a difference: the
stage 26 keymap lost Mod+F inside the full screen in all of them and in the
control without any, and the fixed keymap kept it in all of them.

**Decision.**

- **Enter after the release.** A `fullscreen` dispatch bind on a windowed
  `TscShellContainerClass` waits (`KeyWait`, at most 2 s per Win key) before
  it is sent, so mstsc switches with no Win key held: AutoHotkey masks the
  release itself and mstsc never sees a release it did not see go down.
- **Re-install, but not while it hurts.** The hook is re-installed when an
  mstsc full screen becomes the active window and then every 1000 ms while
  it stays active, but only in a typing pause (`A_TimeIdleKeyboard` >=
  300 ms), never while any modifier is physically down (AutoHotkey's state or
  the keymap's own Win tracking) and never within 400 ms of a hotkey. Without
  the re-install mstsc's hook stays in front and the keymap never sees the
  Win key (measured: twins alone failed). A re-install can drop a keystroke
  typed at that moment (one character in 385 at a 250 ms cadence with no
  pause rule; none at 1000 ms or with the rule), hence the pause rule and the
  slower cadence. No mstsc event was found that moves its hook in front again
  while the full screen stays active, so the periodic part is a guard, not a
  measured need.
- **Twins of the Win chords.** The keymap follows LWin/RWin itself with a
  listening `InputHook` (`V L0`, notifications only), and `ApplyBinds`
  registers, for every bind whose chord has the Win modifier, a twin without
  it under a gate "an mstsc full screen is active and the Win key is held".
  The twin sends `{Blind}{LCtrl}{LWin up}{RWin up}` before it runs the
  bind: the session already got the Win key down (mstsc forwarded it), and
  when the chord leaves the full screen the physical release stays local, so
  without this the session kept a Win key held and the next "l" typed there
  locked it (seen in the VM). The Ctrl tap masks that release; AutoHotkey's
  own mask key `vkE8` has no scan code, mstsc does not forward it, and the
  session's Start menu opened (seen in the VM). Unbound Win chords still
  reach the session. A Win key seen down for more than 5 s counts as released,
  so a release mstsc swallows cannot leave plain keys acting as chords; the
  Hyper-V keyboard sends no auto-repeat, so the bound could not rely on it.

Rejected: re-installing on every tick (drops keystrokes, and still loses the
chord to the modifier state), and making LWin a suppressed prefix key in the
full screen (unbound chords would no longer reach the session).

**The keymap log.** With `[logging] keymap_log = true` the daemon sets
`FLOW_AHK_LOG` for the script it launches, unless the user's environment
has one, to `logs/flow-ahk-YYYY-MM-DD.log` next to `flowd-YYYY-MM-DD.log`
(`autostart::ahk_log_env`); `[logging] keep_days` prunes both regardless.
The switch is off by default because the log records launcher queries;
an always-on log was rejected for that reason. The keymap
writes millisecond times and an `rdp:` line when a full screen becomes
active, when a re-install waits for the modifiers, and when the full screen
is left (with the number of re-installs), so a miss like the host's leaves a
trace. The log also records launcher queries and the top row's name.
