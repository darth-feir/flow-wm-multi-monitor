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
    ActualEntry, ActualLayout, Column, LayoutMode, MonitorInfo, Padding, PileSide, VirtualLayout,
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
///
/// # Row viewports
///
/// `visible_rows` is `[columns] visible_rows` (`0` = every row is shown).
/// A column with more rows than that shows the `visible_rows` rows from
/// [`Column::row_offset`] on, stacked exactly as a column of that many rows;
/// the rows before the offset are parked **above** the monitor and the rows
/// after the window **below** it, at the column's own x (see
/// [`park_above_bottom`] / [`park_below_top`]). The rule mirrors workspace
/// parking: just beyond the monitor's physical screen rect on a free side,
/// beyond the whole virtual screen when another monitor is stacked on that
/// side, so a parked row never lands on a neighbour.
///
/// # Fullscreen columns (ADR-0021)
///
/// A [`fullscreen`](Column::fullscreen) column spans one whole screen on
/// the canvas ([`Column::canvas_width`]). It is shown only when it
/// **starts the view** â€” it is the first column overlapping the viewport â€”
/// and then at the monitor's full [`screen_rect`](MonitorInfo::screen_rect)
/// (taskbar covered, no gap), with every other column parked. Anywhere
/// else it is parked on its side at the screen rect's size, so moving it
/// in and out of view is always a pure move.
#[must_use]
pub fn project(
    virtual_layout: &VirtualLayout,
    monitor: &MonitorInfo,
    padding: &Padding,
    visible_rows: u32,
) -> ActualLayout {
    let places = column_places(virtual_layout, monitor, padding);
    let mut entries = Vec::new();
    for (column, place) in virtual_layout.columns.iter().zip(&places) {
        let col_width = place_width(column, monitor);
        let x = match *place {
            Place::Shown(x) => x,
            Place::Left => park_left_x(monitor, col_width),
            Place::Right => park_right_x(monitor),
        };
        emit_column(
            column,
            x,
            col_width,
            None,
            monitor,
            padding,
            visible_rows,
            &mut entries,
        );
    }
    ActualLayout { entries }
}

/// [`project`] for a strip in `mode`: strip mode is [`project`] itself;
/// book mode keeps every visible column where [`project`] puts it and
/// piles the columns out of view behind the edge columns instead of
/// parking them (ADR-0022, `docs/src/dev-guide/layout/projection.md`,
/// *Book mode*):
///
/// - a column [`project`] parks **left** takes the rect of the leftmost
///   visible column (its x and width; its own rows stacked there) and is
///   marked [`PileSide::Left`]; one parked **right** takes the rightmost
///   visible column's rect, [`PileSide::Right`]. With one visible column
///   both piles share its rect;
/// - a fullscreen column keeps off-screen parking, so its moves stay pure
///   moves (ADR-0019 / ADR-0021). While one **starts the view** there is
///   no edge column: the others pile under it at their own width, flush
///   with the work area's left edge (strip index before it) or right edge
///   (after it);
/// - a column less than half on screen counts as out of view (a pattern's
///   rounding can leave the next column a one-pixel sliver in the view);
/// - with no visible column at all (everything overhangs on a narrow
///   monitor) the strip parking is kept.
#[must_use]
pub fn project_with(
    virtual_layout: &VirtualLayout,
    monitor: &MonitorInfo,
    padding: &Padding,
    visible_rows: u32,
    mode: LayoutMode,
) -> ActualLayout {
    if mode == LayoutMode::Strip {
        return project(virtual_layout, monitor, padding, visible_rows);
    }
    let wa = monitor.work_area;
    // A column less than half on screen is a card in the pile on its side,
    // not an edge column: a pattern's rounding can leave the next column a
    // one-pixel sliver inside the view, and the pile must not sit there.
    let places: Vec<Place> = column_places(virtual_layout, monitor, padding)
        .into_iter()
        .zip(&virtual_layout.columns)
        .map(|(p, c)| match p {
            Place::Shown(x) if !c.fullscreen => {
                let on_screen = (x + c.width_px).min(wa.right()) - x.max(wa.x);
                if 2 * on_screen >= c.width_px {
                    p
                } else if x < wa.x {
                    Place::Left
                } else {
                    Place::Right
                }
            }
            other => other,
        })
        .collect();
    let cols = &virtual_layout.columns;
    let gap = padding.window_gap;
    // The fullscreen column that starts the view, if any.
    let fullscreen_shown = cols
        .iter()
        .zip(&places)
        .position(|(c, p)| c.fullscreen && matches!(p, Place::Shown(_)));
    // The slot rects (x, width) of the visible columns, left to right.
    let shown: Vec<(i32, i32)> = cols
        .iter()
        .zip(&places)
        .filter_map(|(c, p)| match *p {
            Place::Shown(x) if !c.fullscreen => Some((x, c.width_px)),
            _ => None,
        })
        .collect();
    if fullscreen_shown.is_none() && shown.is_empty() {
        return project(virtual_layout, monitor, padding, visible_rows);
    }
    let mut entries = Vec::new();
    for (i, (column, place)) in cols.iter().zip(&places).enumerate() {
        let side = match place {
            Place::Shown(_) => None,
            Place::Left => Some(PileSide::Left),
            Place::Right => Some(PileSide::Right),
        };
        let (x, width, pile) = match (side, column.fullscreen, fullscreen_shown) {
            // Visible columns, and fullscreen columns anywhere: as in strip
            // mode.
            (None, _, _) | (Some(_), true, _) => {
                let w = place_width(column, monitor);
                let x = match *place {
                    Place::Shown(x) => x,
                    Place::Left => park_left_x(monitor, w),
                    Place::Right => park_right_x(monitor),
                };
                (x, w, None)
            }
            // Under a fullscreen column in view: own width, flush with the
            // side of the work area the column lies on.
            (Some(_), false, Some(f)) => {
                let w = column.width_px;
                if i < f {
                    (wa.x + gap, w, Some(PileSide::Left))
                } else {
                    (wa.right() - gap - w, w, Some(PileSide::Right))
                }
            }
            (Some(side), false, None) => {
                let (x, w) = match side {
                    PileSide::Left => shown[0],
                    PileSide::Right => shown[shown.len() - 1],
                };
                (x, w, Some(side))
            }
        };
        emit_column(
            column,
            x,
            width,
            pile,
            monitor,
            padding,
            visible_rows,
            &mut entries,
        );
    }
    ActualLayout { entries }
}

