# Projection

The projection function converts the abstract virtual canvas into concrete pixel rectangles that Windows OS can render. Given a `VirtualLayout`, a `MonitorInfo`, and a `Padding`, the [`project`](../../src/layout/projection.rs) function computes an `ActualLayout` containing one entry per window with an exact screen-coordinate `Rect`. This is the only place in the layout module where pixel coordinates materialize — and the only place where padding is applied.

## The slot model

Columns are laid out in **slots** on the virtual canvas. Each slot is the column's own `width_px` plus a `window_gap` on the right. The canvas starts at `window_gap` (the initial left-edge gap), and the last column's trailing gap serves as the right-edge gap. This structure means the inter-column gap emerges from the layout itself, not from insetting individual windows.

```mermaid
graph LR
    subgraph Canvas["Virtual Canvas"]
        direction LR
        G0["gap"]
        C0["Column 0\n960px"]
        G1["gap"]
        C1["Column 1\n1440px"]
        G2["gap"]
        C2["Column 2\n960px"]
    end

    G0 --- C0 --- G1 --- C1 --- G2 --- C2

    style G0 fill:#fdd,stroke:#999
    style G1 fill:#fdd,stroke:#999
    style G2 fill:#fdd,stroke:#999
    style Canvas fill:#fff,stroke:#333
```

Column x-positions are a **prefix sum**, not a uniform stride. The canvas accumulator starts at `window_gap` and advances by `col.width_px + window_gap` per column. This is important because columns can have different widths (after expand/shrink or drag-resize), so a uniform stride would not work.

## Camera shift: canvas to screen

For each column, projection computes its canvas range `[canvas_left, canvas_right]` and checks whether it overlaps the viewport `[viewport_offset, viewport_offset + monitor_width]`. Visible columns are translated to screen coordinates by subtracting the camera offset:

```
screen_x = monitor_left + (canvas_col_left - viewport_offset)
```

This subtraction is the entire camera mechanism. The `viewport_offset` on the `VirtualLayout` shifts the slice of canvas that maps onto the physical screen. A column whose canvas x-position is less than the offset gets a negative (or off-screen-left) screen x; a column far to the right gets a screen x beyond the monitor right edge.

## Visibility test

A column is visible when its canvas range partially overlaps the viewport:

```
visible = (canvas_col_right > viewport_left) && (canvas_col_left < viewport_right)
```

This is a standard interval overlap test. Columns that overlap even by one pixel are projected at their real screen coordinates; columns with no overlap are parked.

## Row rects: source-of-truth heights

Each `Row` in a column carries its own `height: i32` field, and projection consumes that value **verbatim** as the window's pixel height. There is no equal-division step here, no recomputation, no rescaling, no inset — projection simply stacks the rows. Equal-height distribution is performed exactly once, at mutation time, by [`distribute_heights`](../../src/layout/mutations.rs) (see [Mutations](./mutations.md)). Between mutations, row heights are intentionally stable so that future drag-resize or IPC-driven continuous-height adjustment can preserve user-customized heights.

The vertical stacking model: starting at `y = monitor_y + padding.up + window_gap`, each row occupies `row.height` pixels, and a single `window_gap` separates consecutive rows:

```
y_cursor = monitor_y + padding.up + window_gap
for row in column.rows:
    window.rect = (col_x, y_cursor, col_width, row.height)
    y_cursor += row.height + window_gap
```

This produces one `window_gap` between adjacent rows, one `window_gap` above the topmost row (after `padding.up`), and one trailing `window_gap` below the bottommost row (before `padding.down`). The `row.height` values are the sole determinant of where each window lands vertically.

## The container model: from column cell to window rect

Because `row.height` is consumed as the literal window height, there is **no inset within a row cell**. Horizontally, the window fills the full `col_width` — the gap between columns comes from the slot model (the `window_gap` between slots), not from insetting the window. Vertically, each window's height is exactly `row.height`, and the gaps between windows come from the `+ window_gap` term in the stacking formula above.

