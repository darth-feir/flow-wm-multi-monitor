//! Virtual layout → actual layout projection (camera to screen).
//!
//! The core **camera-to-screen** projection: takes the infinite virtual canvas
//! ([`VirtualLayout`]) and computes real pixel coordinates for every window
//! ([`ActualLayout`]) that Windows OS must render.
//!
//! # How it works
//!
//! 1. **Camera shift**: each column's virtual x-position is offset by subtracting
//!    `viewport_offset` (the camera position). Columns whose shifted range
//!    overlaps the monitor rectangle are **visible** and receive on-screen
//!    coordinates.
//!
//! 2. **Parking**: columns fully outside the viewport are **parked** just
//!    beyond the nearest edge of the *virtual screen* (the bounding box of all
//!    monitors, [`MonitorInfo::virtual_screen`]) — left zone
//!    (`virtual_left - col_width - PARK_MARGIN`) or right zone
//!    (`virtual_right + PARK_MARGIN`). Parking relative to the virtual screen
//!    rather than the monitor is what keeps a parked column off a neighbouring
//!    monitor on multi-monitor desktops; on a single monitor the two are the
//!    same. A partially visible column that would overhang an edge with a
//!    neighbour beyond it ([`MonitorInfo::neighbour_left`] /
//!    [`MonitorInfo::neighbour_right`]) is parked too, since Windows cannot
//!    clip a window at a monitor edge.
//!
//! Columns are laid out in **slots** of width `col_width + window_gap`, so the
//! visual gap between adjacent columns comes from the slot structure (the canvas
//! starts at `window_gap`). Vertically, rows are stacked with one `window_gap`
//! between consecutive rows plus a leading `window_gap` after `padding.up`;
//! screen-level top/bottom margins come from `padding.up` / `padding.down`.
//!
//! # Row heights are source-of-truth here
//!
//! Each [`Row`](crate::layout::types::Row)'s `height` field is consumed
//! **verbatim** as the window's pixel height. Projection never recomputes,
//! rescales, or insets row heights — it only stacks them. Equal distribution
//! of available vertical space happens once, at mutation time
//! ([`distribute_heights`](crate::layout::mutations::distribute_heights)),
//! whenever row membership changes (add/remove/merge/promote). Between
//! mutations, heights are stable so the user can drag-resize later.
//!
//! # Padding lives here
//!
//! Padding is applied **here**, during projection — outside the window concept.
//! The [`ActualEntry`] rects produced are the **final HWND rects**, passable
//! directly to `SetWindowPos` with no further adjustment. This keeps padding
//! logic in one place so no other consumer of window rects needs to know about it.
//!
//! See the developer guide's *Projection* chapter
//! (`docs/src/dev-guide/layout/projection.md`) for the slot model and parking
//! zones with diagrams.

use crate::common::Rect;
use crate::layout::types::{
    ActualEntry, ActualLayout, Column, MonitorInfo, Padding, VirtualLayout,
};

/// Margin between the virtual screen's edge and a parked column, in pixels.
/// Small so scroll-in/out animations on a free edge stay short.
pub const PARK_MARGIN: i32 = 16;

