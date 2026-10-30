# Book mode: the columns out of view pile up behind the edge columns

**Status:** Accepted. An optional layout next to the strip; strip mode is
unchanged. Builds on [ADR-0017](0017-width-patterns-are-screen-slots.md)
(slots), [ADR-0019](0019-pure-moves-and-silent-parked-resizes.md) (pure
moves), [ADR-0021](0021-fullscreen-is-a-column.md) (fullscreen columns) and
the z-order rules of [ADR-0003](0003-float-tile-stacking-invariant.md) /
[ADR-0016](0016-layer-focus-switch-and-one-drag-state.md).

`[columns] layout = "book"` (default `"strip"`; a `[[columns.screen]]` rule
may set its own) keeps every window on screen. The strip, its width
patterns and its camera are exactly the strip mode's; only the rect a column
out of view receives changes:

- **Projection** (`projection::project_with`). A column strip mode would
  park **left** gets the rect of the **leftmost visible column** (its x and
  width, its own rows stacked there) and is marked `PileSide::Left`; one
  parked **right** gets the rightmost visible column's rect,
  `PileSide::Right` (`ActualEntry::pile`). With one visible column both
  piles share its rect. A column less than half on screen counts as out of
  view (a pattern's integer rounding leaves the next column a one-pixel
  sliver inside the view, and a pile must not sit there). A fullscreen
  column keeps off-screen parking, so its moves stay pure moves and an app
  in its own full screen (mstsc) is never sized; while one starts the view
  there is no edge column, and the others lie under it at their own width,
  flush with the work area's left (strip index before it) or right edge. With
  no visible column at all the strip parking is kept.