```mermaid
graph TB
    subgraph Column["Column (vertical stack)"]
        direction TB
        TG["top gap: window_gap (after padding.up)"]
        W0["Row 0 Window\nx = col_x\ny = monitor_y + up + gap\nwidth = col_width\nheight = row.height[0]"]
        G1["inter-row gap: window_gap"]
        W1["Row 1 Window\nwidth = col_width\nheight = row.height[1]"]
        BG["bottom gap: window_gap (before padding.down)"]
    end

    TG --- W0 --- G1 --- W1 --- BG

    style TG fill:#fdd,stroke:#999
    style G1 fill:#fdd,stroke:#999
    style BG fill:#fdd,stroke:#999
    style W0 fill:#dfd,stroke:#333,stroke-width:2px
    style W1 fill:#dfd,stroke:#333,stroke-width:2px
```

This is a deliberate departure from the earlier "equal cell division + inset" model. The new contract is: *the value on `Row.height` is the HWND height that Windows receives*. Anything that wants to change a window's height — equal redistribution, drag-resize, explicit IPC — must write to `Row.height` through the mutation layer; projection never second-guesses it.

## Screen-level margins

The `padding.up` and `padding.down` fields are screen-level margins that reserve space above and below the tiling area. They reduce the available height for row calculation: `available_height = monitor_height - up - down`. Windows never extend into these margin zones. This is distinct from `window_gap` (which applies between windows and between windows and screen edges within the tiling area) — `up`/`down` are for external UI elements like custom title bars or taskbar clearance.

## Parking

Columns that are not visible are **parked** at deterministic off-screen positions rather than left at their unreachable virtual canvas coordinates. Windows OS does not gracefully handle windows placed at extreme off-screen positions — they can cause rendering artifacts, accessibility issues, and broken animation transitions.

```mermaid
graph LR
    subgraph Monitor["Monitor"]
        VP["Viewport"]
    end

    PL["Left Parking\nx = monitor_left - col_width"]
    PR["Right Parking\nx = monitor_right"]

    PL -.->|"parked left"| Monitor
    Monitor -.->|"parked right"| PR

    style PL fill:#f9f,stroke:#333,stroke-dasharray: 5 5
    style PR fill:#f9f,stroke:#333,stroke-dasharray: 5 5
    style Monitor fill:#bbf,stroke:#333,stroke-width:2px
```

There are two parking zones:

- **Left parking**: `monitor_left - col_width`. For columns whose canvas right edge is at or before the viewport left edge.
- **Right parking**: `monitor_right`. For columns whose canvas left edge is at or beyond the viewport right edge.

Parked windows use the same `row.height`-stacking logic as visible windows, so when a column scrolls from parked to visible (or vice versa), its windows animate smoothly with consistent dimensions — there is no sudden resize or padding change at the boundary.

## Row viewports (stacked columns)