/// Where [`project`] puts a column horizontally: on screen at `x`, or
/// parked on one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// On screen with its left edge at this x.
    Shown(i32),
    /// Parked left of the virtual screen.
    Left,
    /// Parked right of the virtual screen.
    Right,
}

/// The width a column is emitted at: one whole screen while fullscreen,
/// its `width_px` otherwise.
fn place_width(column: &Column, monitor: &MonitorInfo) -> i32 {
    if column.fullscreen {
        monitor.screen_rect.width
    } else {
        column.width_px
    }
}

/// Emit `column`'s entries with its left edge at `x` and `width`: a
/// fullscreen column at the screen rect's height and y, every other one
/// with its rows stacked ([`stack_column_rows`]). Every entry carries
/// `pile`.
#[allow(clippy::too_many_arguments)]
fn emit_column(
    column: &Column,
    x: i32,
    width: i32,
    pile: Option<PileSide>,
    monitor: &MonitorInfo,
    padding: &Padding,
    visible_rows: u32,
    entries: &mut Vec<ActualEntry>,
) {
    if column.fullscreen {
        let screen = monitor.screen_rect;
        for row in &column.rows {
            entries.push(ActualEntry {
                window_id: row.window_id,
                rect: Rect { x, ..screen },
                pile,
            });
        }
        return;
    }
    let start = entries.len();
    stack_column_rows(column, x, monitor, width, padding, visible_rows, entries);
    for e in &mut entries[start..] {
        e.pile = pile;
    }
}

