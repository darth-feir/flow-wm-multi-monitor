# Book Mode: Piles

`[columns] layout = "book"` keeps every column on screen: the columns the
camera does not show are **piled** behind the leftmost and rightmost visible
columns. This page covers the daemon side — the z-order of the piles, the
animation rule, input and the lifecycle. Where a piled column's rect comes
from is in [Projection](./layout/projection.md#book-mode-piles); the
decision record is `docs/adr/0022-book-mode-piles.md`.

## Vocabulary

```text
 DISPLAY1, "2/3+1/3", strip A|B|C|D|E, view [B, C]

 ┌──────────────────────────┐ ┌────────────┐
 │ B  (left top)            │ │ C (right   │
 │   A under it (member)    │ │   top)     │
 │                          │ │   D, E     │
 │                          │ │   under it │
 └──────────────────────────┘ └────────────┘
   slot 0 = B's rect           slot 1 = C's rect
```

- **Pile**: the columns out of view on one side, every one at its edge
  column's slot rect (`ActualEntry::pile` = `Left` / `Right`).
- **Top** (top card): the visible edge column the pile lies under. With one
  visible column it is the top of both piles.
- **Member**: a piled column. Nearest to the view = highest.
- **Chain** (`piles::Chain`): one pile as z-order groups, top first —
  `[[B], [A]]` and `[[C], [D], [E]]` above. A group is one column's rows.

`piles::pile_chains(layout, actual)` builds the chains of a strip from the
projected entries (the strip order gives "nearest first"); a strip with no
visible non-fullscreen column — a fullscreen column starts the view, or the
strip-parking fallback — has none: nothing under a topmost fullscreen
window needs an order.

## Keeping the order

The invariant is checked against one `EnumWindows` read (topmost first,
`registry_win32::z_order`): a chain is **in order** when every window of a
group is above every window of the next group (`chain_in_order`; the rows of
one column may be in any order, so focusing the second row of a stacked top
is not drift). A chain out of order is re-seated walking it from the top:
each window goes directly below the previous one with
`SetWindowPos(hwnd, after, NOMOVE | NOSIZE | NOACTIVATE | NOOWNERZORDER)`
(`registry_win32::insert_below_quiet`), and its border overlay is re-seated
directly above it (`Border::seat_above_target`) so a member's ring never
shows over its top. The top's first row is never moved. Chains in order cost
nothing but the read.

`FlowWM::order_piles(monitor, layout, actual)` does this for one strip and
is called:

| Where | Why |
|---|---|
| `animate_layouts_on` (every committed layout batch) | before the animator moves anything: the outgoing edge column sinks at the rect it already has, so lowering it first changes nothing visible until the incoming card slides over it |
| `submit_animation` (drag previews) | a preview moves cards between piles |
| `animate_workspaces_on` (the incoming workspace) | its piles slide in with their tops, already in order |

`check_piles` runs the same for every book-mode monitor's active strip once
per `[focus] foreground_sync_interval_ms` (next to the floating layer's
re-assert in the main loop) and logs `piles: re-seated N window(s) out of
pile order` when the set of repaired windows changes. `seat_all_piles` is
the ungated form after startup, `tiling on`, a loadout and a hotplug (a
reload goes through `animate_layouts_on`). Nothing runs while tiling is
paused. The window a drag holds (`drag_state`) is left out of every chain
so it stays above the piles while it follows the mouse — an edge-scroll
flip mid-drag commits a layout that still has it in a pile — and
`check_piles` skips its ticks while a drag lasts (the previews seat the
piles they move). A window that cannot be lowered (an elevated one, UIPI)
is logged at debug level and tried again on the next pass.

Why not `HWND_BOTTOM`, `WS_EX_NOACTIVATE` or `EVENT_OBJECT_REORDER`: see the
ADR's considered options.

The Win32 calls are behind `piles::PileProbe` (`z_order`, `insert_below`);
the unit tests run the ordering against a mock z-order that really moves
windows.

## Animation

`push_targets_as(.., piled, settled)` in `daemon::animation`:

| Target | Decision |
|---|---|
| a pile slot, the window last sent there, animator idle | `Stay` (no target) |
| a pile slot, otherwise | `Instant` (unless `plan_move` already says `Stay` / `Instant`) |
| out of a pile into view | `Tween` (surfacing) |

So a one-column flip under 1/2 or 2/3+1/3 moves exactly one window per side,
and a carousel flip moves none: only the z-order changes. "Animator idle" is
what makes `Stay` safe: with no batch in flight, the window is where it was
last sent; during rapid flips the instant re-placement is harmless. Workspace
batches do not use the rule: piles travel with their workspace.

## Input

- **Activation.** A taskbar click, Alt+Tab or an app activating its own
  window goes through `on_focus_changed` → `ensure_focused_visible` →
  `animate_layout` as in strip mode: the book flips to the window in one
  batch. Windows raises the activated window itself.
- **Hit-testing.** Members share their top's rect exactly, so only the top
  is reachable with the mouse; overlays are `WS_EX_TRANSPARENT`.
- **Drops.** Pile members are not drop targets (`build_visible_columns`
  skips piled entries). The whole of a top card whose pile has a member is
  one zone, `DropZone::Card { col, side }`: the window is inserted right
  after the left top / right before the right top and
  `ScrollingSpace::edge_layout` puts it at that edge of the view (pinned to
  the slots under a pattern), so it becomes the new top with the old top
  right behind it; `FlowWM::card_camera` (`daemon::drag`) applies that to previews and to the
  commit. A lone visible column is both tops: the half under the cursor
  picks the pile. The screen-edge scroll bands and the gaps beside a card
  are resolved before it, so a card zone is the card minus the band; an
  edge-hold scroll is a flip. See [Tile Drag](./tile-drag.md).
- **Focus wrap.** `[columns] wrap_around` (`off` / `on` / `book`) lives in
  `MutationConfig::wrap_around`; `mutations::focus` wraps left/right from one
  end of the strip to the other when `WrapAround::wraps(layout_mode)`. A wrap
  replaces the edge hop at that end.

## Mode switch

`ScrollingSpace::set_layout_mode(mode, wrap)` re-projects without touching
the strip. The mode comes from `[columns] layout` (or the screen's
`[[columns.screen]]` rule, `ColumnsConfig::layout_for`) through
`LayoutConfig` at startup, hotplug and reload, and from `flow dispatch
set-layout strip|book|toggle` (`FlowWM::dispatch_set_layout`, every
workspace of the focused window's screen) at runtime; a reload puts every
screen back to the file and animates every monitor's active workspace in one
batch. Strip → book places the parked columns instantly under their tops;
book → strip slides them off from under their tops on a free side.

## Lifecycle

| Event | Book mode |
|---|---|
| minimize / hide / close of a member | it leaves the strip as in strip mode; the order of the rest is unchanged |
| close of a top | the next member is the new top (already directly under it) |
| restore / show | as in strip mode, the window returns to its restore seat (the column and row it left, clamped; ADR-0027, `[minimize_restore] strategy`) and the view follows it into the open run, not a pile; see [Event Pipelines](./event-pipelines.md#restored-event_system_minimizeend) |
| workspace switch | the whole book (piles too) parks vertically with its workspace and comes back with its piles in order |
| `move-to-monitor` | both strips re-project; each seats its own piles |
| `tiling off` / `flow stop` / fail-back | nothing to undo: every window is on screen and in the taskbar |
| fullscreen column in view | the others lie under it at their own width; no chains |

`flow query all` reports `layout` and `wrap_around` per monitor and `pile`
per column and per window.
