# Fullscreen is a column of the strip

**Status:** Accepted. Replaces the stage 10 model (a fullscreen window was a
float at the monitor's screen rect, outside the strip) for every window,
frame and app fullscreen alike, and amends the run rule of
[ADR-0017](0017-width-patterns-are-screen-slots.md). ADR-0020 (an app's own
full screen) is unchanged in what it asks of the app.

`set-window fullscreen` (Mod+F) flags the focused window's **column** as
fullscreen (`Column::fullscreen`, `src/layout/types.rs`). The window stays a
tile (`Tiling(Active)`), in its strip, at its place:

- **Model.** A fullscreen column holds one window. A window that shares a
  stack is first expelled into a column of its own right after the stack
  (`mutations::expel_to_column`); leaving, it is merged back at its row
  (`mutations::merge_back`) when the stack is still there and has room. A
  float put fullscreen joins the strip after the focused column and floats
  again, at its rect, on the way out. `width_px` is left alone while the
  flag is set: it is the width the column returns to. `FlowWM::fullscreen`
  (HWND → `FullscreenOrigin`) stays the authority on which windows are
  fullscreen; the column flag follows it (`reflag_fullscreen`,
  `resync_fullscreen_columns`). The origin of a tile now records its row,
  one window of its stack (`None` when alone) and the strip's
  `viewport_offset`, which leaving restores when the window is still on
  that strip.
- **Canvas.** A fullscreen column spans one whole screen on the canvas:
  `Column::canvas_width = monitor_width − gap`, so its slot is exactly
  `monitor_width`. Every prefix sum reads it through one helper,
  `projection::column_left` (and `column_right`, `canvas_width`,
  `column_step_width`); `ensure_column_visible`, `first_visible`,
  `center_viewport_on_focused`, the pin and the scrolling space's anchor
  all go through it, so the three hand-rolled copies of the prefix sum are
  gone. Camera math stays in work-area units.
- **Projection.** A fullscreen column is shown only when it **starts the
  view** (it is the first column overlapping the viewport): then its rect
  is the monitor's **screen rect** (taskbar covered, no gap) and every other
  column is parked. Anywhere else it is parked on its side at the screen
  rect's size, so moving it in and out of view is a pure move (ADR-0019).
- **Camera.** A fullscreen target starts the view exactly
  (`viewport_offset = col_left − gap`). A fullscreen column left of the
  target that would still overlap the new view would start it and hide the
  target, so the view starts at the target instead; when the camera had to
  move and a fullscreen column right of the target would still overlap,
  the view ends at the target (focus leaving a fullscreen column leftwards
  shows the columns before it, not an empty screen). `scroll-right` off a
  fullscreen column takes a whole screen even past the end clamp; raw
  `scroll-left` from one may leave empty space (accepted). `center` on a
  fullscreen column starts the view at it.
- **Commands.** `expand-column`, `shrink-column`, `set-column-width`,
  `toggle-monocle`, `merge-column`, `consume-or-expel`, `promote` and
  `swap-window left|right` are refused on a fullscreen window with a message
  naming the command; a merge or consume *into* a fullscreen column is
  refused too ("holds one window"); a resize-drag boundary next to one does
  not move. `swap-column` / `move-window left|right` move it (the flag
  travels with the column, focus follows). `width <preset>` still applies
  to the whole screen: the fullscreen column keeps its flag and comes back
  at the new width. A window opened while a fullscreen column is focused
  goes after it and takes the view; one Mod+Left returns.
- **Z-order.** `HWND_TOPMOST` for the whole fullscreen life, as before;
  the floating layer keeps excluding daemon-fullscreen windows; no border.
- **Frame vs app.** Frame fullscreen keeps its caption as before; the
  animation bridge places a fullscreen tile at its rect without the ring
  inset (`tile_geometry`, five sites). App fullscreen (ADR-0020) is entered
  and left exactly as before; the rect the app chose is recorded as sent so
  no batch sizes its frame (`app_owns_rect`, now also on the tiling paths),
  and parking by column (x) joins parking by workspace (y) as a pure move.
- **Lifecycle.** Minimize / hide take the column out of the strip and keep
  the origin; restore / show re-seat it as a fullscreen column in view
  (`readd_tile_to_home_monitor` → `reflag_fullscreen`). `move-to-monitor`
  and `move-to-workspace` carry it and re-flag it in the target strip; a
  hotplug or a loadout re-flags every strip from the map. Close forgets it.
  `tiling off` takes every window out of fullscreen where it is (an app's
  own full screen ended, topmost cleared, the column a plain one) before
  the usual pause; `tiling on` finds an ordinary tile. `flow stop` and the
  fail-back clear topmost, end an app's full screen and move the window in
  view inside its work area; a parked one is rescued like any stranded
  window.
- **Drag.** `drag begin` on a fullscreen window is refused as before. A
  native title-bar drag of a frame-fullscreen tile is never promoted from
  the classifying state: nothing moves in the layout and the window snaps
  back on release. A row drop into a fullscreen column is refused (no-op,
  `preview::insert_at_zone`); the outer bands still drop a new column
  beside it.
- **Query.** `flow query all` keeps `mode: "fullscreen"`; `state` is
  `Tiling(Active)`, `tiled_rect` the screen rect (or the parking slot), and
  each monitor's `columns` entry carries `fullscreen`.

## Context

The user asked for niri's behaviour: Mod+F makes the window fill the
monitor, Mod+Left / Right move to the neighbouring column with the view
scrolled so the screen's normal width pattern shows, the fullscreen window
parked like any column; scrolling back shows it full screen again; Mod+F
again restores its width and slot. Under stage 10 a fullscreen window had
left the strip for the floating space, so focus left / right from it was a
silent no-op (`dispatch_focus` bailed on a float) — including the
Mod+Left / Right twins inside a Remote Desktop full screen (ADR-0020,
stage 27), which already reached the daemon and were dropped.

## Considered Options

- **Keep the float-space model for floats, columns for tiles.** Rejected
  (the user's decision): two models, two sets of lifecycle paths, and a
  float would still be a dead end for focus.
- **Fullscreen the whole stack.** Rejected: one window over the screen is
  what fullscreen means; the stack would need a second "which row" view.
  The window is expelled and merged back instead.
- **Topmost only while in view.** Rejected for now: a fullscreen window
  that is topmost only while it starts the view needs a z-order change on
  every scroll; keeping it topmost for its whole life is today's behaviour.
- **Persisting the flag across a daemon restart.** Rejected for now: a
  frame fullscreen window comes back as an ordinary column, an app
  fullscreen one is re-adopted when the app is still in its own full
  screen (ADR-0020). A `fullscreen` field on the loadout column would be
  about 60 lines.
- **Continuous scrolling, a fullscreen column partly in view (niri).**
  Rejected, see below.
- **A screen-wide column, shown only when it starts the view (chosen).**

**Why not a partly visible fullscreen window.** niri scrolls continuously:
a screen-wide window can sit half in view while its neighbour slides in.
Windows does not clip top-level windows to a monitor, so the half that is
"out of view" would be drawn on the neighbouring monitor; on the user's
host DISPLAY2 sits at −1920..0, right where a fullscreen window scrolled
left would spill. The projection's overhang rule already parks any column
that would cross onto a neighbour for that reason (ADR-0007), so a
fullscreen column is either the whole view or parked.

## Amendment to ADR-0017: the run rule

ADR-0017's anchor is the first column at or past the viewport's left edge,
capped so a whole run of `n` slots fits before the strip's end. With
fullscreen columns:

- A fullscreen column found there is the anchor as it is, **a run of one**,
  never capped (the cap would pull the anchor back and hide a fullscreen
  column at the strip's end).
- A run of `n` slots never holds a fullscreen column other than its anchor:
  if one would fall inside it, the anchor moves to `fullscreen − n`, so the
  run **ends just before it** ("the set before F": Mod+Left from F in
  `[A, B, F, C, D]` under 2/3+1/3 shows A 2/3 and B 1/3). The anchor never
  moves back onto or across an earlier fullscreen column (the cap and the
  keep-the-focus rule are floored there too: Mod+Right from F shows C and D,
  and past the last of two adjacent fullscreen columns the run starts after
  it).
- A fullscreen column takes no term. Terms are counted over the other
  columns (`slot_rank`), so the slots the columns on either side of a
  fullscreen column get do not shift when the view passes it, and its
  width stays the one it returns to.

## Consequences

- **Focus moves through a fullscreen window** like niri: Mod+Left / Right
  (and the Remote Desktop twins) reach the neighbouring set, the
  fullscreen window parks, coming back shows it full screen again.
- **Mod+F out restores the view** it was entered from, the column's width
  and, for a window that came from a stack, its row.
- **Fewer paths.** `seat_fullscreen`, `reassert_fullscreen`,
  `move_fullscreen_to_monitor`, `demote_all_fullscreen_to_floats`,
  `reseat_fullscreen_after_hotplug`, the loadout's fullscreen special case,
  the float-tracking filter and the two floating-layer filters in
  `focus_layer_with` are gone: a fullscreen window takes every tile path.
- **The drag blind spot is accepted**: a tile dragged across a fullscreen
  window in view is under it (topmost) until dropped.
- **No border target for a fullscreen column.** Its overlay is dropped
  right after the entry batch, so the animation bridge leaves a fullscreen
  window's overlay out of its targets (`collect_targets`,
  `workspace_targets`); otherwise the animator kept moving the destroyed
  overlay for the rest of the tween (seen in the VM as `SetWindowPos ...
  Invalid window handle` on every Mod+F).
- **Leaving a parking slot resends the size** (ADR-0019): scrolling back to
  an app-fullscreen window sends one sized `SetWindowPos` at the app's own
  rect, the same as a workspace return under stage 26.
- **Tests.** `layout::projection` (canvas width, in view, parked on both
  sides), `layout::mutations` (the camera rules, expel / merge back, every
  refusal), `workspace::scrolling_space` (the run rule under 2/3+1/3 with
  `[A, B, F, C, D]`, a fullscreen column at the strip's end, two adjacent
  ones, leaving with the view restored, no pattern), `layout::resize`,
  `layout::preview` and `daemon::fullscreen` (stack, lone column, float,
  modes, refusals, focus through, minimize / restore / close, workspace and
  tiling off, the shutdown release, move-to-monitor, re-seating, presets,
  app fullscreen: request, refusal, adoption, parking, close). The VM
  driver `flowwm-vm/test-stage28.ps1` runs the cases against real windows
  and Remote Desktop (DISPLAY1 on 2/3+1/3, DISPLAY2 on 1/2): every parking
  move of a fullscreen column was a pure move (the test windows count the
  `WM_WINDOWPOSCHANGING` messages that carry a size), and Remote Desktop
  stayed in its own full screen, chromeless, with nothing posted to it,
  while focus moved off it and back. `test-stage26.ps1` now accepts an app
  full screen that is parked because another tile took the foreground.