/// The camera shift, the visibility test and the overhang rule of
/// [`project`], per column, without emitting any rect: the one place both
/// layout modes take visibility from.
fn column_places(
    virtual_layout: &VirtualLayout,
    monitor: &MonitorInfo,
    padding: &Padding,
) -> Vec<Place> {
    let monitor_rect = monitor.work_area;
    let viewport_left = virtual_layout.viewport_offset;
    let viewport_right = viewport_left + monitor_rect.width;
    let slot_gap = padding.window_gap;

    let mut places = Vec::with_capacity(virtual_layout.columns.len());
    let mut canvas_x: i32 = slot_gap;
    let screen = monitor.screen_rect;
    // A fullscreen column that starts the view hides every other column.
    let fullscreen_in_view = virtual_layout
        .columns
        .iter()
        .scan(slot_gap, |x, c| {
            let left = *x;
            let right = left + c.canvas_width(monitor_rect.width, slot_gap);
            *x = right + slot_gap;
            Some((c.fullscreen, left, right))
        })
        .find(|&(_, left, right)| right > viewport_left && left < viewport_right)
        .is_some_and(|(fs, _, _)| fs);
    let mut first_visible_seen = false;

    for column in &virtual_layout.columns {
        let col_width = column.width_px;
        let canvas_col_left = canvas_x;
        let canvas_col_right = canvas_x + column.canvas_width(monitor_rect.width, slot_gap);
        canvas_x = canvas_col_right + slot_gap;

        let visible = canvas_col_right > viewport_left && canvas_col_left < viewport_right;
        let first = visible && !first_visible_seen;
        first_visible_seen |= visible;

        let place = if column.fullscreen {
            if first {
                Place::Shown(screen.x)
            } else if canvas_col_right <= viewport_left {
                Place::Left
            } else {
                Place::Right
            }
        } else if visible && fullscreen_in_view {
            // A fullscreen column starts the view: this one is behind it.
            Place::Right
        } else if visible {
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
                Place::Left
            } else if overhangs_right && monitor.neighbour_right {
                Place::Right
            } else {
                Place::Shown(on_screen_x)
            }
        } else if canvas_col_right <= viewport_left {
            Place::Left
        } else {
            Place::Right
        };
        places.push(place);
    }
    places
}

/// Canvas x of column `idx`'s left edge: the leading gap plus every earlier
/// column's slot (`canvas_width + gap`, a fullscreen column taking one
/// whole `monitor_width`). The one prefix sum every reader of the strip's
/// geometry shares with [`project`].
#[must_use]
pub fn column_left(layout: &VirtualLayout, idx: usize, gap: i32, monitor_width: i32) -> i32 {
    gap + layout.columns[..idx.min(layout.columns.len())]
        .iter()
        .map(|c| c.canvas_width(monitor_width, gap) + gap)
        .sum::<i32>()
}