/// Project a virtual layout onto a monitor viewport.
///
/// This is the **camera-to-screen** conversion. For each column on the infinite
/// canvas, it:
///
/// 1. **Visible columns**: Computes real screen coordinates by subtracting the
///    camera offset (`viewport_offset`) from the column's virtual x-position.
///
/// 2. **Off-screen-left columns**: Parks at [`park_left_x`] (just beyond the
///    virtual screen's left edge).
///
/// 3. **Off-screen-right columns**: Parks at [`park_right_x`] (just beyond the
///    virtual screen's right edge).
///
/// 4. **Overhanging columns**: A visible column that would extend past a
///    monitor edge with another monitor beyond it is parked on that side
///    instead of spilling onto the neighbour.
///
/// Every window gets an [`ActualEntry`] — visible or parked. This ensures the
/// diff engine can track smooth transitions when windows scroll in/out.
///
/// # Slot Model
///
/// Each column occupies a **slot** on the virtual canvas whose width is that
/// column's own [`width_px`](crate::layout::types::Column::width_px) plus
/// `window_gap`. The canvas starts at `window_gap` (left-edge gap). Column
/// x-positions are therefore a **prefix sum**, not a uniform stride — the
/// running `canvas_x` accumulator in [`project`] advances by
/// `col.width_px + window_gap` per column.
///
/// The window rect fills the full `col.width_px` (no horizontal inset); the
/// gap between columns comes from the slot structure itself.
#[must_use]
pub fn project(
    virtual_layout: &VirtualLayout,
    monitor: &MonitorInfo,
    padding: &Padding,
) -> ActualLayout {
    let monitor_rect = monitor.work_area;
    let viewport_left = virtual_layout.viewport_offset;
    let viewport_right = viewport_left + monitor_rect.width;
    let slot_gap = padding.window_gap;

    let mut entries = Vec::new();
    let mut canvas_x: i32 = slot_gap;

    for column in &virtual_layout.columns {
        let col_width = column.width_px;
        let canvas_col_left = canvas_x;
        let canvas_col_right = canvas_x + col_width;

        let visible = canvas_col_right > viewport_left && canvas_col_left < viewport_right;

        let x = if visible {
            let on_screen_x = monitor_rect.x + (canvas_col_left - viewport_left);
            // A column that overhangs an edge with a neighbouring monitor
            // beyond it would spill onto that neighbour. Park it instead —
            // whatever its width: a column wider than the work area (a
            // monitor that shrank under a hotplug, an oversized configured
            // width) must not be exempt, or it would cover the neighbour.
            // Monocle never overhangs (its width is `monitor_width − 2·gap`
            // at the viewport's left edge), so it is never parked by this.
            let overhangs_left = on_screen_x < monitor_rect.x;
            let overhangs_right = on_screen_x + col_width > monitor_rect.right();
            if overhangs_left && monitor.neighbour_left {
                park_left_x(monitor, col_width)
            } else if overhangs_right && monitor.neighbour_right {
                park_right_x(monitor)
            } else {
                on_screen_x
            }
        } else if canvas_col_right <= viewport_left {
            park_left_x(monitor, col_width)
        } else {
            park_right_x(monitor)
        };

        stack_column_rows(column, x, monitor_rect, col_width, padding, &mut entries);
        canvas_x += col_width + slot_gap;
    }

    ActualLayout { entries }
}

/// X coordinate of the left parking zone: just beyond the virtual screen's
/// left edge, so the column is off every monitor, not merely off this one.
#[must_use]
pub fn park_left_x(monitor: &MonitorInfo, col_width: i32) -> i32 {
    monitor.virtual_screen.x - col_width - PARK_MARGIN
}

/// X coordinate of the right parking zone: just beyond the virtual screen's
/// right edge.
#[must_use]
pub fn park_right_x(monitor: &MonitorInfo) -> i32 {
    monitor.virtual_screen.right() + PARK_MARGIN
}

fn stack_column_rows(
    column: &Column,
    x: i32,
    monitor_rect: Rect,
    col_width: i32,
    padding: &Padding,
    entries: &mut Vec<ActualEntry>,
) {
    if column.rows.is_empty() {
        return;
    }

    // First row starts one gap below the top padding edge; each subsequent row
    // starts one gap below the previous row's bottom.
    let mut y = monitor_rect.y + padding.up + padding.window_gap;

    for row in &column.rows {
        entries.push(ActualEntry {
            window_id: row.window_id,
            rect: Rect {
                x,
                y,
                width: col_width,
                height: row.height,
            },
        });
        y += row.height + padding.window_gap;
    }
}

/// Compute the canvas width consumed by all columns using the slot model.
///
/// Each column occupies a slot of `col.width_px + window_gap`, starting from
/// `window_gap` (the initial left-edge gap). The total canvas width is:
/// `window_gap + sum(width_px_i + window_gap)`.
///
/// When `window_gap = 0`, this degenerates to the packed model (sum of col widths).
#[must_use]
pub fn canvas_width(layout: &VirtualLayout, window_gap: i32) -> i32 {
    if layout.columns.is_empty() {
        return 0;
    }
    let total_slots: i32 = layout.columns.iter().map(|c| c.width_px + window_gap).sum();
    // Leading edge gap is the window_gap at the very start of the canvas.
    // But each slot already includes its trailing gap, and the first slot starts
    // at window_gap. So the total span from canvas_x=0 to the end of the last
    // slot's gap is: window_gap (initial) + sum(col_width_i + window_gap).
    // The trailing gap of the last column is the right-edge gap.
    window_gap + total_slots
}