- **Z-order** (`daemon::piles`). Within a pile the column nearest to the
  view is highest and every member is below its top; a member's border
  overlay is re-seated directly above its own window. One `EnumWindows`
  read decides whether a pile is in order (`chain_in_order`, by column: the
  rows of one column may be in any order); only a pile out of order is
  re-seated, member by member with `SetWindowPos(insert after the previous,
  NOMOVE|NOSIZE|NOACTIVATE|NOOWNERZORDER)` (`insert_below_quiet`). The top
  is never moved: Windows has raised it when it was activated, and it is
  above its members by the invariant. This runs before every batch that
  moves a book-mode strip (`animate_layouts_on`, the drag previews, the
  incoming workspace of a switch) and once per
  `[focus] foreground_sync_interval_ms` against the live z-order
  (`check_piles`, next to the floating layer's pass), which puts back an
  app that raised itself without being activated. Startup, `tiling on`, a
  loadout, a hotplug and a reload seat every pile. The window a drag holds
  is left out (it stays above everything while it follows the mouse).
- **Animation bridge.** A window whose target is a pile slot is placed
  instantly (`push_targets_as`): it lands under its top, while a tween
  would cross the visible columns and show in the gaps. With the animator
  idle a member already at its pile rect gets no target at all, so a
  one-column flip moves exactly one window per side (the incoming card), and
  none at all on a carousel. Surfacing from a pile is an ordinary tween.
  Workspace batches move piles with their workspace (no instant rule).
- **Focus.** A taskbar click, Alt+Tab or any other activation of a piled
  window reaches `on_focus_changed` → `ensure_focused_visible` as before; the
  book flips to it in **one** batch, the columns in between change piles.
- **Runtime switch.** `flow dispatch set-layout strip|book|toggle` (keymap
  Mod+Shift+B, binds action `toggle-layout`) switches every workspace of
  the focused window's screen; a config reload puts every screen back to
  `flow.toml`. The mode lives in `MutationConfig::layout_mode`, carried
  across reconfigure / monitor changes like the row limits.
- **Wrap-around** (`[columns] wrap_around = "off" | "on" | "book"`).
  `focus left/right` at a strip end wraps to the other end never, always,
  or only on a book screen (`mutations::focus`, `wrapped_neighbor`).
  Moving a column never wraps. While a screen wraps, its ends do not hop
  to the next monitor (a strip of one column has nothing to wrap to and
  still hops).
- **Drops** (`preview::resolve_drop_zone`, `DropZone::Card`). The whole of
  a pile's top card is one zone: the window is inserted right after the
  left top / right before the right top and the camera puts it at that edge
  (`ScrollingSpace::edge_layout`, pinned to the slots under a pattern), so
  it is the pile's new top and the old top lies right behind it. A lone
  visible column is the top of both piles; the half under the cursor picks
  one. An edge column whose pile is empty, the middle columns and a
  fullscreen column keep the strip zones. Pile members are not drop
  targets (under their top); deeper drops go through the edge bands, whose
  scroll is a flip in book mode.
- **Query.** `flow query all`: per monitor `layout` and `wrap_around`, per
  column and per window `pile` (`"left"`, `"right"` or `null`);
  `query layout actual` entries carry `pile` too.

## Context

The user wanted the strip without anything off-screen: the columns out of
view should be cards piled behind the edge columns, a flip being a normal
one-column scroll, 1/1 a carousel, 1/2 two piles, 1/3 a plain middle
column, and a taskbar click on a piled app flipping the book to it. FlowWM
never managed tile z-order (placements use `SWP_NOZORDER`; only the floating
layer is managed), so the piles need their order kept on every flip and
against apps that raise themselves. The camera and the pin (ADR-0017) never
look at where a parked column sits, so book mode is a projection and z-order
feature, not a layout-engine change.

## Considered options

- **`HWND_BOTTOM` for members.** Rejected: it drops a member under every
  unmanaged window and the desktop's other apps, and the drift check could
  no longer tell order from accident. `insert after the top` is surgical.
- **Making members non-activatable (`WS_EX_NOACTIVATE`).** Rejected: the
  taskbar / Alt+Tab flip *is* an activation, and the style also takes a
  window off the taskbar unless `WS_EX_APPWINDOW` is forced.
- **Hooking `EVENT_OBJECT_REORDER`.** Rejected: it fires for every child
  reorder in every process. A periodic check on the existing interval costs
  one `EnumWindows` and no `SetWindowPos` while the piles are in order.
- **A fullscreen column as a pile card.** Rejected: it would size the
  window on every flip (impossible for an app's own full screen, ADR-0020)
  and lose ADR-0019's pure-move parking.
- **Flipping through the cards one by one on a far jump.** Rejected: the
  animator supersedes in-flight batches and nothing sequences them; a far
  activation is one jump.
- **A hidden "drop into the pile" zone.** Rejected (the user's decision): a
  target the user cannot see. Deeper drops flip with the edge bands.
- **Members at the top's row geometry.** Rejected: the row-gap see-through
  below is cosmetic and a member keeps its own rows for when it surfaces.
- **The mode as a top-level key.** Rejected: `[columns]` already owns the
  per-screen rules and the patterns the mode works with.
- **Dropping a member's border overlay** (as a fullscreen window does).
  Rejected: an overlay would be created and destroyed on every flip (a DWM
  round-trip each, and stage 28's animated-after-destroy bug); re-seating
  it directly above its own window keeps its ring under the top.
- **The reload as the only switch, no wrap-around** (the plan's first cut,
  both deferred to a later stage). Taken into this stage: `set-layout`
  (Mod+Shift+B) compares the two modes live without editing `flow.toml`,
  and `wrap_around` gives the 1/1 carousel a way round. Both are opt-in;
  `wrap_around` defaults to `"off"`.

## Consequences

- **Every window stays on screen** in book mode; `tiling off`, `flow stop`
  and the fail-back have nothing to undo (members are covered, not hidden,
  and reachable from the taskbar). Strip mode is unchanged
  (`project_with(.., Strip)` is `project`, the half-on-screen rule is
  book-only, nothing is piled, so the z-order module reads nothing) with
  one exception: a config reload now animates every monitor's active
  workspace in one batch, so a screen whose `layout` changed re-projects
  with the others.
- **Restored windows** (un-minimize, show) are appended at the strip end,
  as in strip mode; the old slot is not kept, so a restored window is not
  put back into the pile it came from.
- **Row-gap see-through (accepted).** A stacked top card shows a
  `window_gap`-high strip of the member below between its rows, and a
  member's own rows beyond its row viewport are parked vertically as in
  strip mode ("nothing off-screen" is about columns).
- **Apps that raise themselves** without activation show above their top
  for up to one `foreground_sync_interval_ms`; one that activates itself
  flips the book to it (the intended behaviour). An elevated window cannot
  be lowered by the daemon (UIPI); it cannot be placed either, so this is
  no new limitation; the failure is logged at debug level.
- **Occlusion.** A covered browser may throttle like a minimized one; strip
  parking had the same effect.
- **A far jump shows the in-between cards briefly** at their new pile slot
  until the incoming card arrives (they are placed instantly, above the old
  pile).
- **book → strip** slides the members out from under their top on a free
  side (instant toward a neighbouring monitor), as strip parking always
  does; strip → book places them instantly under their tops.
- **The edge-scroll flip during a drag** keeps the dragged window above
  the piles; it is excluded from seating while the drag lasts.
- **Tests.** `layout::projection` (strip identity, two piles, a pattern's
  slot width, the carousel, a stacked member, fullscreen parking and the
  under-fullscreen fallback), `layout::mutations` (the three wrap values,
  moves never wrap), `layout::preview` (card zones, empty piles, the lone
  column's halves, a card drop that keeps the order),
  `workspace::scrolling_space` (a flip moves one window, the mode survives
  reconfigure / monitor changes, `edge_layout` with and without a pattern),
  `config::width_preset` (parsing, layout-only rules, refusals),
  `daemon::piles` (chains, per-column order, seating with a mock z-order,
  the periodic repair, strip screens untouched), `daemon::animation` (the
  instant / stay rule). The VM driver `flowwm-vm/test-stage29.ps1` runs the
  cases against real windows on two displays (DISPLAY1 2/3+1/3, DISPLAY2
  1/2 and 1/1) with the z-order read from the desktop.