/// Canvas x of column `idx`'s right edge ([`column_left`] plus its
/// [`canvas_width`](Column::canvas_width)).
#[must_use]
pub fn column_right(layout: &VirtualLayout, idx: usize, gap: i32, monitor_width: i32) -> i32 {
    column_left(layout, idx, gap, monitor_width)
        + layout.columns[idx].canvas_width(monitor_width, gap)
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

/// Bottom edge of the slot rows parked **above** the column viewport sit
/// against: `PARK_MARGIN` above the virtual screen when a monitor is
/// stacked above this one (`neighbour_up`), else `PARK_MARGIN` above the
/// monitor's own screen rect. Every row parked above shares this edge (the
/// one-slot-per-side model of workspace parking); a row's y is this minus
/// its height.
#[must_use]
pub fn park_above_bottom(monitor: &MonitorInfo) -> i32 {
    let edge = if monitor.neighbour_up {
        monitor.virtual_screen.y
    } else {
        monitor.screen_rect.y
    };
    edge - PARK_MARGIN
}

/// Top edge of the slot rows parked **below** the column viewport sit
/// against: `PARK_MARGIN` below the virtual screen when a monitor is
/// stacked below this one (`neighbour_down`), else `PARK_MARGIN` below
/// the monitor's own screen rect.
#[must_use]
pub fn park_below_top(monitor: &MonitorInfo) -> i32 {
    let edge = if monitor.neighbour_down {
        monitor.virtual_screen.bottom()
    } else {
        monitor.screen_rect.bottom()
    };
    edge + PARK_MARGIN
}

fn stack_column_rows(
    column: &Column,
    x: i32,
    monitor: &MonitorInfo,
    col_width: i32,
    padding: &Padding,
    visible_rows: u32,
    entries: &mut Vec<ActualEntry>,
) {
    if column.rows.is_empty() {
        return;
    }
    let monitor_rect = monitor.work_area;
    let slots = column.slots(visible_rows);
    // A stale offset (rows removed without a clamp) must not hide every
    // row: project as if it had been clamped.
    let first = column.row_offset.min(column.max_row_offset(visible_rows));
    let last = first + slots; // exclusive

    // First row starts one gap below the top padding edge; each subsequent row
    // starts one gap below the previous row's bottom.
    let mut y = monitor_rect.y + padding.up + padding.window_gap;
    let above_bottom = park_above_bottom(monitor);
    let below_top = park_below_top(monitor);

    for (i, row) in column.rows.iter().enumerate() {
        let row_y = if i < first {
            above_bottom - row.height
        } else if i >= last {
            below_top
        } else {
            let at = y;
            y += row.height + padding.window_gap;
            at
        };
        entries.push(ActualEntry::new(
            row.window_id,
            Rect {
                x,
                y: row_y,
                width: col_width,
                height: row.height,
            },
        ));
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
pub fn canvas_width(layout: &VirtualLayout, window_gap: i32, monitor_width: i32) -> i32 {
    if layout.columns.is_empty() {
        return 0;
    }
    let total_slots: i32 = layout
        .columns
        .iter()
        .map(|c| c.canvas_width(monitor_width, window_gap) + window_gap)
        .sum();
    // Leading edge gap is the window_gap at the very start of the canvas.
    // But each slot already includes its trailing gap, and the first slot starts
    // at window_gap. So the total span from canvas_x=0 to the end of the last
    // slot's gap is: window_gap (initial) + sum(col_width_i + window_gap).
    // The trailing gap of the last column is the right-edge gap.
    window_gap + total_slots
}

/// Compute the pixel width of a single step (one slot = the column's
/// canvas width + window gap; a fullscreen column's step is one screen).
/// Used by scroll operations to determine viewport offset changes.
#[must_use]
pub fn column_step_width(column: &Column, window_gap: i32, monitor_width: i32) -> i32 {
    column.canvas_width(monitor_width, window_gap) + window_gap
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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

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
        let actual = project(&layout, &monitor, &padding, 0);

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
        let actual = project(&layout, &monitor, &padding, 0);

        // Third column parked just beyond the virtual screen's right edge
        assert_eq!(actual.entries[2].rect.x, 1920 + PARK_MARGIN);
    }

    #[test]
    fn canvas_width_empty() {
        let layout = VirtualLayout::new();
        assert_eq!(canvas_width(&layout, 4, 1920), 0);
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
        assert_eq!(canvas_width(&layout, 4, 1920), 1932);
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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);
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
        let actual = project(&layout, &test_monitor(), &zero_padding, 0);
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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

        assert_eq!(actual.entries.len(), 1);
        // Slot model: full col_width, no horizontal inset
        assert_eq!(actual.entries[0].rect.width, 240);
    }

    #[test]
    fn project_empty_layout_yields_empty_actual() {
        // Positive: empty virtual layout → no entries
        let layout = VirtualLayout::new();
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);
        assert!(actual.entries.is_empty());
    }

    #[test]
    fn canvas_width_single_column() {
        // Positive: single column → window_gap + col_width + window_gap
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(1920, Row::new(WindowId(1), 0))], 0);
        // 4 + (1920 + 4) = 1928
        assert_eq!(canvas_width(&layout, 4, 1920), 1928);
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
        assert_eq!(canvas_width(&layout, 4, 1920), 4824);
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
        let actual = project(&layout, &test_monitor(), &padding, 0);

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
        let actual = project(&layout, &test_monitor(), &test_padding(), 0);

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
        let actual = project(&layout, &m, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
        assert_eq!(actual.entries[0].rect.x, park_left_x(&m, 960));
        assert!(m.is_parked(actual.entries[0].rect));
        // Secondary: left edge is free → same column keeps its overhang.
        let m2 = secondary_with_right_neighbour();
        let actual2 = project(&layout, &m2, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
        assert!(m.is_parked(actual.entries[0].rect));
        // With no neighbour on that side it keeps its overhang as before.
        let lone = MonitorInfo::single(m.work_area);
        let actual = project(&layout, &lone, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
        assert_eq!(actual.entries[0].rect.x, -1920 + 4);
        assert_eq!(actual.entries[0].rect.y, -1080 + 10 + 4);
        assert_eq!(actual.entries[1].rect.x, -1920 + 968);
        // Off-right: parked beyond the monitor's right edge, which is x=0.
        assert_eq!(actual.entries[2].rect.x, PARK_MARGIN);
        assert_eq!(actual.entries[2].rect.y, -1080 + 10 + 4, "parking keeps y");
        assert!(m.is_parked(actual.entries[2].rect));

        // Scrolled past col 0: parked beyond the monitor's left edge.
        let layout = VirtualLayout::with_columns(layout.columns.clone(), 968);
        let actual = project(&layout, &m, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
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
        let actual = project(&layout, &m, &padding, 0);
        assert_eq!(actual.entries[0].rect.x, park_left_x(&m, 960));
        assert_eq!(actual.entries[0].rect.x, -1920 - 960 - PARK_MARGIN);
        assert_eq!(actual.entries[1].rect.x, 60 + 968 - 30);
    }

    // ── Row viewports (stacked columns) ───────────────────────────────

    /// Four rows sized for a two-row viewport (any two fill the budget).
    fn stacked_column(width: i32, ids: &[isize], visible: usize, row_offset: usize) -> Column {
        let per_slot = crate::layout::mutations::distribute_heights(visible, 1080, 4);
        let rows: Vec<Row> = ids
            .iter()
            .enumerate()
            .map(|(i, &id)| Row::new(WindowId(id), per_slot[i % visible]))
            .collect();
        Column::with_rows(width, rows).with_row_offset(row_offset)
    }

    fn rect_of(actual: &ActualLayout, id: isize) -> Rect {
        actual.find(WindowId(id)).expect("window projected").rect
    }

    /// `visible_rows = 2`: the two rows from the offset are stacked exactly
    /// like a two-row column; the rows after them are parked below the
    /// monitor at the column's x, the rows before it above.
    #[test]
    fn row_viewport_stacks_visible_rows_and_parks_the_rest() {
        let m = test_monitor();
        let layout = VirtualLayout::with_columns(vec![stacked_column(960, &[1, 2, 3, 4], 2, 0)], 0);
        let actual = project(&layout, &m, &test_padding(), 2);
        let h = (1080 - 3 * 4) / 2;
        assert_eq!(
            rect_of(&actual, 1),
            Rect {
                x: 4,
                y: 4,
                width: 960,
                height: h
            }
        );
        assert_eq!(
            rect_of(&actual, 2),
            Rect {
                x: 4,
                y: 4 + h + 4,
                width: 960,
                height: h
            }
        );
        // Rows 3 and 4 share the below slot: top edge PARK_MARGIN under the
        // screen, same x as the column.
        for id in [3, 4] {
            let r = rect_of(&actual, id);
            assert_eq!(r.y, 1080 + PARK_MARGIN, "row {id} parked below");
            assert_eq!(r.x, 4);
            assert!(m.is_parked(r));
        }

        // Offset 1: row 1 parks above (bottom edge PARK_MARGIN over the
        // screen), rows 2 and 3 are on screen, row 4 stays below.
        let layout = VirtualLayout::with_columns(vec![stacked_column(960, &[1, 2, 3, 4], 2, 1)], 0);
        let actual = project(&layout, &m, &test_padding(), 2);
        let above = rect_of(&actual, 1);
        assert_eq!(above.bottom(), -PARK_MARGIN);
        assert!(m.is_parked(above));
        assert_eq!(rect_of(&actual, 2).y, 4);
        assert_eq!(rect_of(&actual, 3).y, 4 + h + 4);
        assert_eq!(rect_of(&actual, 4).y, 1080 + PARK_MARGIN);
    }

    /// With `visible_rows = 0`, or when the column has no more rows than
    /// the viewport shows, projection is unchanged from the flat model.
    #[test]
    fn row_viewport_is_inert_when_every_row_fits() {
        let m = test_monitor();
        let layout = VirtualLayout::with_columns(vec![stacked_column(960, &[1, 2], 2, 0)], 0);
        let flat = project(&layout, &m, &test_padding(), 0);
        let capped = project(&layout, &m, &test_padding(), 2);
        let roomy = project(&layout, &m, &test_padding(), 5);
        assert_eq!(flat, capped);
        assert_eq!(flat, roomy);
        assert!(!m.is_parked(rect_of(&flat, 1)));
        assert!(!m.is_parked(rect_of(&flat, 2)));
    }

    /// A stale offset past the end (rows removed without a clamp) projects
    /// as if clamped — the column never hides every row.
    #[test]
    fn stale_row_offset_projects_clamped() {
        let m = test_monitor();
        let layout = VirtualLayout::with_columns(vec![stacked_column(960, &[1, 2, 3], 2, 7)], 0);
        let actual = project(&layout, &m, &test_padding(), 2);
        assert!(m.is_parked(rect_of(&actual, 1)));
        assert!(!m.is_parked(rect_of(&actual, 2)));
        assert!(!m.is_parked(rect_of(&actual, 3)));
    }

    /// Parked rows keep their vertical slot inside a column that is itself
    /// parked left/right: the rect is off-screen on both axes and its x is
    /// the column's parking x.
    #[test]
    fn parked_rows_follow_a_parked_column() {
        let m = test_monitor();
        let layout = VirtualLayout::with_columns(
            vec![
                stacked_column(960, &[1, 2, 3], 2, 0),
                Column::with_row(960, Row::new(WindowId(9), 1072)),
            ],
            968,
        );
        let actual = project(&layout, &m, &test_padding(), 2);
        let r = rect_of(&actual, 3);
        assert_eq!(r.x, -960 - PARK_MARGIN);
        assert_eq!(r.y, 1080 + PARK_MARGIN);
    }

    /// A laptop panel at (0,0) under a 1440p display: above is blocked, so a
    /// row parked above goes beyond the whole virtual screen (over the
    /// external), while below is free and parks just under the laptop.
    fn laptop_under_external() -> (MonitorInfo, Rect, Rect) {
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
        let vs = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 2520,
        };
        (
            MonitorInfo::in_desktop(laptop, laptop, vs, &[external]),
            laptop,
            external,
        )
    }

    #[test]
    fn parked_rows_go_beyond_the_virtual_screen_toward_a_stacked_neighbour() {
        let (m, laptop, external) = laptop_under_external();
        assert!(m.neighbour_up && !m.neighbour_down);
        let layout = VirtualLayout::with_columns(vec![stacked_column(960, &[1, 2, 3, 4], 2, 1)], 0);
        let actual = project(&layout, &m, &test_padding(), 2);
        let above = rect_of(&actual, 1);
        assert_eq!(above.bottom(), external.y - PARK_MARGIN);
        assert!(
            !above.overlaps(external),
            "parked row must not land on the external"
        );
        assert!(!above.overlaps(laptop));
        assert!(m.is_parked(above));
        let below = rect_of(&actual, 4);
        assert_eq!(below.y, laptop.bottom() + PARK_MARGIN);
        assert!(!below.overlaps(external));
        assert!(!below.overlaps(laptop));
    }

    /// The external display of the same pair: below is blocked (the laptop
    /// is there), above is free.
    #[test]
    fn parked_rows_on_the_upper_monitor_skip_the_laptop_below() {
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
        let vs = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 2520,
        };
        let m = MonitorInfo::in_desktop(external, external, vs, &[laptop]);
        assert!(m.neighbour_down && !m.neighbour_up);
        let layout = VirtualLayout::with_columns(vec![stacked_column(960, &[1, 2, 3, 4], 2, 1)], 0);
        let actual = project(&layout, &m, &test_padding(), 2);
        let above = rect_of(&actual, 1);
        assert_eq!(above.bottom(), external.y - PARK_MARGIN);
        let below = rect_of(&actual, 4);
        assert_eq!(below.y, laptop.bottom() + PARK_MARGIN);
        for id in [1, 4] {
            let r = rect_of(&actual, id);
            assert!(
                !r.overlaps(laptop) && !r.overlaps(external),
                "row {id} straddles a monitor"
            );
        }
    }

    /// A monitor with a taskbar: the work area is shorter than the screen,
    /// and a row parked below on the free side clears the taskbar strip
    /// (it sits under the physical screen rect, not the work area).
    #[test]
    fn parked_rows_clear_the_taskbar_strip() {
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let work = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1032,
        };
        let m = MonitorInfo::in_desktop(work, screen, screen, &[]);
        let layout = VirtualLayout::with_columns(vec![stacked_column(960, &[1, 2, 3], 2, 0)], 0);
        let actual = project(&layout, &m, &test_padding(), 2);
        let below = rect_of(&actual, 3);
        assert_eq!(below.y, screen.bottom() + PARK_MARGIN);
        assert!(m.is_parked(below));
    }

    // --- fullscreen columns (ADR-0021) ---

    /// A taskbar'd primary (work area 1920x1040) with a monitor to its left.
    fn taskbar_primary() -> MonitorInfo {
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let work = Rect {
            height: 1040,
            ..screen
        };
        let left = Rect { x: -1920, ..screen };
        let vs = Rect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        MonitorInfo::in_desktop(work, screen, vs, &[left])
    }

    /// [A 600] [F fs, 900 to return to] [B 600] on the taskbar'd primary.
    fn with_fullscreen(viewport_offset: i32) -> VirtualLayout {
        VirtualLayout::with_columns(
            vec![
                Column::with_row(600, Row::new(WindowId(1), 1032)),
                Column::with_row(900, Row::new(WindowId(2), 1032)).with_fullscreen(true),
                Column::with_row(600, Row::new(WindowId(3), 1032)),
            ],
            viewport_offset,
        )
    }

    #[test]
    fn a_fullscreen_column_spans_one_screen_on_the_canvas() {
        let vl = with_fullscreen(0);
        let gap = 4;
        assert_eq!(vl.columns[1].canvas_width(1920, gap), 1916);
        assert_eq!(column_left(&vl, 1, gap, 1920), 4 + 600 + 4);
        assert_eq!(column_left(&vl, 2, gap, 1920), 608 + 1920);
        assert_eq!(column_right(&vl, 1, gap, 1920), 608 + 1916);
        assert_eq!(canvas_width(&vl, gap, 1920), 4 + 604 + 1920 + 604);
        assert_eq!(column_step_width(&vl.columns[1], gap, 1920), 1920);
        // The width it returns to is untouched.
        assert_eq!(vl.columns[1].width_px, 900);
    }

    #[test]
    fn a_fullscreen_column_that_starts_the_view_covers_the_screen_and_parks_the_rest() {
        let m = taskbar_primary();
        let vl = with_fullscreen(608 - 4);
        let actual = project(&vl, &m, &test_padding(), 0);
        // The SCREEN rect, taskbar covered, no gap.
        assert_eq!(rect_of(&actual, 2), m.screen_rect);
        assert!(m.is_parked(rect_of(&actual, 1)));
        assert!(m.is_parked(rect_of(&actual, 3)));
        assert!(rect_of(&actual, 1).right() <= m.virtual_screen.x);
        assert_eq!(rect_of(&actual, 3).x, park_right_x(&m));
    }

    #[test]
    fn a_fullscreen_column_off_the_start_of_the_view_is_parked_at_the_screen_size() {
        let m = taskbar_primary();
        // The view starts at A: F overlaps it but does not start it.
        let actual = project(&with_fullscreen(0), &m, &test_padding(), 0);
        let a = rect_of(&actual, 1);
        assert_eq!((a.x, a.width), (4, 600));
        let f = rect_of(&actual, 2);
        assert_eq!(f.x, park_right_x(&m));
        assert_eq!((f.y, f.width, f.height), (0, 1920, 1080));
        // The view starts at B: F is parked left, again at the screen size,
        // beyond the neighbour.
        let vl = with_fullscreen(column_left(&with_fullscreen(0), 2, 4, 1920) - 4);
        let actual = project(&vl, &m, &test_padding(), 0);
        assert_eq!(rect_of(&actual, 3).x, 4);
        let f = rect_of(&actual, 2);
        assert_eq!(
            f,
            Rect {
                x: park_left_x(&m, 1920),
                ..m.screen_rect
            }
        );
        assert!(f.right() <= m.virtual_screen.x);
    }

    // --- Book mode (ADR-0022) -------------------------------------------

    /// `n` single-row columns of `width`, ids 1..=n, camera at `offset`.
    fn book_strip(widths: &[i32], offset: i32) -> VirtualLayout {
        VirtualLayout::with_columns(
            widths
                .iter()
                .enumerate()
                .map(|(i, &w)| Column::with_row(w, Row::new(WindowId(i as isize + 1), 1072)))
                .collect(),
            offset,
        )
    }

    fn book(vl: &VirtualLayout, m: &MonitorInfo) -> ActualLayout {
        project_with(vl, m, &test_padding(), 0, LayoutMode::Book)
    }

    fn pile_of(actual: &ActualLayout, id: isize) -> Option<PileSide> {
        actual.find(WindowId(id)).expect("window projected").pile
    }

    #[test]
    fn strip_mode_is_project_itself() {
        let m = taskbar_primary();
        for vl in [
            book_strip(&[958, 958, 958, 958], 962),
            with_fullscreen(0),
            with_fullscreen(column_left(&with_fullscreen(0), 1, 4, 1920) - 4),
        ] {
            let strip = project_with(&vl, &m, &test_padding(), 0, LayoutMode::Strip);
            assert_eq!(strip, project(&vl, &m, &test_padding(), 0));
            assert!(strip.entries.iter().all(|e| e.pile.is_none()));
        }
    }

    #[test]
    fn half_width_book_piles_each_side_under_its_edge_column() {
        // 1/2 on 1920: two 954 px columns per screen; the view on 3 and 4.
        let m = test_monitor();
        let w = (1920 - 3 * 4) / 2;
        let vl = book_strip(&[w, w, w, w, w, w], 2 * (w + 4));
        let actual = book(&vl, &m);
        let strip = project(&vl, &m, &test_padding(), 0);
        // Visible columns are exactly where strip mode puts them.
        for id in [3, 4] {
            assert_eq!(rect_of(&actual, id), rect_of(&strip, id));
            assert_eq!(pile_of(&actual, id), None);
        }
        for id in [1, 2] {
            assert_eq!(
                rect_of(&actual, id),
                rect_of(&actual, 3),
                "left pile at 3's slot"
            );
            assert_eq!(pile_of(&actual, id), Some(PileSide::Left));
        }
        for id in [5, 6] {
            assert_eq!(
                rect_of(&actual, id),
                rect_of(&actual, 4),
                "right pile at 4's slot"
            );
            assert_eq!(pile_of(&actual, id), Some(PileSide::Right));
        }
        // Nothing is off-screen.
        assert!(actual.entries.iter().all(|e| !m.is_parked(e.rect)));
    }

    #[test]
    fn a_pile_takes_the_top_slot_width_not_its_own() {
        // 2/3 + 1/3 pinned: member widths alternate, the pile rect does not.
        let m = test_monitor();
        let big = 2 * (1920 - 4) / 3 - 4;
        let small = (1920 - 4) / 3 - 4;
        let vl = book_strip(&[big, small, big, small, big], 0);
        let actual = book(&vl, &m);
        assert_eq!(rect_of(&actual, 1).width, big);
        assert_eq!(rect_of(&actual, 2).width, small);
        for id in [3, 4, 5] {
            assert_eq!(rect_of(&actual, id), rect_of(&actual, 2));
            assert_eq!(pile_of(&actual, id), Some(PileSide::Right));
        }
    }

    #[test]
    fn one_visible_column_is_the_slot_of_both_piles() {
        let m = test_monitor();
        let w = 1920 - 8;
        let vl = book_strip(&[w, w, w], w + 4);
        let actual = book(&vl, &m);
        let top = rect_of(&actual, 2);
        assert_eq!(top.x, 4);
        assert_eq!(rect_of(&actual, 1), top);
        assert_eq!(rect_of(&actual, 3), top);
        assert_eq!(pile_of(&actual, 1), Some(PileSide::Left));
        assert_eq!(pile_of(&actual, 3), Some(PileSide::Right));
    }

    #[test]
    fn a_pile_member_keeps_its_own_rows_inside_the_slot() {
        // Member 3 has two rows: both at the top's x and width, stacked.
        let m = test_monitor();
        let w = (1920 - 3 * 4) / 2;
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_row(w, Row::new(WindowId(1), 1072)),
                Column::with_row(w, Row::new(WindowId(2), 1072)),
                stacked_column(w, &[3, 4], 2, 0),
            ],
            0,
        );
        let actual = book(&vl, &m);
        let top = rect_of(&actual, 2);
        let (r3, r4) = (rect_of(&actual, 3), rect_of(&actual, 4));
        assert_eq!(
            (r3.x, r3.width, r4.x, r4.width),
            (top.x, top.width, top.x, top.width)
        );
        assert_eq!(r3.y, top.y);
        assert_eq!(r4.y, r3.bottom() + 4);
        assert_eq!(pile_of(&actual, 4), Some(PileSide::Right));
    }

    #[test]
    fn a_fullscreen_column_keeps_off_screen_parking_in_book_mode() {
        let m = taskbar_primary();
        // The view starts at column 3 (after F): F is parked left at the
        // screen size, exactly as in strip mode; 1 piles under 3.
        let vl = with_fullscreen(column_left(&with_fullscreen(0), 2, 4, 1920) - 4);
        let actual = book(&vl, &m);
        let strip = project(&vl, &m, &test_padding(), 0);
        assert_eq!(rect_of(&actual, 2), rect_of(&strip, 2));
        assert_eq!(pile_of(&actual, 2), None);
        assert_eq!(rect_of(&actual, 1), rect_of(&actual, 3));
        assert_eq!(pile_of(&actual, 1), Some(PileSide::Left));
    }

    #[test]
    fn under_a_fullscreen_column_in_view_the_others_pile_at_their_own_width() {
        let m = taskbar_primary();
        let vl = with_fullscreen(column_left(&with_fullscreen(0), 1, 4, 1920) - 4);
        let actual = book(&vl, &m);
        assert_eq!(rect_of(&actual, 2), m.screen_rect);
        let (a, c) = (rect_of(&actual, 1), rect_of(&actual, 3));
        assert_eq!((a.x, a.width), (m.work_area.x + 4, 600));
        assert_eq!((c.right(), c.width), (m.work_area.right() - 4, 600));
        assert_eq!(pile_of(&actual, 1), Some(PileSide::Left));
        assert_eq!(pile_of(&actual, 3), Some(PileSide::Right));
    }

    #[test]
    fn with_no_visible_column_book_mode_falls_back_to_parking() {
        // A column wider than the work area overhangs a neighbour: parked in
        // strip mode, and with nothing on screen there is no slot to pile at.
        let m = taskbar_primary();
        let vl = book_strip(&[2400], 0);
        assert_eq!(book(&vl, &m), project(&vl, &m, &test_padding(), 0));
    }
}