/// Compute the pixel width of a single step (one slot = column's own width + window gap).
/// Used by scroll operations to determine viewport offset changes.
#[must_use]
pub fn column_step_width(column: &Column, window_gap: i32) -> i32 {
    column.width_px + window_gap
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::WindowId;
    use crate::layout::types::Row;

    fn test_monitor() -> MonitorInfo {
        MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        })
    }

    fn test_padding() -> Padding {
        Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        }
    }

    #[test]
    fn project_single_column_fills_monitor() {
        // Row height pre-distributed by mutation layer: (1080 - 2*4) / 1 = 1072.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_row(1920, Row::new(WindowId(1), 1072))],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 1);
        let entry = &actual.entries[0];
        assert_eq!(entry.window_id, WindowId(1));
        // Slot model: canvas starts at window_gap=4, window fills full width
        // window x = 0 + (4 - 0) = 4
        // window width = 1920 (full column width, no horizontal inset)
        // y = monitor_y + padding.up + window_gap = 0 + 0 + 4 = 4
        // height = row.height (consumed verbatim) = 1072
        assert_eq!(entry.rect.x, 4);
        assert_eq!(entry.rect.y, 4);
        assert_eq!(entry.rect.width, 1920);
        assert_eq!(entry.rect.height, 1072);
    }

    #[test]
    fn project_two_equal_columns() {
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 2);

        let col_width = 960; // base column width (px)
        // Slot model: canvas starts at window_gap=4
        // Col 0: canvas_x = 4, screen_x = 0 + (4-0) = 4
        assert_eq!(actual.entries[0].rect.x, 4);
        assert_eq!(actual.entries[0].rect.width, col_width);
        // Col 1: canvas_x = 4 + 960 + 4 = 968, screen_x = 0 + (968-0) = 968
        assert_eq!(actual.entries[1].rect.x, col_width + 2 * 4);
        assert_eq!(actual.entries[1].rect.width, col_width);
    }

    #[test]
    fn project_column_with_two_rows() {
        // Two rows stacked: each height 534, gap 4.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 534), Row::new(WindowId(2), 534)],
            )],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 2);
        assert_eq!(actual.entries[0].rect.x, actual.entries[1].rect.x);
        // row 0 y = 4, row 1 y = 4 + 534 + 4 = 542
        assert!(actual.entries[1].rect.y > actual.entries[0].rect.y);
    }

    #[test]
    fn off_screen_left_parked_one_column_beyond() {
        let monitor = test_monitor();
        let padding = test_padding();

        // Two columns, viewport scrolled past the first
        // Slot model: col 0 at canvas_x=4, col 1 at canvas_x=4+960+4=968
        // viewport_offset = 968 means viewport starts at col 1's canvas position
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            968, // offset = start of col 1's slot
        );
        let actual = project(&layout, &monitor, &padding);

        // First column parked just beyond the virtual screen's left edge
        assert_eq!(actual.entries[0].rect.x, -960 - PARK_MARGIN);
        // Second column visible on screen
        assert!(actual.entries[1].rect.x >= 0);
    }

    #[test]
    fn off_screen_right_parked_one_column_beyond() {
        let monitor = test_monitor();
        let padding = test_padding();

        // Three columns, viewport at 0, only first two visible on 1920px
        // Slot model: col 0 at canvas_x=4, col 1 at canvas_x=968, col 2 at canvas_x=1932
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)), // off-screen right
            ],
            0,
        );
        let actual = project(&layout, &monitor, &padding);

        // Third column parked just beyond the virtual screen's right edge
        assert_eq!(actual.entries[2].rect.x, 1920 + PARK_MARGIN);
    }

    #[test]
    fn canvas_width_empty() {
        let layout = VirtualLayout::new();
        assert_eq!(canvas_width(&layout, 4), 0);
    }

    #[test]
    fn canvas_width_two_columns() {
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        // Slot model: window_gap(4) + (960+4) + (960+4) = 4 + 964 + 964 = 1932
        assert_eq!(canvas_width(&layout, 4), 1932);
    }

    // --- Integration: Projection correctness ---

    #[test]
    fn project_three_varying_width_columns_exact_pixels() {
        // Positive: 240px + 720px + 960px columns
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(240, Row::new(WindowId(1), 0)),
                Column::with_row(720, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());
        assert_eq!(actual.entries.len(), 3);

        // Slot model: canvas starts at window_gap=4
        // Column 1: 240px, canvas_x=4, screen_x = 0 + (4-0) = 4
        assert_eq!(actual.entries[0].rect.x, 4);
        assert_eq!(actual.entries[0].rect.width, 240);

        // Column 2: 720px, canvas_x = 4 + 240 + 4 = 248, screen_x = 248
        assert_eq!(actual.entries[1].rect.x, 248);
        assert_eq!(actual.entries[1].rect.width, 720);

        // Column 3: 960px, canvas_x = 248 + 720 + 4 = 972, screen_x = 972
        assert_eq!(actual.entries[2].rect.x, 972);
        assert_eq!(actual.entries[2].rect.width, 960);
    }

    #[test]
    fn project_visible_tiles_cover_full_monitor_width() {
        // Positive: with 0 padding, visible tiles cover exactly the col widths
        let zero_padding = Padding {
            window_gap: 0,
            up: 0,
            down: 0,
        };
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            0,
        );
        let actual = project(&layout, &test_monitor(), &zero_padding);
        let total_width: i32 = actual.entries.iter().map(|e| e.rect.width).sum();
        assert_eq!(
            total_width, 1920,
            "visible tiles must cover full monitor width with zero gap"
        );
    }

    #[test]
    fn project_parked_tiles_left_and_right_simultaneously() {
        // Positive: with 5 columns at 4/8 each, viewport showing cols 2-3,
        // cols 0-1 parked left, col 4 parked right
        // Slot model: col i at canvas_x = 4 + i * (960+4) = 4 + i*964
        // col 0: 4, col 1: 968, col 2: 1932, col 3: 2896, col 4: 3860
        // viewport at 1932: visible [1932, 3852)
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
                Column::with_row(960, Row::new(WindowId(4), 0)),
                Column::with_row(960, Row::new(WindowId(5), 0)),
            ],
            1932, // offset: start of col 2's slot
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        // Col 0 (canvas 4–964): 964 <= 1932 → parked left
        assert_eq!(actual.entries[0].rect.x, -960 - PARK_MARGIN);
        // Col 1 (canvas 968–1928): 1928 <= 1932 → parked left
        assert_eq!(actual.entries[1].rect.x, -960 - PARK_MARGIN);

        // Col 2 (canvas 1932–2892): visible
        assert!(actual.entries[2].rect.x >= 0, "col 2 should be visible");

        // Col 3 (canvas 2896–3856): visible (2896 < 3852)
        assert!(actual.entries[3].rect.x >= 0, "col 3 should be visible");

        // Col 4 (canvas 3860–4820): 3860 >= 3852 → parked right at 1920
        assert_eq!(actual.entries[4].rect.x, 1920 + PARK_MARGIN);
    }

    #[test]
    fn project_parked_tiles_no_overlap_with_visible() {
        // Positive: parked tiles must not overlap the monitor area
        // Slot model: col 0 at canvas 4, col 1 at canvas 968, col 2 at canvas 1932
        // viewport at 968 shows col 1 and col 2
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            968, // offset past col 0
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        // Col 0 is parked left — must not overlap monitor area
        let parked_entry = &actual.entries[0];
        let parked_right = parked_entry.rect.x + parked_entry.rect.width;
        assert!(
            parked_right <= 0,
            "parked left tile overlaps visible: right edge = {parked_right}"
        );
    }

    #[test]
    fn project_nonzero_viewport_offset() {
        // Positive: viewport offset shifts visible columns right
        // Slot model: col 0 at canvas_x=4, col 1 at canvas_x=968, col 2 at canvas_x=1932
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            500, // non-zero offset
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        // Col 0: canvas_x=4, screen_x = 0 + (4 - 500) = -496
        assert_eq!(actual.entries[0].rect.x, -496);

        // Col 1: canvas_x=968, screen_x = 0 + (968 - 500) = 468
        assert_eq!(actual.entries[1].rect.x, 468);

        // Col 2: canvas_x=1932, screen_x = 0 + (1932 - 500) = 1432
        assert_eq!(actual.entries[2].rect.x, 1432);
    }

    #[test]
    fn project_two_rows_equal_height() {
        // Two rows pre-distributed equally by mutation layer.
        // User formula: (1080 - (2+1)*4) / 2 = 1068 / 2 = 534 each.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                1920,
                vec![Row::new(WindowId(1), 534), Row::new(WindowId(2), 534)],
            )],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 2);
        // row.height consumed verbatim — no inset applied by projection.
        assert_eq!(actual.entries[0].rect.height, 534);
        assert_eq!(actual.entries[1].rect.height, 534);
        // row 0 y = 0 + 0 + 4 = 4
        assert_eq!(actual.entries[0].rect.y, 4);
        // row 1 y = 4 + 534 + 4 = 542
        assert_eq!(actual.entries[1].rect.y, 542);
    }

    #[test]
    fn project_single_column_narrow_width() {
        // Positive: narrow 240px column
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(240, Row::new(WindowId(1), 0))], 0);
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 1);
        // Slot model: full col_width, no horizontal inset
        assert_eq!(actual.entries[0].rect.width, 240);
    }

    #[test]
    fn project_empty_layout_yields_empty_actual() {
        // Positive: empty virtual layout → no entries
        let layout = VirtualLayout::new();
        let actual = project(&layout, &test_monitor(), &test_padding());
        assert!(actual.entries.is_empty());
    }

    #[test]
    fn canvas_width_single_column() {
        // Positive: single column → window_gap + col_width + window_gap
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1920, Row::new(WindowId(1), 0))], 0);
        // 4 + (1920 + 4) = 1928
        assert_eq!(canvas_width(&layout, 4), 1928);
    }

    #[test]
    fn canvas_width_five_columns() {
        // Positive: 5 × 960px columns → 4 + 5*(960+4) = 4 + 4820 = 4824
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
                Column::with_row(960, Row::new(WindowId(4), 0)),
                Column::with_row(960, Row::new(WindowId(5), 0)),
            ],
            0,
        );
        assert_eq!(canvas_width(&layout, 4), 4824);
    }

    #[test]
    fn project_with_up_and_down_padding() {
        // Positive: up/down padding creates screen-level margins
        let padding = Padding {
            window_gap: 4,
            up: 10,
            down: 40,
        };
        // Row height pre-distributed: available = 1080 - 10 - 40 = 1030,
        // (1030 - 2*4) / 1 = 1022.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_row(1920, Row::new(WindowId(1), 1022))],
            0,
        );
        let actual = project(&layout, &test_monitor(), &padding);

        assert_eq!(actual.entries.len(), 1);
        let entry = &actual.entries[0];
        // y = monitor_y + padding.up + window_gap = 0 + 10 + 4 = 14
        // height = row.height consumed verbatim = 1022
        // Slot model: window x = window_gap = 4, width = 1920 (no horizontal inset)
        assert_eq!(entry.rect.y, 14);
        assert_eq!(entry.rect.height, 1022);
    }

    #[test]
    fn project_consumes_row_height_verbatim_uneven() {
        // Rows need NOT be equal — projection is a pure consumer of row.height.
        // This test pins the source-of-truth contract: whatever heights the
        // mutation layer hands us, we stack verbatim.
        let layout = VirtualLayout::with_columns(
            vec![Column::with_rows(
                960,
                vec![Row::new(WindowId(1), 200), Row::new(WindowId(2), 800)],
            )],
            0,
        );
        let actual = project(&layout, &test_monitor(), &test_padding());

        assert_eq!(actual.entries.len(), 2);
        // row 0: y = 0 + 0 + 4 = 4, height = 200 verbatim
        assert_eq!(actual.entries[0].rect.y, 4);
        assert_eq!(actual.entries[0].rect.height, 200);
        // row 1: y = 4 + 200 + 4 = 208, height = 800 verbatim
        assert_eq!(actual.entries[1].rect.y, 208);
        assert_eq!(actual.entries[1].rect.height, 800);
    }

    // ---- multi-monitor parking ------------------------------------------

    /// Primary at 0..1920 with a second monitor to its LEFT (-1920..0), as on
    /// a side-by-side desktop whose secondary sits left of the primary.
    fn primary_with_left_neighbour() -> MonitorInfo {
        let primary = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let left = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let virtual_screen = Rect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        MonitorInfo::in_desktop(primary, primary, virtual_screen, &[left])
    }

    /// The left monitor of the same desktop: its free edge is the left one.
    fn secondary_with_right_neighbour() -> MonitorInfo {
        let primary = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let left = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let virtual_screen = Rect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        MonitorInfo::in_desktop(left, left, virtual_screen, &[primary])
    }

    #[test]
    fn in_desktop_detects_neighbours_by_side() {
        let m = primary_with_left_neighbour();
        assert!(m.neighbour_left);
        assert!(!m.neighbour_right);
        // Side by side: nothing above or below.
        assert!(!m.neighbour_up && !m.neighbour_down);
        let m = secondary_with_right_neighbour();
        assert!(!m.neighbour_left);
        assert!(m.neighbour_right);
        assert!(!m.neighbour_up && !m.neighbour_down);
        let m = MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        });
        assert!(!m.neighbour_left && !m.neighbour_right);
        assert!(!m.neighbour_up && !m.neighbour_down);
    }

    /// Bounding box of a set of rects (the Win32 virtual screen).
    fn union(rects: &[Rect]) -> Rect {
        let x0 = rects.iter().map(|r| r.x).min().unwrap_or(0);
        let y0 = rects.iter().map(|r| r.y).min().unwrap_or(0);
        let x1 = rects.iter().map(|r| r.right()).max().unwrap_or(0);
        let y1 = rects.iter().map(|r| r.bottom()).max().unwrap_or(0);
        Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        }
    }

    #[test]
    fn in_desktop_stacked_pair_has_vertical_neighbours_only() {
        // Laptop at (0,0) 1920x1080 with a wider 2560x1440 external above it.
        let laptop = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let external = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        let vs = union(&[laptop, external]);
        let m = MonitorInfo::in_desktop(laptop, laptop, vs, &[external]);
        assert!(m.neighbour_up && !m.neighbour_down);
        assert!(!m.neighbour_left && !m.neighbour_right);
        let m = MonitorInfo::in_desktop(external, external, vs, &[laptop]);
        assert!(!m.neighbour_up && m.neighbour_down);
        assert!(!m.neighbour_left && !m.neighbour_right);
    }

    #[test]
    fn in_desktop_diagonal_offset_has_no_neighbours() {
        // Corner to corner: no band overlap on either axis, so nothing
        // parked straight out from any edge can land on the other.
        let a = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let b = Rect {
            x: 1920,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        let vs = union(&[a, b]);
        let m = MonitorInfo::in_desktop(a, a, vs, &[b]);
        assert!(!m.neighbour_left && !m.neighbour_right);
        assert!(!m.neighbour_up && !m.neighbour_down);
        let m = MonitorInfo::in_desktop(b, b, vs, &[a]);
        assert!(!m.neighbour_left && !m.neighbour_right);
        assert!(!m.neighbour_up && !m.neighbour_down);
    }

    #[test]
    fn in_desktop_two_by_two_grid_has_one_neighbour_per_touching_edge() {
        let w = 1920;
        let h = 1080;
        let tl = Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        };
        let tr = Rect {
            x: w,
            y: 0,
            width: w,
            height: h,
        };
        let bl = Rect {
            x: 0,
            y: h,
            width: w,
            height: h,
        };
        let br = Rect {
            x: w,
            y: h,
            width: w,
            height: h,
        };
        let vs = union(&[tl, tr, bl, br]);
        let m = MonitorInfo::in_desktop(tl, tl, vs, &[tr, bl, br]);
        assert!(!m.neighbour_left && m.neighbour_right);
        assert!(!m.neighbour_up && m.neighbour_down);
        let m = MonitorInfo::in_desktop(tr, tr, vs, &[tl, bl, br]);
        assert!(m.neighbour_left && !m.neighbour_right);
        assert!(!m.neighbour_up && m.neighbour_down);
        let m = MonitorInfo::in_desktop(bl, bl, vs, &[tl, tr, br]);
        assert!(!m.neighbour_left && m.neighbour_right);
        assert!(m.neighbour_up && !m.neighbour_down);
        let m = MonitorInfo::in_desktop(br, br, vs, &[tl, tr, bl]);
        assert!(m.neighbour_left && !m.neighbour_right);
        assert!(m.neighbour_up && !m.neighbour_down);
    }

    #[test]
    fn stacked_pair_projection_parks_columns_like_a_lone_monitor() {
        // Regression: vertical neighbours must not change horizontal column
        // parking. The laptop under an external parks off-viewport columns
        // beyond the virtual screen's left/right edges, which for a stacked
        // pair whose x-range the laptop shares are x = 0 and x = 2560.
        let padding = test_padding();
        let laptop = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let external = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        let vs = union(&[laptop, external]);
        let m = MonitorInfo::in_desktop(laptop, laptop, vs, &[external]);
        let layout = VirtualLayout::with_columns(
            (1..=3)
                .map(|i| Column::with_row(960, Row::new(WindowId(i), 0)))
                .collect(),
            968, // col 1 fills the viewport; col 0 off-left, col 2 overhangs right
        );
        let actual = project(&layout, &m, &padding);
        assert_eq!(actual.entries[0].rect.x, park_left_x(&m, 960));
        assert_eq!(actual.entries[0].rect.x, vs.x - 960 - PARK_MARGIN);
        assert_eq!(actual.entries[1].rect.x, 0);
        // The right side is free, so the overhanging column is NOT parked.
        assert_eq!(actual.entries[2].rect.x, 964);
        // Every rect keeps the laptop's own y band: nothing moves vertically.
        for e in &actual.entries {
            assert!(e.rect.y >= laptop.y && e.rect.bottom() <= laptop.bottom());
        }
    }

    #[test]
    fn parked_columns_never_land_on_the_neighbour_monitor() {
        let padding = test_padding();
        // 5 columns of 460px on a 4-per-screen primary, scrolled so that
        // col 0 is off to the left and col 4 is off to the right.
        let layout = VirtualLayout::with_columns(
            (1..=5)
                .map(|i| Column::with_row(460, Row::new(WindowId(i), 0)))
                .collect(),
            464, // start of col 1's slot
        );
        let m = primary_with_left_neighbour();
        let actual = project(&layout, &m, &padding);
        let left_monitor = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        for e in &actual.entries {
            assert!(
                !e.rect.overlaps(left_monitor) || e.rect.overlaps(m.work_area),
                "window {:?} at x={} spills onto the left monitor",
                e.window_id,
                e.rect.x
            );
        }
        // col 0 is parked beyond the virtual screen's left edge, not the primary's
        assert_eq!(actual.entries[0].rect.x, -1920 - 460 - PARK_MARGIN);
        assert!(m.is_parked(actual.entries[0].rect));
        // col 4 (canvas 1860..2320, viewport 464..2384) is partially visible
        // on the free right edge, so it stays where it is (hangs off the edge)
        assert_eq!(actual.entries[4].rect.x, 1860 - 464);
        assert!(!m.is_parked(actual.entries[4].rect));
    }

    #[test]
    fn overhang_toward_a_neighbour_is_parked_but_free_edge_overhang_is_kept() {
        let padding = test_padding();
        // Two 960px columns, viewport offset 500: col 0 (canvas 4..964) is
        // partially visible, hanging off the LEFT edge.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            500,
        );
        // Primary: left neighbour → the overhanging column is parked left.
        let m = primary_with_left_neighbour();
        let actual = project(&layout, &m, &padding);
        assert_eq!(actual.entries[0].rect.x, park_left_x(&m, 960));
        assert!(m.is_parked(actual.entries[0].rect));
        // Secondary: left edge is free → same column keeps its overhang.
        let m2 = secondary_with_right_neighbour();
        let actual2 = project(&layout, &m2, &padding);
        assert_eq!(actual2.entries[0].rect.x, -1920 + (4 - 500));
        assert!(!m2.is_parked(actual2.entries[0].rect));
    }

    #[test]
    fn monocle_width_column_at_the_viewport_edge_stays_on_screen() {
        let padding = test_padding();
        // A monocle column (monitor width − 2·gap) at the viewport's left
        // edge fills the work area without overhanging either side.
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1912, Row::new(WindowId(1), 0))], 0);
        let m = primary_with_left_neighbour();
        let actual = project(&layout, &m, &padding);
        assert!(!m.is_parked(actual.entries[0].rect));
        assert_eq!(actual.entries[0].rect.x, 4);
        assert_eq!(actual.entries[0].rect.right(), 1916);
    }

    #[test]
    fn column_wider_than_the_work_area_is_parked_like_any_overhang() {
        let padding = test_padding();
        // A 1920-wide column (the width of the whole monitor) scrolled 10 px
        // overhangs the left edge, where DISPLAY2 sits: it is parked, not
        // exempt — on screen it would cover the neighbour.
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1920, Row::new(WindowId(1), 0))], 10);
        let m = primary_with_left_neighbour();
        let actual = project(&layout, &m, &padding);
        assert!(m.is_parked(actual.entries[0].rect));
        // With no neighbour on that side it keeps its overhang as before.
        let lone = MonitorInfo::single(m.work_area);
        let actual = project(&layout, &lone, &padding);
        assert!(!lone.is_parked(actual.entries[0].rect));
        assert_eq!(actual.entries[0].rect.x, 4 - 10);
    }

    #[test]
    fn secondary_monitor_parks_beyond_the_far_edge_of_the_desktop() {
        let padding = test_padding();
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let m = secondary_with_right_neighbour();
        let actual = project(&layout, &m, &padding);
        // col 2 is off the right of the secondary; the primary sits there,
        // so it is parked beyond the primary's far edge.
        assert_eq!(actual.entries[2].rect.x, 1920 + PARK_MARGIN);
        assert!(m.is_parked(actual.entries[2].rect));
        let primary = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert!(!actual.entries[2].rect.overlaps(primary));
    }

    /// The middle monitor of a three-wide desktop: neighbours on both sides.
    fn middle_of_three() -> MonitorInfo {
        let left = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let middle = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let right = Rect {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let virtual_screen = Rect {
            x: -1920,
            y: 0,
            width: 5760,
            height: 1080,
        };
        MonitorInfo::in_desktop(middle, middle, virtual_screen, &[left, right])
    }

    #[test]
    fn middle_monitor_parks_beyond_both_far_edges() {
        let m = middle_of_three();
        assert!(m.neighbour_left && m.neighbour_right);
        let padding = test_padding();
        // 4 × 960 columns, viewport at the start of col 1's slot (968):
        // col 0 is off-left, col 1 fills 0..960, col 2 (964..1924) overhangs
        // the right edge, col 3 is off-right.
        let layout = VirtualLayout::with_columns(
            (1..=4)
                .map(|i| Column::with_row(960, Row::new(WindowId(i), 0)))
                .collect(),
            968,
        );
        let actual = project(&layout, &m, &padding);
        assert_eq!(actual.entries[0].rect.x, -1920 - 960 - PARK_MARGIN);
        assert_eq!(actual.entries[1].rect.x, 0);
        // Overhang toward the right neighbour: parked beyond ITS far edge.
        assert_eq!(actual.entries[2].rect.x, 3840 + PARK_MARGIN);
        assert_eq!(actual.entries[3].rect.x, 3840 + PARK_MARGIN);
        let left = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let right = Rect {
            x: 1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        for e in &actual.entries {
            assert!(
                !e.rect.overlaps(left) && !e.rect.overlaps(right),
                "{:?} at x={} lands on a neighbour",
                e.window_id,
                e.rect.x
            );
        }

        // Overhang toward the left neighbour is parked left of everything.
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
            ],
            500,
        );
        let actual = project(&layout, &m, &padding);
        assert_eq!(actual.entries[0].rect.x, park_left_x(&m, 960));
        assert_eq!(actual.entries[0].rect.x, -1920 - 960 - PARK_MARGIN);
        assert_eq!(actual.entries[1].rect.x, 968 - 500);
    }

    #[test]
    fn negative_origin_monitor_projects_relative_to_its_own_corner() {
        // A monitor above-left of the primary: both x and y are negative.
        let m = MonitorInfo::single(Rect {
            x: -1920,
            y: -1080,
            width: 1920,
            height: 1080,
        });
        let padding = Padding {
            window_gap: 4,
            up: 10,
            down: 0,
        };
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), 0)),
                Column::with_row(960, Row::new(WindowId(2), 0)),
                Column::with_row(960, Row::new(WindowId(3), 0)),
            ],
            0,
        );
        let actual = project(&layout, &m, &padding);
        assert_eq!(actual.entries[0].rect.x, -1920 + 4);
        assert_eq!(actual.entries[0].rect.y, -1080 + 10 + 4);
        assert_eq!(actual.entries[1].rect.x, -1920 + 968);
        // Off-right: parked beyond the monitor's right edge, which is x=0.
        assert_eq!(actual.entries[2].rect.x, PARK_MARGIN);
        assert_eq!(actual.entries[2].rect.y, -1080 + 10 + 4, "parking keeps y");
        assert!(m.is_parked(actual.entries[2].rect));

        // Scrolled past col 0: parked beyond the monitor's left edge.
        let layout = VirtualLayout::with_columns(layout.columns.clone(), 968);
        let actual = project(&layout, &m, &padding);
        assert_eq!(actual.entries[0].rect.x, -1920 - 960 - PARK_MARGIN);
        assert_eq!(actual.entries[1].rect.x, -1920);
    }

    #[test]
    fn work_area_offset_from_screen_edge_drives_overhang_and_origin() {
        // Taskbar docked on the left: the work area starts 60 px into the
        // screen. A second monitor sits to the left.
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let work_area = Rect {
            x: 60,
            y: 0,
            width: 1860,
            height: 1080,
        };
        let left = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let virtual_screen = Rect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        let m = MonitorInfo::in_desktop(work_area, screen, virtual_screen, &[left]);
        assert!(m.neighbour_left && !m.neighbour_right);
        let padding = test_padding();
        let columns = vec![
            Column::with_row(960, Row::new(WindowId(1), 0)),
            Column::with_row(960, Row::new(WindowId(2), 0)),
            Column::with_row(960, Row::new(WindowId(3), 0)),
        ];

        // Viewport at 0: columns are placed from the WORK AREA's left edge.
        let layout = VirtualLayout::with_columns(columns.clone(), 0);
        let actual = project(&layout, &m, &padding);
        assert_eq!(actual.entries[0].rect.x, 60 + 4);
        // Col 1 (1028..1988) overhangs the free right edge: kept.
        assert_eq!(actual.entries[1].rect.x, 60 + 968);
        // Col 2 (canvas 1932) is past the viewport (0..1860): parked right of
        // the virtual screen.
        assert_eq!(actual.entries[2].rect.x, 1920 + PARK_MARGIN);

        // Viewport at 30: col 0 would sit at x=34 — on the screen, but under
        // the taskbar strip left of the work area, with a neighbour beyond.
        // Overhang is judged against the work area, so it is parked.
        let layout = VirtualLayout::with_columns(columns, 30);
        let actual = project(&layout, &m, &padding);
        assert_eq!(actual.entries[0].rect.x, park_left_x(&m, 960));
        assert_eq!(actual.entries[0].rect.x, -1920 - 960 - PARK_MARGIN);
        assert_eq!(actual.entries[1].rect.x, 60 + 968 - 30);
    }
}