`project` takes a fourth argument, `visible_rows` (`[columns] visible_rows`,
carried as `MutationConfig::visible_rows`; `0` = no viewport). A column with
more rows than that shows only `visible_rows` of them — the **slots** — from
`Column::row_offset` on, stacked exactly like a column of that many rows (the
row heights were distributed over the slot count by the mutation layer, see
[`column_heights`](./mutations.md#row-heights-source-of-truth-model)). The
other rows are **parked vertically** at the column's own x:

| rows                                   | y                                                      |
|----------------------------------------|--------------------------------------------------------|
| before the offset (parked above)       | bottom edge at `park_above_bottom`: `PARK_MARGIN` above the monitor's physical screen rect, or above the **virtual screen** when a monitor is stacked above (`neighbour_up`) |
| `row_offset .. row_offset + slots`     | stacked from the top of the work area as usual         |
| after the viewport (parked below)      | top edge at `park_below_top`: `PARK_MARGIN` below the screen rect, or below the virtual screen when a monitor is stacked below (`neighbour_down`) |

This is the workspace-parking rule applied to rows (one shared slot per side,
the physical screen rect rather than the work area so a parked row clears a
taskbar, the far slot on a blocked side so it never lands on the neighbour),
which is why `MonitorInfo` now carries `screen_rect`. The parked rows of a
column that is itself parked left/right keep their vertical slot too, so they
are off-screen on both axes. The animation bridge needs no new rule: a rect
parked above or below the virtual screen is classified by `park_sides` like
a parked workspace and the move is instant when that side has a neighbour
([Animation](../animation.md#instant-targets-and-the-neighbour-teleport-rule)).
A stale `row_offset` (rows removed without a clamp) is projected as if
clamped, so a column never hides every row.

When a column has no more rows than `visible_rows`, or `visible_rows` is
`0`, the output is identical to the flat model above.

## Why projection is the only place padding lives

Padding is applied exclusively during projection and never stored on `Column`, `VirtualLayout`, or individual windows. This design choice keeps the virtual canvas model clean: columns are pure containers of windows with widths, and the spatial realities of screen edges, gaps, and margins are deferred to the one function that converts canvas geometry to screen coordinates. The `ActualEntry` rects produced by projection are the **final HWND rects** — they can be passed directly to `SetWindowPos` without any further adjustment.

This separation has a practical benefit: the mutation layer operates on a padding-agnostic model. Swap, expand, shrink, and scroll never need to know about gaps or margins. Only the projection function — a single deterministic function with no side effects — handles the translation from the abstract to the physical.

See [Overview](./overview.md) for the VirtualLayout/ActualLayout type relationship and [Mutations](./mutations.md) for the operations that produce the `VirtualLayout` inputs to projection.

> **Multi-monitor note.** Parking zones are measured from the *virtual screen*
> (the bounding box of every monitor, `MonitorInfo::virtual_screen`), not from
> the monitor's own edge: left zone at `virtual_left - col_width - PARK_MARGIN`,
> right zone at `virtual_right + PARK_MARGIN`. On a single monitor this is the
> same as before. A partially visible column that would overhang an edge with a
> neighbouring monitor beyond it (`MonitorInfo::neighbour_left/right`) is parked
> instead, because Windows cannot clip a window at a monitor edge. Overhang is
> judged against the *work area*, so a column tucked under a left-docked
> taskbar counts as overhanging. The daemon moves windows whose slide would
> cross a neighbour instantly instead of tweening them — see
> [Instant targets and the teleport rule](../animation.md#instant-targets-and-the-neighbour-teleport-rule).

### Neighbours on four sides

`MonitorInfo::in_desktop` derives one flag per edge from the other monitors'
screen rects. A monitor is a **left/right** neighbour when it lies beyond that
edge and its y-range overlaps ours; an **up/down** neighbour
(`MonitorInfo::neighbour_up/down`) when it lies beyond the top/bottom edge and
its *x-range* overlaps ours. A display that is only diagonally offset
(overlapping neither band) is no neighbour on any side: nothing parked
straight out from an edge can land on it. For a laptop at `(0,0) 1920×1080`
with an external `2560×1440` at `(0,-1440)` the laptop has `neighbour_up`
only, the external `neighbour_down` only, and neither has a left/right
neighbour — so column parking on a stacked pair is exactly the single-monitor
rule, measured from the virtual screen.

The vertical flags are not read by `project` at all: columns only ever park
horizontally. They drive **workspace parking**, which lives in the daemon
(`workspace::y_offset::WorkspaceParking`) because projection is
workspace-local. The rule mirrors the horizontal one:

| side        | free (no neighbour)                        | blocked (neighbour there)                                  |
|-------------|--------------------------------------------|------------------------------------------------------------|
| above (`id < active`) | one monitor height up: `-(screen_height + window_gap)` | strip bottom at `virtual_screen.y - PARK_MARGIN`     |
| below (`id > active`) | one monitor height down: `+(screen_height + window_gap)` | strip top at `virtual_screen.bottom() + PARK_MARGIN` |

The offset is measured from the monitor's full physical screen rect, never
the work area, and every workspace on one side shares that side's slot (the
±1-unit model of [Workspace](../workspace.md)). A free side keeps the short
slot so a switch still reads as the workspace strip sliding up or down; a
blocked side uses the far slot, and the animation bridge then moves the
windows instantly (same decision as for columns, see the animation chapter).
A monitor with a display above and nothing below therefore slides workspaces
in from below and teleports them out above.
