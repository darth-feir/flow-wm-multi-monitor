# Mod + left-button drag: the keymap moves, the daemon brackets

**Status:** Accepted — implements the roadmap's "Super+LMB gestures" item
without the in-process input hook the roadmap assumed. "Input hooks
(DragSession etc.)" stays open for resize.

niri's Mod+LMB grabs a window anywhere, not only by its title bar. In this
fork the AutoHotkey keymap owns the gesture: `#LButton` resolves the
top-level window under the cursor, activates it, tells the daemon
`flow dispatch drag begin`, moves the window itself with `WinMove` every
~10 ms while the button is physically held, and ends with
`flow dispatch drag end`. The daemon treats the session exactly like a native
title-bar drag: a tile enters `DragMode` (classification, border following,
drop-zone preview and edge-scroll are untouched because the keymap's moves
arrive as the same `LOCATIONCHANGE` stream), a float has its float-sync
bookkeeping held until the end and its final rect recorded once.

## Context

A hook hotkey swallows the button-down. Everything that could start
Windows' own move loop afterwards was tried under real (hypervisor-injected)
input in the VM and fails the same way — the daemon logs the drag start and
end but the window never moves: `WM_NCLBUTTONDOWN`/`HTCAPTION`,
`WM_SYSCOMMAND SC_MOVE` (both spellings), a re-injected button-down, a decoy
window taking the injected click, polling `GetCapture` /
`GetGUIThreadInfo` for the move-size state. Synthetic AutoHotkey input is
never "the button is held" to the native loop. A plain `WinMove` loop while
`GetKeyState("LButton", "P")` moved the window on the first try. The drag
module's state machine needed a way to be entered and left without
`MoveSizeStart`/`End`, which only Windows' own loop produces.

## Considered Options

- **An in-process low-level mouse hook in the daemon** (the roadmap's
  `src/input/`). Rejected for now: a `WH_MOUSE_LL` hook puts every mouse
  event through the daemon's hook thread with a system-wide latency budget,
  and the keymap already owns the Win key; two processes would then both
  have to agree on who swallows the click. The keymap-side loop is ~40 lines.
- **Convert the window to a float for the drag.** Rejected: the same
  float round-trip upstream removed from the native drag ("Why This Design"
  in the tile-drag chapter) — dwell timers, animation locks, cancel hazards.
- **Explicit session commands bracketing a keymap-driven move (chosen).**
  `drag begin` acts on the focused window and enters the state a
  `MoveSizeStart` would give it; `drag end` runs the `MoveSizeEnd` path.
  The commands *are* the drag, so they bypass the busy gate, but `begin` is
  refused while tiling is off.
- **Let the daemon ignore the keymap and treat the moves as float-sync /
  snap-back.** Rejected: a tile would snap back on the first
  `LOCATIONCHANGE` and a float would be re-applied under the cursor by the
  next animation batch.

## Consequences

- **Rules the daemon enforces.** `begin` while any drag is active (native
  `drag_state` or a session) is an error; `begin` on a focus that is not an
  active tile or float (minimized, hidden, fullscreen, ignored, untracked) is
  an error; `end` with no session is a no-op (`ended: false`, exit 0), because
  the keymap always sends it; a native `MoveSizeStart`/`End` for the
  session's own window is ignored; a window destroyed, minimized or hidden
  mid-session abandons the session (nothing committed); `tiling off`,
  `flow stop` and the fail-back end it first so the busy gate cannot refuse
  them.
- **Float geometry got one rule.** The animation bridge inset floats like
  tiles (border thickness minus overlap) and the float-sync path read the
  smaller window back as the new rect, losing 4 px per drag. `float_geometry`
  now places a float at `visible_to_window(rect)` with no inset and seats
  its ring around it; tiles are inset as before. This was a pre-existing bug
  the session tests exposed (a repeated empty session kept the size).
- **The keymap side has its own lessons** (`keymaps/win-niri/README.md`):
  `CoordMode("Mouse", "Screen")` before comparing with `WinGetPos`
  (AutoHotkey v2 defaults to the active window's client area, which resolved
  the grab point inside the wrong window); send an unassigned virtual key
  (`vkE8`) first so Windows never opens Start on the Win-up, whichever order
  the keys are released in; skip the border overlays, click-through and
  cloaked windows, the desktop and the taskbar; a maximized window is not
  draggable.
- **Accepted costs.** The daemon trusts the keymap to send `end`; a keymap
  killed mid-drag leaves the session open until the window is minimized,
  hidden, destroyed, or `tiling off` / `stop` — the same stuck-state risk
  upstream's "floats enter DragMode" record (on its `feat/floats-above-tiles`
  branch) accepts for native drags.
  There is no Mod+RMB resize. Upstream's unmerged `DragMode::Float` would
  make the float half of the session shorter; the fork's version should be
  rebased on it if that lands.
- **Tests.** `decide_begin`, `native_event_is_shadowed` and
  `ends_manual_drag` are pure; the Win32 reads go through `DragProbe`, so
  begin → location changes → end runs on a tile (strip reorders on release,
  nothing commits before) and on a float (writes held, final rect recorded,
  size kept) against a scripted desktop. The VM driver (`test-stage18.ps1`)
  injects a real Win-held drag through the hypervisor's synthetic mouse and
  checks the reorder, the float's size, the Start menu and the daemon's
  session log. See `docs/src/dev-guide/tile-drag.md` (*Keymap-driven
  sessions*).
