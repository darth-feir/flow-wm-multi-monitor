# Tile drops land by window halves, scroll bands are ratios, and a tile can be dropped on another screen

**Status:** Accepted — replaces the `(n+1)`-region row split and the
pixel-only screen-edge band of the tile-drag zone map
(`docs/src/dev-guide/tile-drag.md`), and the rule that a tile released over
another monitor returns to its own strip.

A tile drag (a native title-bar drag or a Mod+drag keymap session) resolves
the cursor against the strip of **the monitor under the cursor**. In that
strip the outer `col_edge_ratio` (a third) of a column on either side drops
the window as a new column beside it; the middle third drops it above or
below **the visible row under the cursor**, by which half of that window the
cursor is in. A column with more rows than `visible_rows` offers a top and a
bottom row-scroll band in its middle third while rows are parked that way.
The screen-edge bands are `max(edge_scroll_width, edge_scroll_ratio · work
area)` wide, a twelfth of the screen by default, but only
`edge_scroll_width` on an edge shared with another monitor. A drop on
another monitor's strip moves the window there, through the same tail as
`move-to-monitor`.

## Context

The user described the drag they want in their own words: the outer third
of a column puts the window next to it, pushing the column aside; the
middle third puts it above or below the window under the cursor, "based on
which half of that window" — in a stack, the top half of the bottom window
and the bottom half of the top window are both "between"; holding the
cursor at the top or bottom of a stack whose rows do not all fit scrolls it
so a hidden row can be reached; the dropped window must be visible
afterwards, bumping the top window off screen; the edge bands should be a
twelfth of the screen; and a tile must be droppable on the other monitor,
which floats already were.

The map it replaces had three mismatches with that. Its column-insert band
was 18 % capped at 120 px, so on a 640-px column the "beside it" target was
a sliver. Its row split cut the **whole column** into `n + 1` equal
regions, whose boundaries do not fall at the middle of a window, and the
column box was the bounding box of *all* rows, including the ones
`visible_rows` parks above and below the monitor, so a stacked column with
parked rows resolved against a box several screens tall. And it resolved
only against the dragged window's own strip, so a release over the other
monitor snapped back.

## Considered Options

- **Keep `(n+1)` regions, over the visible rows only.** Fixes the parked
  rows but not the boundaries: in a two-row stack "above all" would still
  be the top third of the column, not the top half of the top window.
- **Per-window halves of the visible rows (chosen).** Each insertion slot is
  the union of the two halves that border it, so the map is still a
  bijection with no ties, and every boundary is the middle of a window the
  user can see. A cursor in the gap between two rows belongs to the lower
  row's top half, the same slot.
- **Row bands that scroll one row per move event**, as first described.
  Rejected for the cadence: `LOCATIONCHANGE` fires per pixel, so a stack
  would race to its end, the bug the edge-scroll scheduler was built to
  remove. The row bands use that scheduler (one row on entering the band,
  then the timed repeat while the cursor stays), like the column bands.
- **Edge bands as a pure ratio** (drop `edge_scroll_width`). Rejected: one
  new knob, `edge_scroll_ratio`, keeps the pixel knob as a floor and as the
  band on an inner edge. A twelfth of the screen on the edge the user
  crosses to reach the other monitor would scroll the source strip on every
  cross-screen drag, because entering a band scrolls at once.
- **Drop the column-insert cap.** `col_edge_max_px = 0` now means "no cap";
  a positive value still caps. A third of any column is the target the user
  asked for, and the ratio is clamped to a half so the middle never
  vanishes.
- **Cross-screen: the keymap moves the window onto the other strip itself.**
  Rejected: it would make the keymap reason about layouts, and native
  title-bar drags would not get it. The daemon already sees every move.
- **Cross-screen: resolve against the target strip and move on release
  (chosen).** The drop is decided by the last move's `(monitor, zone)`, not
  the cursor at release, because a keymap session's final `WinMove` can
  still be queued when `drag end` arrives.

## Consequences

- **One pure map.** `resolve_drop_zone(applied, monitor, mx, my, &ZoneParams)`
  stays pure and total over the work area; an empty strip answers
  `Column { col: 0 }`; outside the work area (another monitor, a gap, a
  taskbar) it answers `None`. `DropZone` gains `ScrollUp { col }` and
  `ScrollDown { col }`. `preview_insert` is the cross-screen counterpart of
  `preview_move`.
- **Visible rows only.** `build_visible_columns` takes each column's rows
  `row_offset .. row_offset + slots` from the projection; parked rows are
  not drop targets.
- **The drop is visible and focused.** A drop into a stack reveals its row
  (`insert_row_at` → `reveal_row`) and the strip's focus moves to the
  dropped window, so `apply_mutation`'s focused-row invariant keeps it on
  screen; the window that was on top is parked, and Mod+Up reaches it. A
  row scroll during the drag does not pull the focused window back
  (`scroll_column_rows` applies without the focused-row normalisation).
- **Cross-screen.** `DragState` carries `zone_monitor` and `touched`. Every
  preview or scroll re-settles the other touched strips in the same
  animator batch, because a new batch supersedes an in-flight one. A drop
  on another strip removes the window from its own strip, inserts it at the
  zone, and runs `finish_tiled_monitor_move`, shared with `move-to-monitor`
  (owner memory, active monitor, one batch for both strips, foreground,
  border). The target strip's edge and row bands scroll that strip. A full
  target column refuses the drop like a full own column: the window snaps
  back. A stacked pair works the same way: a cursor above or below the
  source work area that is on the other display is a cross-screen drag.
- **Two fixes the VM found.** A window alone in its column dropped on
  that same column (either half) is a no-op; before, the post-removal index
  made it a row of the *next* column, so "drag out and come back" moved it.
  And `scroll_right` stops at the end of the strip when a step would
  overshoot it, instead of refusing: the trailing gap left the last column
  2 px short of a full step, so the right edge band could never reveal it.
- **Defaults.** `edge_scroll_width = 30`, `edge_scroll_ratio = 0.0833`,
  `col_edge_ratio = 0.33`, `col_edge_max_px = 0`. A user config that set
  `col_edge_max_px = 120` keeps a capped band; one that set `0` before (a
  one-pixel band) now gets no cap.
- **Accepted costs.** The source strip shows its committed layout while
  the cursor is on another screen (no gap where the window was) — the
  preview shows only where it lands. A drop on another screen makes that
  monitor active and focuses the window there. The column preview uses the
  base column width; a width pattern re-pins it on commit (ADR-0017).
- **Tests.** In `layout::preview`: outer thirds on both sides, the
  uncapped third of a 640-px column, per-window halves in 1-, 2- and 3-row
  columns, halves of the visible rows of a 4-row/2-visible stack, the row
  bands offered only toward parked rows, no bands when every row fits,
  the edge bands at a twelfth and the pixel band on an inner edge, the
  target strip's zones and an empty strip, `preview_insert` and its
  refusals, the dropped row revealed; `scroll_column_rows` bounded by
  content. In `daemon::drag_session`: a session dropped on the other
  monitor's outer third and middle third, a drag that returns to its own
  screen, a release off every monitor, a drop on an empty monitor. The VM
  driver (`test-stage24.ps1`) runs every zone on both screens, the four
  bands, drops mid-scroll, cross-screen in each zone, native title-bar
  drags across, the stacked layout and a float crossing as before.
