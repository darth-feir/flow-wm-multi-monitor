# The layers are reached by a focus switch, and every drag is a `drag_state`

**Status:** Accepted — closes two items from the fork's niri comparison
(2026-10-23): niri's `switch-focus-between-floating-and-tiling` chord and its
grab threshold for Mod + left-button drags. On the way the float half of
ADR-0011's keymap drag sessions moves into the single `drag_state`, the shape
upstream's unmerged "floats enter `DragMode`" record (ADR-0008 on
`feat/floats-above-tiles`) chose for native float drags; ADR-0011's "rebase
on it if that lands" cost is paid now.

Three commands reach the other side of the active monitor's workspace:
`focus-floating` (the float the user used last there, else the top-most one),
`focus-tiling` (the strip's last-focused window, else its first) and
`switch-focus-between-floating-and-tiling` (whichever side does not hold the
focus; Mod+Shift+V in the win-niri keymap). An empty side is a no-op that
says so, exit 0. A Mod + left-button press becomes a drag only once the
cursor has left an 8 px circle around the press point with the button still
held; a release inside it is a plain Mod+click that activated the window. A
floating window entering a drag — natively on `MoveSizeStart` or through
`drag begin` — occupies `drag_state` as `DragMode::Float`, exactly as a tile
occupies it as `Classifying`: its `LOCATIONCHANGE` events route to the drag
path, which stores the live rect through the same `store_float_rect` the
passive path uses; the animator leaves whatever `drag_state` names alone;
`MoveSizeEnd` / `drag end` clears it with no commit.

## Context

The floating layer (ADR-0013) drops under a fullscreen or foreign foreground,
which left no way to reach a float while a browser was in F11: the float sat
under it until the browser left fullscreen. niri has the same stacking rule
and answers it with a chord that makes the floating layer active, so the
floats come up over the fullscreen tile; the fork had `toggle-window-floating`
(Mod+V) but no focus switch. The first idea was raising the layer when the
Start menu opens; the switch is what niri does, needs no inspection of shell
windows, and the stage-19 rule already raises the layer the moment a float is
the foreground, so the switch is one command and no new rule.

Stage 18's Mod-drag began the session on the button press, so a Mod+click
meant to activate a window opened and closed a `drag` session and, on a tile,
re-committed the layout. niri's `MoveGrab` recognises a drag only after 8 px.

Stage 18 also kept a float session out of `drag_state`: a side field named
the float, and the animation bridge and the float-sync path each checked it
to leave that window alone. Upstream's ADR-0008 rejects exactly that shape —
"a twin of `drag_state`" that every consumer has to learn — and puts floats
into the one field, where the busy gate, the animator exclusion and the
`LOCATIONCHANGE` routing already look.

## Considered Options

- **Raise the floating layer when the Start menu takes the foreground.**
  Rejected: it is the fork's invention, it needs the owning process of a
  shell `CoreWindow` to tell Start from Search, and it still leaves no way to
  *focus* a float from the keyboard. The switch covers the case and matches
  niri's `switch-focus-between-floating-and-tiling`.
- **Raise the floats on activation order** (move the activated float to the
  end of the `FloatingSpace`, niri's `raise_window`). Rejected for now: the
  space's order is a z-order the animator submits, and reordering it on every
  focus change is a visible change to a stable structure. A per-workspace
  `last_focused_float`, written by the foreground sink and checked against
  the space when read, gives the same "the float I used last" answer.
- **A fullscreen window is the floating side.** Rejected: the daemon's
  fullscreen window sits in the floating space but niri treats fullscreen as
  a scrolling state, and a user whose fullscreen window has the focus wants
  the switch to reach a *float*. Fullscreen windows neither make the layer
  active nor are offered by `focus-floating`.
- **Dead zone in the daemon** (`drag begin` on the press, the daemon ignores
  sessions that never move). Rejected: the gesture's threshold belongs where
  the cursor is read, and a session that never began needs no `end`; the
  keymap waits for 8 px of travel before `drag begin`, as niri's grab waits
  before `interactive_move_begin`.
- **Keep the float session's side flag** and only document it. Rejected: the
  maintainer's own record rejects it, and the two skip-checks it needed
  (animation bridge, float-sync) vanish when the float holds `drag_state`.
- **`DragMode::Float` for sessions only, native float drags unchanged.**
  Rejected: one path for both is the point; a native title-bar drag of a
  float now also keeps the animator off the window for the gesture.

## Consequences

- **The foreground sink records the float.** `on_focus_changed` writes
  `Workspace::last_focused_float` for an active float of the window's home
  workspace; readers check the float is still in the space, so no removal
  path has to clear it. `focus_layer_with` walks the candidates like
  `focus_monitor` does (dead purged, hidden skipped, refused push skipped),
  with the three Win32 questions injected.
- **Wire and CLI.** `SocketMessage::FocusLayer { target: floating | tiling |
  other }` is layout-mutating (refused by the busy gate and while tiling is
  off, like `focus`); it answers `Data { focused, hwnd, message }` so the CLI
  prints `focused the floating windows` or `no tiling window to focus`. The
  three dispatch subcommands carry niri's names; the `[binds]` actions map
  to them one to one.
- **`drag_state` is five states.** `DragMode::Float { id, hwnd }` is entered
  by `on_drag_start` for `Floating(Active)` (native or session), moved by
  `store_float_rect`, ended by one more `store_float_rect` (the last
  `WinMove` of a session can still be in the hook queue when `drag end` is
  read from the pipe) and a border refresh. `ManualDrag` keeps only the fact
  that the keymap owns the gesture (native move-size events for the window
  are ignored) and the word for the reply. The stage-18 float geometry rule
  (no inset for floats) is unchanged.
- **Behaviour that changed.** A native float drag now holds the busy gate
  and the animator exclusion for its duration (before, only tiles did); the
  top-layer re-decision on the foreground's own `LOCATIONCHANGE` is skipped
  for a window being dragged (it was already skipped for tiles). A Mod+click
  that moves less than 8 px opens no session: the daemon log shows no `drag
  session: begin` for it.
- **Accepted costs.** The 8 px zone is the keymap's constant
  (`DragDeadZonePx`), not config; the switch acts on the active monitor's
  workspace, so a float on another monitor is reached by focusing that
  monitor first; `last_focused_float` is per workspace and not saved in the
  loadout.
- **Tests.** `layer_to_focus` and `layer_focus_candidates` are pure; the
  tile → float → tile round trip, both empty sides, a fullscreen window
  excluded from the floating side, and dead / hidden / refused floats run
  against injected probes. The float drag runs as a keymap session (state
  entered, rect stored on every move, the animator's batch leaves the window
  out, final rect kept, a late move recorded at the end) and as a native
  `MoveSizeStart` (same state, `drag begin` refused meanwhile, strip
  untouched); the size regression from stage 18 still passes. The VM driver
  (`test-stage22.ps1`) reaches a float under a borderless full-screen window
  with a real Mod+Shift+V, returns to the tile, checks a Mod+click without
  movement opens no session, a Mod+drag of a float moves it and keeps its
  size, and a native title-bar drag of a float stores its rect. See
  `docs/src/dev-guide/tile-drag.md` (*Floats in the drag state*, *Keymap-driven
  sessions*) and `docs/src/dev-guide/floating-space.md`.
