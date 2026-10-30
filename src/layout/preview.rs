//! Drop-zone preview: pure computation of a layout after a drag-and-drop move.
//!
//! Provides [`preview_move`] (move a window already in the layout to a new
//! drop zone), [`preview_insert`] (drop a window that comes from another
//! monitor's strip) and [`resolve_drop_zone`] (map the cursor position to a
//! [`DropZone`]). All are pure — they clone the virtual layout, apply their
//! operation, and project to actual coordinates without touching any live
//! state or Win32 APIs.
//!
//! (`docs/src/dev-guide/tile-drag.md`, `docs/adr/0018-drop-zones-by-window-halves.md`)

use std::collections::HashMap;

use crate::common::{Rect, WindowId};
#[cfg(test)]
use crate::layout::mutations::distribute_heights;
use crate::layout::mutations::{MutationConfig, column_heights, remove_window};
use crate::layout::projection;
use crate::layout::types::{AppliedLayout, Column, MonitorInfo, PileSide, Row, VirtualLayout};

/// Where a dragged window would land when dropped.
///
/// Produced by zone detection during a tile drag and consumed by
/// [`preview_move`] / [`preview_insert`] to compute the resulting layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    /// Insert the dragged window as a new row in column `col` at row index `row`.
    Row {
        /// Target column index in the original layout.
        col: usize,
        /// Target row position within that column.
        row: usize,
    },
    /// Insert the dragged window as a new single-row column at index `col`.
    Column {
        /// Target column index in the original layout.
        col: usize,
    },
    /// Book mode (ADR-0022): a drop on a pile's top card. The window is
    /// inserted as a new single-row column at index `col` (right after the
    /// left top, right before the right top) and the camera puts it at the
    /// `side` edge of the view, so it becomes that pile's new top and the
    /// old top goes into the pile right behind it. The insertion is
    /// [`preview_move`] / [`preview_insert`]'s; the camera is the strip's
    /// ([`ScrollingSpace::edge_layout`](crate::workspace::ScrollingSpace::edge_layout)).
    Card {
        /// Target column index in the original layout.
        col: usize,
        /// The pile whose top the window becomes.
        side: PileSide,
    },
    /// Scroll the viewport left by one column. NOT a layout mutation.
    ScrollLeft,
    /// Scroll the viewport right by one column. NOT a layout mutation.
    ScrollRight,
    /// Scroll column `col`'s row viewport up by one row. NOT a layout
    /// mutation; only offered while rows are parked above the viewport.
    ScrollUp {
        /// The column whose row viewport scrolls.
        col: usize,
    },
    /// Scroll column `col`'s row viewport down by one row. NOT a layout
    /// mutation; only offered while rows are parked below the viewport.
    ScrollDown {
        /// The column whose row viewport scrolls.
        col: usize,
    },
}

impl DropZone {
    /// Whether this zone places the window (`Row` / `Column`) rather than
    /// scrolling a viewport.
    #[must_use]
    pub fn is_placement(self) -> bool {
        matches!(
            self,
            DropZone::Row { .. } | DropZone::Column { .. } | DropZone::Card { .. }
        )
    }

    /// The pile a [`Card`](DropZone::Card) drop makes the window the top
    /// of; `None` for every other zone.
    #[must_use]
    pub fn card_side(self) -> Option<PileSide> {
        match self {
            DropZone::Card { side, .. } => Some(side),
            _ => None,
        }
    }
}

/// The knobs of the zone map: the `[drag]` table plus the strip's
/// `visible_rows`. See (`docs/src/dev-guide/tile-drag.md`, *Configuration*).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneParams {
    /// Minimum width of a screen-edge scroll band in pixels, and the whole
    /// width of a band on an edge shared with a neighbouring monitor.
    pub edge_scroll_px: i32,
    /// Screen-edge band as a fraction of the work-area width (outer edges)
    /// and of its height (the row-scroll bands of a stacked column).
    pub edge_scroll_ratio: f32,
    /// Fraction of a column's width on each side that inserts a new column.
    pub col_edge_ratio: f32,
    /// Pixel cap on that band; `0` = no cap.
    pub col_edge_max_px: i32,
    /// `[columns] visible_rows` of the strip (`0` = every row is shown).
    pub visible_rows: u32,
}

/// Computes the layout that would result from moving `dragged_id` to `zone`.
///
/// Clones the virtual layout, removes the dragged window, re-inserts it at
/// the target zone, projects to actual coordinates, and returns the result.
///
/// Returns `None` if:
/// - the zone is a scroll zone (handled by daemon scroll logic),
/// - the dragged window is not found in the layout,
/// - the target column/row is out of bounds after source removal,
/// - the move is a no-op (the layout would be identical after the move).
///
/// # Zone → mutation mapping
///
/// - `Column { col }` → new single-row column at index `col`
/// - `Row { col, row }` → new row at position `row` in column `col`
/// - scroll zones → `None` (not a layout mutation)
#[must_use]
pub fn preview_move(
    virtual_layout: &VirtualLayout,
    dragged_id: WindowId,
    zone: DropZone,
    config: &MutationConfig,
    monitor: &MonitorInfo,
) -> Option<AppliedLayout> {
    // Scroll zones are not layout mutations.
    if !zone.is_placement() {
        return None;
    }

    // The dragged window must exist in the layout.
    let (src_col, _src_row) = virtual_layout.find_window(dragged_id)?;
    let src_had_single_row = virtual_layout.columns[src_col].rows.len() == 1;
    // A window alone in its column dropped onto that same column (either
    // half) stays where it is: its column is the slot, not the next one.
    if src_had_single_row && matches!(zone, DropZone::Row { col, .. } if col == src_col) {
        return None;
    }

    // Preserve the original viewport offset.
    let original_offset = virtual_layout.viewport_offset;

    // Step 1: Remove the dragged window from its current position.
    let mut layout = remove_window(virtual_layout, dragged_id, config);

    // Step 2: Re-insert at the target zone.
    let zone = match zone {
        DropZone::Column { col } => DropZone::Column {
            col: adjust_index(col, src_col, src_had_single_row),
        },
        DropZone::Card { col, side } => DropZone::Card {
            col: adjust_index(col, src_col, src_had_single_row),
            side,
        },
        DropZone::Row { col, row } => DropZone::Row {
            col: adjust_index(col, src_col, src_had_single_row),
            row,
        },
        other => other,
    };
    insert_at_zone(&mut layout, zone, dragged_id, config)?;

    // Restore the viewport offset.
    layout.viewport_offset = original_offset;

    // Step 3: No-op check. A card drop that keeps the order still moves
    // the camera (the caller's), so it is not a no-op here.
    if layout == *virtual_layout && zone.card_side().is_none() {
        return None;
    }

    // Step 4: Project to actual screen coordinates.
    let actual = projection::project_with(
        &layout,
        monitor,
        &config.padding,
        config.visible_rows,
        config.layout_mode,
    );

    Some(AppliedLayout {
        virtual_layout: layout,
        actual_layout: actual,
    })
}

/// Computes the layout that would result from dropping `window`, which is
/// **not** in `virtual_layout` (it comes from another monitor's strip), at
/// `zone`: the cross-screen counterpart of [`preview_move`].
///
/// Returns `None` for a scroll zone, a window that already has a seat in
/// this layout, a `Row` zone whose column does not exist, and a `Row` zone
/// into a full column (`max_rows`), which refuses the drop like
/// [`preview_move`] does. The viewport offset is kept; the caller brings
/// the new column into view when it commits.
#[must_use]
pub fn preview_insert(
    virtual_layout: &VirtualLayout,
    window: WindowId,
    zone: DropZone,
    config: &MutationConfig,
    monitor: &MonitorInfo,
) -> Option<AppliedLayout> {
    if virtual_layout.find_window(window).is_some() {
        return None;
    }
    let mut layout = virtual_layout.clone();
    insert_at_zone(&mut layout, zone, window, config)?;
    let actual = projection::project_with(
        &layout,
        monitor,
        &config.padding,
        config.visible_rows,
        config.layout_mode,
    );
    Some(AppliedLayout {
        virtual_layout: layout,
        actual_layout: actual,
    })
}

/// Insert `window` at `zone` (indices already relative to `layout`).
/// `None` for a scroll zone, a missing column or a full column.
fn insert_at_zone(
    layout: &mut VirtualLayout,
    zone: DropZone,
    window: WindowId,
    config: &MutationConfig,
) -> Option<()> {
    match zone {
        DropZone::Column { col } | DropZone::Card { col, .. } => {
            let pos = col.min(layout.columns.len());
            let new_col = make_single_row_column(window, config.column_width as i32, config);
            layout.columns.insert(pos, new_col);
            Some(())
        }
        DropZone::Row { col, row } => {
            if col >= layout.columns.len() {
                return None;
            }
            // A full column (`max_rows`) refuses the drop: a drag has no
            // "past the column" to walk to, the user picks another zone.
            // So does a fullscreen column, which holds one window
            // (ADR-0021).
            if layout.columns[col].fullscreen || config.column_full(layout.columns[col].rows.len())
            {
                return None;
            }
            insert_row_at(layout, col, row, window, config);
            Some(())
        }
        DropZone::ScrollLeft
        | DropZone::ScrollRight
        | DropZone::ScrollUp { .. }
        | DropZone::ScrollDown { .. } => None,
    }
}

/// Build a single-row column with height distributed for one row.
///
/// Duplicates the private `single_row_column` logic from `mutations.rs`.
fn make_single_row_column(window: WindowId, width_px: i32, config: &MutationConfig) -> Column {
    let h = column_heights(1, config)[0];
    Column::with_row(width_px, Row::new(window, h))
}

/// Insert `window` as a new row at `row_idx` in the column at `col_idx`.
///
/// Redistributes all row heights equally for `n+1` rows
/// ([`column_heights`]) and scrolls the column's row viewport to the new
/// row. Clamps `row_idx` to the current column length (append at bottom if
/// beyond last row).
fn insert_row_at(
    layout: &mut VirtualLayout,
    col_idx: usize,
    row_idx: usize,
    window: WindowId,
    config: &MutationConfig,
) {
    let col = &mut layout.columns[col_idx];
    let pos = row_idx.min(col.rows.len());
    col.rows.insert(pos, Row::new(window, 0));
    let heights = column_heights(col.rows.len(), config);
    for (row, h) in col.rows.iter_mut().zip(heights) {
        row.height = h;
    }
    col.reveal_row(pos, config.visible_rows);
}

/// Map an index from the original layout to the post-removal layout.
///
/// When the source column is deleted (had one row), every column to its
/// right shifts left by one. This function subtracts one from `idx` when
/// the removed column was strictly before it.
fn adjust_index(idx: usize, src_col: usize, src_col_removed: bool) -> usize {
    if src_col_removed && src_col < idx {
        idx - 1
    } else {
        idx
    }
}

// ---------------------------------------------------------------------------
// resolve_drop_zone: cursor position to DropZone (pure function)
// ---------------------------------------------------------------------------

/// Compute the column-edge band width: `col_edge_ratio` of the column,
/// capped by `max_px` when that is positive, at least one pixel.
fn compute_col_edge_band(col_width: i32, ratio: f32, max_px: i32) -> i32 {
    let band = (col_width as f32 * ratio.clamp(0.0, 0.5)) as i32;
    let band = if max_px > 0 { band.min(max_px) } else { band };
    band.max(1)
}

/// Width of a scroll band along an extent of `extent` pixels: the larger of
/// `px` and `ratio · extent`, or just `px` on an edge shared with another
/// monitor (`neighbour`), so that crossing to the neighbour does not sweep
/// through a wide band.
fn scroll_band(params: &ZoneParams, extent: i32, neighbour: bool) -> i32 {
    let px = params.edge_scroll_px.max(0);
    if neighbour {
        px
    } else {
        px.max((extent as f32 * params.edge_scroll_ratio.clamp(0.0, 0.5)).round() as i32)
    }
}

/// Check whether content exists offscreen to the left of the viewport.
fn can_scroll_left(vl: &VirtualLayout) -> bool {
    vl.viewport_offset > 0
}

/// Check whether content exists offscreen to the right of the viewport.
///
/// Derives the inter-column gap from the first two visible columns when
/// available; otherwise uses a structural check (single visible column that
/// is not the last virtual column).
fn can_scroll_right(vl: &VirtualLayout, wa_width: i32, vis_cols: &[VisibleColumn]) -> bool {
    if vis_cols.len() >= 2 {
        // Visible columns form a contiguous virtual-index range, so the
        // first two are always adjacent on the canvas. Derive the gap from
        // their screen positions: gap = screen_x_1 - (screen_x_0 + col0_width).
        let col0_w = vl.columns[vis_cols[0].idx].width_px;
        let gap = (vis_cols[1].rect.x - (vis_cols[0].rect.x + col0_w)).max(0);
        let total_span = projection::canvas_width(vl, gap, wa_width);
        // canvas_width includes the trailing right-edge gap, which is not
        // scrollable content; compare the last column's content right edge.
        let content_right = total_span - gap;
        content_right > vl.viewport_offset + wa_width
    } else if let Some(first) = vis_cols.first() {
        // Single visible column: right scroll is possible iff more columns
        // exist to the right on the canvas.
        first.idx < vl.columns.len() - 1
    } else {
        false
    }
}

/// A column on screen: its virtual index, the bounding box of its visible
/// rows, and those rows (row index + rect), top to bottom.
struct VisibleColumn {
    idx: usize,
    rect: Rect,
    rows: Vec<(usize, Rect)>,
    /// A fullscreen column (never a pile top).
    fullscreen: bool,
    /// Rows are parked above the row viewport.
    parked_above: bool,
    /// Rows are parked below the row viewport.
    parked_below: bool,
}

/// Build the visible columns from the applied layout.
///
/// Only the rows inside each column's row viewport (`row_offset ..
/// row_offset + slots`) count: a row parked above or below the monitor is
/// not a drop target. Columns entirely outside `wa` are excluded, and so
/// are the members of a book-mode pile (on screen, but under their top:
/// the top is the target). Does NOT exclude the dragged window's own
/// column.
fn build_visible_columns(
    applied: &AppliedLayout,
    wa: &Rect,
    visible_rows: u32,
) -> Vec<VisibleColumn> {
    let vl = &applied.virtual_layout;
    let rects: HashMap<WindowId, Rect> = applied
        .actual_layout
        .entries
        .iter()
        .filter(|e| e.pile.is_none())
        .map(|e| (e.window_id, e.rect))
        .collect();

    let mon_left = wa.x;
    let mon_right = wa.x + wa.width;

    let mut result = Vec::new();
    for (ci, col) in vl.columns.iter().enumerate() {
        let slots = col.slots(visible_rows);
        let first = col.row_offset.min(col.max_row_offset(visible_rows));
        let rows: Vec<(usize, Rect)> = (first..first + slots)
            .filter_map(|ri| {
                let id = col.rows.get(ri)?.window_id;
                rects.get(&id).map(|r| (ri, *r))
            })
            .collect();
        let Some(&(_, top)) = rows.first() else {
            continue;
        };
        let (_, bottom) = rows[rows.len() - 1];
        if top.x + top.width <= mon_left || top.x >= mon_right {
            continue;
        }
        result.push(VisibleColumn {
            idx: ci,
            rect: Rect {
                x: top.x,
                y: top.y,
                width: top.width,
                height: bottom.y + bottom.height - top.y,
            },
            rows,
            fullscreen: col.fullscreen,
            parked_above: first > 0,
            parked_below: first + slots < col.rows.len(),
        });
    }
    result
}

/// Which pile's top card `vc` is in a book-mode layout, for a cursor at
/// `mx`: the first visible column tops the left pile and the last one the
/// right pile, each only while that pile has a member; a lone visible
/// column tops both and the half under the cursor picks one. `None` for a
/// middle column, a fullscreen one, an edge column whose pile is empty and
/// in strip mode (no pile at all).
fn card_side(
    applied: &AppliedLayout,
    vis_cols: &[VisibleColumn],
    vc: &VisibleColumn,
    mx: i32,
) -> Option<PileSide> {
    if vc.fullscreen {
        return None;
    }
    let has = |side: PileSide| {
        applied
            .actual_layout
            .entries
            .iter()
            .any(|e| e.pile == Some(side))
    };
    let first = vis_cols.first().map(|c| c.idx) == Some(vc.idx);
    let last = vis_cols.last().map(|c| c.idx) == Some(vc.idx);
    let left = first && has(PileSide::Left);
    let right = last && has(PileSide::Right);
    match (left, right) {
        (true, true) => Some(if mx < vc.rect.x + vc.rect.width / 2 {
            PileSide::Left
        } else {
            PileSide::Right
        }),
        (true, false) => Some(PileSide::Left),
        (false, true) => Some(PileSide::Right),
        (false, false) => None,
    }
}

/// Resolve the drop target for a tile drag from the cursor position.
///
/// Pure: reads only `applied`, `monitor`, and `params`; touches no live
/// state or Win32 APIs. Total over the work area: every cursor position
/// inside it maps to a zone, an empty strip included (`Column { col: 0 }`,
/// the drop target of a cross-screen drag onto an empty monitor). Returns
/// `None` for a cursor **outside the work area**: on a stacked pair the
/// display above or below shares this strip's x range, and beside it the
/// neighbour shares its y range; a cursor over there must hit neither a
/// scroll band nor a column zone of this strip.
///
/// Map, first match wins (`docs/src/dev-guide/tile-drag.md`, ADR-0018):
/// 1. screen-edge scroll bands (left/right, only when there is content
///    that way);
/// 2. gaps beside and between columns → new column there;
/// 3. the outer `col_edge_ratio` of a column on either side → new column
///    beside it;
/// 4. the top/bottom row-scroll bands of a column with rows parked that
///    way;
/// 5. the rest of the column: the half of the visible row under the
///    cursor → above it (top half) or below it (bottom half).
#[must_use]
pub fn resolve_drop_zone(
    applied: &AppliedLayout,
    monitor: &MonitorInfo,
    mx: i32,
    my: i32,
    params: &ZoneParams,
) -> Option<DropZone> {
    let wa = &monitor.work_area;
    let vl = &applied.virtual_layout;
    let wa_right = wa.x + wa.width;

    // Outside this strip's work area the cursor is on a neighbour (or
    // off-screen): no zone of this strip.
    if my < wa.y || my >= wa.y + wa.height || mx < wa.x || mx >= wa_right {
        return None;
    }

    let vis_cols = build_visible_columns(applied, wa, params.visible_rows);
    if vis_cols.is_empty() {
        // An empty strip takes the window as its first column; a strip
        // whose columns are all off screen appends it.
        return Some(DropZone::Column {
            col: vl.columns.len(),
        });
    }

    // 1. Screen-edge scroll bands.
    let left_band = scroll_band(params, wa.width, monitor.neighbour_left);
    let right_band = scroll_band(params, wa.width, monitor.neighbour_right);
    if mx <= wa.x + left_band && can_scroll_left(vl) {
        return Some(DropZone::ScrollLeft);
    }
    if mx >= wa_right - right_band && can_scroll_right(vl, wa.width, &vis_cols) {
        return Some(DropZone::ScrollRight);
    }

    // 2. Gaps: left of the first column, right of the last, between two.
    let first = &vis_cols[0];
    let last = &vis_cols[vis_cols.len() - 1];
    if mx < first.rect.x {
        return Some(DropZone::Column { col: first.idx });
    }
    let last_right = last.rect.x + last.rect.width;
    if mx >= last_right {
        return Some(DropZone::Column { col: last.idx + 1 });
    }
    for pair in vis_cols.windows(2) {
        let seam_left = pair[0].rect.x + pair[0].rect.width;
        if mx >= seam_left && mx < pair[1].rect.x {
            return Some(DropZone::Column { col: pair[1].idx });
        }
    }

    // The column body containing mx.
    let Some(vc) = vis_cols
        .iter()
        .find(|c| mx >= c.rect.x && mx < c.rect.x + c.rect.width)
    else {
        // Unreachable for valid inputs: every mx between the first column's
        // left and the last column's right is in a column or a seam.
        return Some(DropZone::Column { col: last.idx + 1 });
    };
    let col_right = vc.rect.x + vc.rect.width;

    // Book mode: the whole of a pile's top card is one zone, a new top
    // card for that pile (ADR-0022).
    if let Some(side) = card_side(applied, &vis_cols, vc, mx) {
        let col = match side {
            PileSide::Left => vc.idx + 1,
            PileSide::Right => vc.idx,
        };
        return Some(DropZone::Card { col, side });
    }

    // 3. Outer bands of the column → new column beside it.
    let band = compute_col_edge_band(vc.rect.width, params.col_edge_ratio, params.col_edge_max_px);
    if mx < vc.rect.x + band {
        return Some(DropZone::Column { col: vc.idx });
    }
    if mx >= col_right - band {
        return Some(DropZone::Column { col: vc.idx + 1 });
    }

    // 4. Row-scroll bands of a column with parked rows.
    let row_band = scroll_band(params, wa.height, false);
    if vc.parked_above && my < vc.rect.y + row_band {
        return Some(DropZone::ScrollUp { col: vc.idx });
    }
    if vc.parked_below && my >= vc.rect.y + vc.rect.height - row_band {
        return Some(DropZone::ScrollDown { col: vc.idx });
    }

    // 5. Per-window halves: the top half of a row inserts above it, the
    //    bottom half below it (= above the next row).
    for &(ri, r) in &vc.rows {
        if my < r.y + r.height / 2 {
            return Some(DropZone::Row {
                col: vc.idx,
                row: ri,
            });
        }
    }
    let below_last = vc.rows.last().map_or(0, |&(ri, _)| ri + 1);
    Some(DropZone::Row {
        col: vc.idx,
        row: below_last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Rect;
    use crate::layout::ActualLayout;
    use crate::layout::types::Padding;

    /// The pre-ADR-0018 positional call shape the older resolver tests use:
    /// a pixel-only screen-edge band and no row viewport.
    #[allow(clippy::too_many_arguments)]
    fn rdz_legacy(
        applied: &AppliedLayout,
        monitor: &MonitorInfo,
        _dragged: WindowId,
        mx: i32,
        my: i32,
        edge_band: i32,
        col_edge_ratio: f32,
        col_edge_max_px: i32,
    ) -> Option<DropZone> {
        resolve_drop_zone(
            applied,
            monitor,
            mx,
            my,
            &ZoneParams {
                edge_scroll_px: edge_band,
                edge_scroll_ratio: 0.0,
                col_edge_ratio,
                col_edge_max_px,
                visible_rows: 0,
            },
        )
    }

    fn test_config() -> MutationConfig {
        MutationConfig {
            monitor_width: 1920,
            monitor_height: 1080,
            min_window_height_px: 100,
            min_row_height_px: 100,
            column_width: 960,
            min_column_width_px: 200,
            max_n: 1,
            abs_max_width: 1912,
            padding: Padding {
                window_gap: 4,
                up: 0,
                down: 0,
            },
            columns_per_screen: 2,
            visible_rows: 0,
            max_rows: 0,
            layout_mode: crate::layout::types::LayoutMode::Strip,
            wrap_around: crate::layout::types::WrapAround::Off,
        }
    }

    fn test_monitor() -> MonitorInfo {
        MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        })
    }

    /// Two-column layout: col 0 = [A], col 1 = [B, C].
    fn two_col_layout() -> VirtualLayout {
        let h1 = distribute_heights(1, 1080, 4)[0];
        let h2 = distribute_heights(2, 1080, 4);
        VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), h2[0]), Row::new(WindowId(3), h2[1])],
                ),
            ],
            0,
        )
    }

    /// Three-column layout: col 0 = [A], col 1 = [B], col 2 = [C, D].
    fn three_col_layout() -> VirtualLayout {
        let h1 = distribute_heights(1, 1080, 4)[0];
        let h2 = distribute_heights(2, 1080, 4);
        VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_row(960, Row::new(WindowId(2), h1)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(3), h2[0]), Row::new(WindowId(4), h2[1])],
                ),
            ],
            0,
        )
    }

    // ── Column zone tests ──────────────────────────────────────────────────

    #[test]
    fn column_zone_inserts_new_column_at_index() {
        // [A], [B, C] → drag B to Column{col:0} → [B], [A], [C]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        ); // B at col 0
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        ); // A shifted to col 1
    }

    #[test]
    fn column_zone_at_index_zero() {
        // [A], [B, C] → drag B to Column{col:0} → [B], [A], [C]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        );
    }

    #[test]
    fn column_zone_appends_at_end() {
        // [A], [B], [C, D] → drag B (col 1, single row) to Column{col:99}
        // Remove B → [A], [C, D] (2 cols). adjust(99, 1, true) = 98. min(98, 2) = 2.
        // Insert at 2 → [A], [C, D], [B]
        let layout = three_col_layout();
        let result = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 99 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        let last = applied.virtual_layout.columns.last().unwrap();
        assert_eq!(last.rows[0].window_id, WindowId(2)); // B appended at end
    }

    #[test]
    fn column_zone_clamps_beyond_end() {
        // [A], [B, C] → drag A to Column{col:99} → clamps to append
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Column { col: 99 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 2);
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        );
    }

    #[test]
    fn column_zone_source_removal_shift() {
        // [A], [B], [C, D] → drag A (col 0, single row) to Column{col:2}
        // Remove A → [B], [C, D]. adjust_index(2, 0, true) = 1. Insert at 1.
        // Result: [B], [A], [C, D]
        let layout = three_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Column { col: 2 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 3);
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        );
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        );
    }

    // ── Row zone tests ─────────────────────────────────────────────────────

    #[test]
    fn row_zone_inserts_at_top() {
        // [A], [B, C] → drag A to Row{col:1, row:0} → [B,C] → [A, B, C]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Row { col: 1, row: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 1);
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.len(), 3);
        assert_eq!(col.rows[0].window_id, WindowId(1)); // A at top
        assert_eq!(col.rows[1].window_id, WindowId(2));
        assert_eq!(col.rows[2].window_id, WindowId(3));
    }

    #[test]
    fn row_zone_inserts_at_bottom() {
        // [A], [B, C] → drag A to Row{col:1, row:99} → clamped, appended
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Row { col: 1, row: 99 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.last().unwrap().window_id, WindowId(1));
    }

    #[test]
    fn row_zone_inserts_in_middle() {
        // [A], [B, C] → drag A to Row{col:1, row:1} → [B, A, C]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(1),
            DropZone::Row { col: 1, row: 1 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.len(), 3);
        assert_eq!(col.rows[0].window_id, WindowId(2)); // B
        assert_eq!(col.rows[1].window_id, WindowId(1)); // A in middle
        assert_eq!(col.rows[2].window_id, WindowId(3)); // C
    }

    #[test]
    fn row_zone_cross_column() {
        // [A], [B, C] → drag C to Row{col:0, row:0} → [C, A], [B]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(3),
            DropZone::Row { col: 0, row: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 2);
        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(3)
        ); // C at top of col 0
        assert_eq!(
            applied.virtual_layout.columns[1].rows[0].window_id,
            WindowId(2)
        ); // B alone
    }

    #[test]
    fn row_zone_within_same_column() {
        // [A], [B, C] → drag C (row 1) to Row{col:1, row:0} → [A], [C, B]
        let layout = two_col_layout();
        let result = preview_move(
            &layout,
            WindowId(3),
            DropZone::Row { col: 1, row: 0 },
            &test_config(),
            &test_monitor(),
        );
        let applied = result.expect("should produce layout");
        let col = &applied.virtual_layout.columns[1];
        assert_eq!(col.rows[0].window_id, WindowId(3)); // C moved up
        assert_eq!(col.rows[1].window_id, WindowId(2)); // B
    }

    // ── Scroll zone tests ──────────────────────────────────────────────────

    #[test]
    fn scroll_left_returns_none() {
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::ScrollLeft,
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    #[test]
    fn scroll_right_returns_none() {
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::ScrollRight,
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    // ── Edge cases ─────────────────────────────────────────────────────────

    #[test]
    fn empty_layout_returns_none() {
        let empty = VirtualLayout::new();
        for zone in [
            DropZone::Column { col: 0 },
            DropZone::Row { col: 0, row: 0 },
        ] {
            assert!(
                preview_move(&empty, WindowId(1), zone, &test_config(), &test_monitor()).is_none(),
                "empty layout should return None for zone {zone:?}"
            );
        }
    }

    #[test]
    fn window_not_found_returns_none() {
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(99),
                DropZone::Column { col: 0 },
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    #[test]
    fn no_op_column_same_spot() {
        // [A], [B] single-row columns. Drag A to Column{col:0}.
        // Remove A → [B]. Insert at 0 → [A], [B] — identical → no-op.
        let h1 = distribute_heights(1, 1080, 4)[0];
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_row(960, Row::new(WindowId(2), h1)),
            ],
            0,
        );
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::Column { col: 0 },
                &test_config(),
                &test_monitor()
            )
            .is_none(),
            "dropping back in same spot should be no-op"
        );
    }

    #[test]
    fn row_zone_on_removed_source_column_returns_none() {
        // [A] alone in col 0. Drag A to Row{col:0, row:0}.
        // Remove A → col 0 gone. adjust_index(0, 0, true) = 0.
        // 0 >= 0 (layout has 0 columns) → None.
        let h1 = distribute_heights(1, 1080, 4)[0];
        let layout =
            VirtualLayout::with_columns(vec![Column::with_row(960, Row::new(WindowId(1), h1))], 0);
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::Row { col: 0, row: 0 },
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    #[test]
    fn a_lone_window_dropped_on_its_own_column_stays() {
        // [A], [B, C]: A over its own column's middle, either half, is a
        // no-op, not a row in the column that slides into index 0.
        let layout = two_col_layout();
        for row in [0, 1] {
            assert!(
                preview_move(
                    &layout,
                    WindowId(1),
                    DropZone::Row { col: 0, row },
                    &test_config(),
                    &test_monitor()
                )
                .is_none(),
                "row {row}"
            );
        }
    }

    #[test]
    fn row_zone_out_of_bounds_column_returns_none() {
        // [A], [B, C] → drag A to Row{col:5, row:0}
        // Remove A → [B, C] (1 col). adjust_index(5, 0, true) = 4.
        // 4 >= 1 → None.
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::Row { col: 5, row: 0 },
                &test_config(),
                &test_monitor()
            )
            .is_none()
        );
    }

    // ── Projection correctness ─────────────────────────────────────────────

    #[test]
    fn preview_produces_actual_layout() {
        // Drag B to Column{col:0} — a real move, not a no-op.
        let layout = two_col_layout();
        let applied = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");

        // All windows must have entries in the actual layout.
        assert_eq!(applied.actual_layout.entries.len(), 3);
        let expected_actual = projection::project(
            &applied.virtual_layout,
            &test_monitor(),
            &test_config().padding,
            0,
        );
        assert_eq!(applied.actual_layout, expected_actual);
    }

    // ── Invariants ─────────────────────────────────────────────────────────

    #[test]
    fn preview_preserves_viewport_offset() {
        // remove_window clamps viewport_offset to the new span; preview_move
        // must restore the original offset so a drag preview never moves the
        // camera. (`docs/src/dev-guide/tile-drag.md`)
        let h1 = distribute_heights(1, 1080, 4)[0];
        let h2 = distribute_heights(2, 1080, 4);
        let layout = VirtualLayout::with_columns(
            vec![
                Column::with_row(960, Row::new(WindowId(1), h1)),
                Column::with_rows(
                    960,
                    vec![Row::new(WindowId(2), h2[0]), Row::new(WindowId(3), h2[1])],
                ),
            ],
            960, // scrolled one column right
        );
        let applied = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(
            applied.virtual_layout.viewport_offset, 960,
            "viewport offset must be preserved across preview_move"
        );
    }

    #[test]
    fn preview_preserves_window_count() {
        // A move must never drop or duplicate a window.
        let layout = three_col_layout();
        let applied = preview_move(
            &layout,
            WindowId(1),
            DropZone::Column { col: 2 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(applied.virtual_layout.window_count(), 4);
        let ids: std::collections::HashSet<WindowId> = applied
            .virtual_layout
            .columns
            .iter()
            .flat_map(|c| c.rows.iter().map(|r| r.window_id))
            .collect();
        assert_eq!(ids.len(), 4, "window ids must remain unique after move");
    }

    #[test]
    fn row_insert_redistributes_heights_equally() {
        // insert_row_at must redistribute all rows of the target column to
        // equal heights (±1px integer remainder) for the new row count.
        let layout = two_col_layout();
        let applied = preview_move(
            &layout,
            WindowId(1),
            DropZone::Row { col: 1, row: 0 },
            &test_config(),
            &test_monitor(),
        )
        .expect("should produce layout");
        assert_eq!(applied.virtual_layout.columns.len(), 1);
        let col = &applied.virtual_layout.columns[0];
        assert_eq!(col.rows.len(), 3);
        let heights: Vec<i32> = col.rows.iter().map(|r| r.height).collect();
        let spread = heights.iter().max().unwrap() - heights.iter().min().unwrap();
        assert!(
            spread <= 1,
            "row heights must be equal up to 1px remainder: {heights:?}"
        );
    }

    #[test]
    fn no_op_row_same_spot() {
        // [A], [B, C] → drag C (col 1, row 1) back to Row{col:1, row:1}.
        // Remove + re-insert at the same slot yields an identical layout → None.
        let layout = two_col_layout();
        assert!(
            preview_move(
                &layout,
                WindowId(3),
                DropZone::Row { col: 1, row: 1 },
                &test_config(),
                &test_monitor(),
            )
            .is_none(),
            "dropping a window back into its own row slot should be a no-op"
        );
    }

    // =================================================================
    // resolve_drop_zone tests
    // =================================================================

    fn rdz_monitor() -> MonitorInfo {
        MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        })
    }

    fn rdz_padding(gap: i32) -> Padding {
        Padding {
            window_gap: gap,
            up: 0,
            down: 0,
        }
    }

    /// Build an AppliedLayout projected with the given gap and viewport offset.
    fn make_rdz_applied(columns: Vec<Column>, viewport_offset: i32, gap: i32) -> AppliedLayout {
        let vl = VirtualLayout::with_columns(columns, viewport_offset);
        let actual = projection::project(&vl, &rdz_monitor(), &rdz_padding(gap), 0);
        AppliedLayout {
            virtual_layout: vl,
            actual_layout: actual,
        }
    }

    // -- n=1 column: row split --

    #[test]
    fn rdz_single_col_top_region() {
        // 1 col (W1, h=1000, w=500), gap=0. Col at (0,0,500,1000).
        // band=min(90,72)=72. Body: [72,428). n=1, rh=500.
        // my=100 in top half -> row 0.
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                100,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
    }

    #[test]
    fn rdz_single_col_bottom_region() {
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // my=800 in bottom half -> row 1.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                800,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    #[test]
    fn rdz_single_col_boundary_at_half() {
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // n=1, rh=500. Boundary at col.y + 500 = 500.
        // my=499: rel_y=499, 499/500=0 -> row 0.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                499,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
        // my=500: rel_y=500, 500/500=1 -> row 1.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    // -- n=2 column: three regions --

    #[test]
    fn rdz_two_row_col_three_regions() {
        // 1 col, 2 rows (each 500), w=500, gap=0.
        // Col at (0,0,500,1000). n=2, rh=1000/3=333.
        let layout = make_rdz_applied(
            vec![Column::with_rows(
                500,
                vec![Row::new(WindowId(1), 500), Row::new(WindowId(2), 500)],
            )],
            0,
            0,
        );
        // Top region (0..333): my=100 -> row 0.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                100,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
        // Middle region (333..666): my=400 -> row 1.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                400,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
        // Bottom region (666..1000): my=800 -> row 2.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                800,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 2 }),
        );
    }

    #[test]
    fn rdz_two_row_col_boundaries() {
        let layout = make_rdz_applied(
            vec![Column::with_rows(
                500,
                vec![Row::new(WindowId(1), 500), Row::new(WindowId(2), 500)],
            )],
            0,
            0,
        );
        // Per-window halves (ADR-0018): row 0 spans 0..500 (mid 250), row 1
        // spans 500..1000 (mid 750). Boundaries at 250 and 750.
        for (my, row) in [(249, 0), (250, 1), (749, 1), (750, 2)] {
            assert_eq!(
                rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 250, my, 30, 0.18, 72),
                Some(DropZone::Row { col: 0, row }),
                "my={my}"
            );
        }
    }

    // -- Column-edge band --

    #[test]
    fn rdz_col_edge_band_left() {
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // band=min(90,72)=72. mx=50 (< 0+72=72) -> Column{col:0}.
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 50, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
    }

    #[test]
    fn rdz_col_edge_band_right() {
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // band=72. col_right=500. mx=480 (> 500-72=428) -> Column{col:1}.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                480,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 1 }),
        );
    }

    #[test]
    fn rdz_band_caps_at_max_px() {
        // Wide column: 0.18*1000=180 > 72 -> band=72.
        let layout = make_rdz_applied(
            vec![Column::with_row(1000, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // mx=71 -> inside left band (0..72).
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 71, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
        // mx=73 -> outside band, in body. n=1, rh=500, my=500 -> j=1.
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 73, 500, 30, 0.18, 72),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    #[test]
    fn rdz_band_uses_ratio_for_narrow_col() {
        // Narrow column: 0.18*200=36 < 72 -> band=36.
        let layout = make_rdz_applied(
            vec![Column::with_row(200, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        // mx=35 -> inside left band (0..36).
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 35, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
        // mx=37 -> body. col_right=200. 37 < 200-36=164 -> body.
        // n=1, rh=500, my=500 -> j=1.
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 37, 500, 30, 0.18, 72),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    // -- Seam between columns --

    #[test]
    fn rdz_seam_between_columns() {
        // 2 cols, each 490 wide, gap=10.
        // Canvas: col0 at 10, col1 at 510. Screen: col0 (10,0,490,1000),
        // col1 (510,0,490,1000). Seam: [500, 510).
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        // mx=505 -> in seam -> Column{col:1} (right neighbor).
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                505,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 1 }),
        );
    }

    // -- Cursor outside columns --

    #[test]
    fn rdz_left_of_first_column() {
        // 2 cols, gap=10. Col0 at (10,0,490,1000). mx=5 -> left of col0.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 5, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
    }

    #[test]
    fn rdz_right_of_last_column() {
        // 2 cols 480 wide, gap=10. Col1 spans [500, 980); the gap right of
        // it (mx=985) appends. (Past the work area, mx >= 1000, is no zone.)
        let layout = make_rdz_applied(
            vec![
                Column::with_row(480, Row::new(WindowId(1), 1000)),
                Column::with_row(480, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                1000,
                500,
                30,
                0.18,
                72
            ),
            None,
        );
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                985,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 2 }),
        );
    }

    // -- Own column allowed --

    #[test]
    fn rdz_own_column_allowed() {
        // W1 in col 0. Drag W1, cursor over col 0 -> resolves, not excluded.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            0,
            0,
        );
        let zone = rdz_legacy(&layout, &rdz_monitor(), WindowId(1), 250, 500, 30, 0.18, 72);
        assert!(zone.is_some(), "own column should not be excluded");
        match zone {
            Some(DropZone::Row { col: 0, .. }) | Some(DropZone::Column { col: 0 }) => {}
            other => panic!("expected zone in col 0, got {other:?}"),
        }
    }

    // -- Edge scroll --

    #[test]
    fn rdz_edge_scroll_left_with_offscreen() {
        // 3 cols, each 500, gap=0, viewport_offset=500.
        // Screen: col0 parked left, col1 at (0,0,500,1000), col2 at (500,0,500,1000).
        // viewport_offset=500 > 0 -> can_scroll_left=true.
        // mx=10 (<= 0+30=30) -> ScrollLeft.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
                Column::with_row(500, Row::new(WindowId(3), 1000)),
            ],
            500,
            0,
        );
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 10, 500, 30, 0.18, 72),
            Some(DropZone::ScrollLeft),
        );
    }

    #[test]
    fn rdz_edge_scroll_left_no_offscreen() {
        // 2 cols, viewport_offset=0. No left offscreen.
        // mx=10 -> in left band but no scroll -> falls through to col insert.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            0,
            0,
        );
        let zone = rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 10, 500, 30, 0.18, 72);
        assert_ne!(zone, Some(DropZone::ScrollLeft));
    }

    #[test]
    fn rdz_edge_scroll_right_with_offscreen() {
        // 3 cols, each 500, gap=0, viewport_offset=0.
        // Total span=1500 > 0+1000. Right scroll possible.
        // mx=990 (>= 1000-30=970) -> ScrollRight.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
                Column::with_row(500, Row::new(WindowId(3), 1000)),
            ],
            0,
            0,
        );
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                990,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
    }

    #[test]
    fn rdz_edge_scroll_right_no_offscreen() {
        // 2 cols, each 500, gap=0, viewport_offset=0.
        // Total span=1000 = wa_width. No right offscreen.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            0,
            0,
        );
        let zone = rdz_legacy(
            &layout,
            &rdz_monitor(),
            WindowId(99),
            990,
            500,
            30,
            0.18,
            72,
        );
        assert_ne!(zone, Some(DropZone::ScrollRight));
    }

    // -- Totality --

    #[test]
    fn rdz_totality_all_points_some() {
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
            ],
            0,
            0,
        );
        for &mx in &[0, 200, 499, 500, 750, 999] {
            for &my in &[0, 333, 666, 999] {
                let zone = rdz_legacy(&layout, &rdz_monitor(), WindowId(99), mx, my, 30, 0.18, 72);
                assert!(zone.is_some(), "no zone at ({mx}, {my})");
            }
        }
    }

    // -- Empty layout --

    #[test]
    fn rdz_empty_layout_takes_column_zero() {
        // An empty strip (a cross-screen drop onto an empty monitor) takes
        // the window as its first column anywhere in the work area.
        let empty = AppliedLayout {
            virtual_layout: VirtualLayout::new(),
            actual_layout: ActualLayout::new(),
        };
        assert_eq!(
            rdz_legacy(&empty, &rdz_monitor(), WindowId(99), 500, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
        assert_eq!(
            rdz_legacy(
                &empty,
                &rdz_monitor(),
                WindowId(99),
                500,
                1000,
                30,
                0.18,
                72
            ),
            None,
            "below the work area is still no zone"
        );
    }

    // -- Scrolled viewport --

    #[test]
    fn rdz_scrolled_viewport_uses_projected_coords() {
        // 3 cols, each 500, gap=0, viewport_offset=500.
        // Screen: col1 at (0,0,500,1000), col2 at (500,0,500,1000).
        // mx=250 -> inside col1 (virtual idx 1), body -> Row{col:1, row:0}.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(500, Row::new(WindowId(1), 1000)),
                Column::with_row(500, Row::new(WindowId(2), 1000)),
                Column::with_row(500, Row::new(WindowId(3), 1000)),
            ],
            500,
            0,
        );
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                250,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 1, row: 0 }),
        );
    }

    // =================================================================
    // resolve_drop_zone — additional branch/boundary coverage
    // (these fill gaps left by the suite above: the can_scroll_right
    // single-visible-column branch, the trailing-gap content-right fix,
    // the row-index clamps, the band floor, and seam boundaries)
    // =================================================================

    // -- can_scroll_right: single-visible-column branch (vis_cols.len() < 2) --

    #[test]
    fn rdz_scroll_right_single_visible_column() {
        // 3 cols each exactly monitor-width (1000), gap=0, viewport=0.
        // Project: col0 screen [0,1000) visible; col1/col2 parked right at
        // x=1000 and excluded by build_visible_column_rects (x >= mon_right).
        // vis_cols.len() == 1, idx=0 < columns.len()-1 = 2 -> can scroll right.
        // mx=990 (>= wa_right-edge_band = 970) -> ScrollRight.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(1000, Row::new(WindowId(1), 1000)),
                Column::with_row(1000, Row::new(WindowId(2), 1000)),
                Column::with_row(1000, Row::new(WindowId(3), 1000)),
            ],
            0,
            0,
        );
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                990,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
    }

    #[test]
    fn rdz_scroll_right_single_column_is_last_no_scroll() {
        // Single column total: vis_cols.len()==1, idx=0, but
        // 0 < columns.len()-1 = 0 is false -> can_scroll_right false.
        // Falls through to col body -> right edge band -> Column{col:1}.
        let layout = make_rdz_applied(
            vec![Column::with_row(1000, Row::new(WindowId(1), 1000))],
            0,
            0,
        );
        let zone = rdz_legacy(
            &layout,
            &rdz_monitor(),
            WindowId(99),
            990,
            500,
            30,
            0.18,
            72,
        );
        assert_ne!(zone, Some(DropZone::ScrollRight));
        assert_eq!(zone, Some(DropZone::Column { col: 1 }));
    }

    // -- can_scroll_right: trailing-gap content-right fix (line ~186) --
    // content_right = canvas_width - gap, NOT canvas_width. These two tests
    // pin the fix: gap must not inflate the scrollable span.

    #[test]
    fn rdz_scroll_right_trailing_gap_content_fits() {
        // 2 cols width 490, gap 10, viewport 0.
        // canvas_width = 10 + (490+10) + (490+10) = 1010.
        // content_right = 1010 - 10 = 1000 == wa_width. Strict `>` is false.
        // -> NOT scrollable, even though raw canvas_width (1010) > wa (1000).
        // Both cols visible: col0 screen [10,500), col1 screen [510,1000).
        // mx=985 lands in col1's right edge band -> Column{col:2}.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        let zone = rdz_legacy(
            &layout,
            &rdz_monitor(),
            WindowId(99),
            985,
            500,
            30,
            0.18,
            72,
        );
        assert_ne!(
            zone,
            Some(DropZone::ScrollRight),
            "trailing gap must not count as scrollable content"
        );
        assert_eq!(zone, Some(DropZone::Column { col: 2 }));
    }

    #[test]
    fn rdz_scroll_right_with_gap_still_scrolls_offscreen() {
        // 3 cols width 490, gap 10, viewport 0. col2 is parked offscreen right.
        // canvas_width = 1510; content_right = 1500 > 1000 -> scrollable.
        // The `- gap` adjustment must NOT suppress a true positive.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
                Column::with_row(490, Row::new(WindowId(3), 1000)),
            ],
            0,
            10,
        );
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                990,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
    }

    // -- (n+1) row split: last-region clamp (.min(n) at line ~336) --

    #[test]
    fn rdz_two_row_col_clamps_at_very_bottom() {
        // 1 col, 2 rows (n=2), gap 0. col_rect height 1000, rh = 1000/3 = 333.
        // my=999: rel_y=999, 999/333 = 3, then .min(2) clamps to 2.
        // Without the clamp, j=3 would be out of range (only 0,1,2 valid).
        let layout = make_rdz_applied(
            vec![Column::with_rows(
                500,
                vec![Row::new(WindowId(1), 500), Row::new(WindowId(2), 500)],
            )],
            0,
            0,
        );
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                250,
                999,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 0, row: 2 }),
        );
    }

    // -- negative relative y: .max(0) floor at line ~334 --

    #[test]
    fn rdz_cursor_above_column_clamps_to_row_zero() {
        // gap=10 shifts col down: first row y = up(0) + gap(10) = 10.
        // col_rect.y = 10. Cursor my=5 < 10 -> rel_y = (5-10).max(0) = 0 -> row 0.
        // Exercises the defensive .max(0) clamp on the row-split input.
        let layout = make_rdz_applied(
            vec![Column::with_row(500, Row::new(WindowId(1), 1000))],
            0,
            10,
        );
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 100, 5, 30, 0.18, 72),
            Some(DropZone::Row { col: 0, row: 0 }),
        );
    }

    // -- scroll bands are bounded by the work area (B10) --

    /// A layout with content off-screen on both sides, so both scroll bands
    /// are live: 3 cols of 490 (gap 10) on a 1000-wide monitor, viewport
    /// scrolled 500 right.
    fn rdz_scrollable_both_ways() -> AppliedLayout {
        make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
                Column::with_row(490, Row::new(WindowId(3), 1000)),
                Column::with_row(490, Row::new(WindowId(4), 1000)),
            ],
            500,
            10,
        )
    }

    #[test]
    fn rdz_cursor_left_of_the_work_area_is_not_a_scroll_band() {
        let layout = rdz_scrollable_both_ways();
        // Inside the band: scrolls.
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 0, 500, 30, 0.18, 72),
            Some(DropZone::ScrollLeft),
        );
        // One pixel onto the monitor to the left: not this strip's band.
        assert_ne!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), -1, 500, 30, 0.18, 72),
            Some(DropZone::ScrollLeft),
        );
        assert_ne!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                -800,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollLeft),
        );
    }

    #[test]
    fn rdz_cursor_right_of_the_work_area_is_not_a_scroll_band() {
        let layout = rdz_scrollable_both_ways();
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                999,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
        // wa.right() itself is the first pixel of the neighbour.
        assert_ne!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                1000,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
        assert_ne!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                1500,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::ScrollRight),
        );
    }

    #[test]
    fn rdz_bands_follow_a_monitor_with_a_nonzero_origin() {
        // The same strip on a monitor whose work area starts at x = -1000
        // (a display left of the primary).
        let monitor = MonitorInfo::single(Rect {
            x: -1000,
            y: 0,
            width: 1000,
            height: 1000,
        });
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
                Column::with_row(490, Row::new(WindowId(3), 1000)),
                Column::with_row(490, Row::new(WindowId(4), 1000)),
            ],
            500,
        );
        let actual = projection::project(&vl, &monitor, &rdz_padding(10), 0);
        let layout = AppliedLayout {
            virtual_layout: vl,
            actual_layout: actual,
        };
        assert_eq!(
            rdz_legacy(&layout, &monitor, WindowId(99), -995, 500, 30, 0.18, 72),
            Some(DropZone::ScrollLeft),
        );
        assert_eq!(
            rdz_legacy(&layout, &monitor, WindowId(99), -5, 500, 30, 0.18, 72),
            Some(DropZone::ScrollRight),
        );
        // On the primary monitor (x >= 0): neither band.
        let on_primary = rdz_legacy(&layout, &monitor, WindowId(99), 5, 500, 30, 0.18, 72);
        assert!(
            !matches!(
                on_primary,
                Some(DropZone::ScrollLeft | DropZone::ScrollRight)
            ),
            "got {on_primary:?}"
        );
    }

    // -- stacked monitors: the strip has no zones above or below it --

    /// The same strip on the lower display of a stacked pair: work area
    /// (0, 1440, 1000, 1000) under a display spanning y in [0, 1440).
    fn rdz_stacked_lower() -> (AppliedLayout, MonitorInfo) {
        let lower = Rect {
            x: 0,
            y: 1440,
            width: 1000,
            height: 1000,
        };
        let upper = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1440,
        };
        let vs = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 2440,
        };
        let monitor = MonitorInfo::in_desktop(lower, lower, vs, &[upper]);
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
                Column::with_row(490, Row::new(WindowId(3), 1000)),
                Column::with_row(490, Row::new(WindowId(4), 1000)),
            ],
            500,
        );
        let actual = projection::project(&vl, &monitor, &rdz_padding(10), 0);
        (
            AppliedLayout {
                virtual_layout: vl,
                actual_layout: actual,
            },
            monitor,
        )
    }

    #[test]
    fn rdz_cursor_on_the_display_above_hits_nothing() {
        let (layout, monitor) = rdz_stacked_lower();
        // Same x as the strip's left scroll band / first column / seam, but
        // on the upper display: no zone at all.
        for mx in [5, 100, 500, 995] {
            assert_eq!(
                rdz_legacy(&layout, &monitor, WindowId(99), mx, 700, 30, 0.18, 72),
                None,
                "mx={mx} on the upper display"
            );
            // The last pixel above the work area, too.
            assert_eq!(
                rdz_legacy(&layout, &monitor, WindowId(99), mx, 1439, 30, 0.18, 72),
                None,
                "mx={mx} just above the work area"
            );
        }
        // Inside the strip the same x values resolve as usual.
        assert_eq!(
            rdz_legacy(&layout, &monitor, WindowId(99), 5, 1440, 30, 0.18, 72),
            Some(DropZone::ScrollLeft),
        );
        assert_eq!(
            rdz_legacy(&layout, &monitor, WindowId(99), 995, 2000, 30, 0.18, 72),
            Some(DropZone::ScrollRight),
        );
        assert!(matches!(
            rdz_legacy(&layout, &monitor, WindowId(99), 300, 2000, 30, 0.18, 72),
            Some(DropZone::Row { .. } | DropZone::Column { .. })
        ));
    }

    #[test]
    fn rdz_cursor_below_the_work_area_hits_nothing() {
        let (layout, monitor) = rdz_stacked_lower();
        // wa.bottom() is the first pixel past the strip (a display below,
        // or off-screen): nothing.
        for mx in [5, 300, 995] {
            assert_eq!(
                rdz_legacy(&layout, &monitor, WindowId(99), mx, 2440, 30, 0.18, 72),
                None
            );
            assert_eq!(
                rdz_legacy(&layout, &monitor, WindowId(99), mx, 3000, 30, 0.18, 72),
                None
            );
        }
    }

    // -- column-edge band: .max(1) floor in compute_col_edge_band --

    #[test]
    fn rdz_col_edge_band_floors_at_one_pixel() {
        // Very narrow col (width 3): ratio*width = 0.18*3 = 0.54 -> `as i32`
        // = 0. .min(max_px)=0. .max(1) = 1. So the left band is 1px wide;
        // mx=0 (col.x) falls inside it -> Column{col:0}. Without the floor
        // the band would be 0px and the left-edge insert zone unreachable.
        let layout = make_rdz_applied(vec![Column::with_row(3, Row::new(WindowId(1), 1000))], 0, 0);
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 0, 500, 30, 0.18, 72),
            Some(DropZone::Column { col: 0 }),
        );
        // mx=1 is past the 1px band, into the body -> Row split.
        assert_eq!(
            rdz_legacy(&layout, &rdz_monitor(), WindowId(99), 1, 500, 30, 0.18, 72),
            Some(DropZone::Row { col: 0, row: 1 }),
        );
    }

    // -- seam: exact boundaries map to the right neighbor --

    #[test]
    fn rdz_seam_boundaries_map_to_right_neighbor() {
        // 2 cols width 490, gap 10. col0 screen [10,500), col1 screen [510,1000).
        // Seam = [seam_left=500, seam_right=510). Inclusive on the left,
        // exclusive on the right: mx in [500,510) -> Column{col:1}.
        let layout = make_rdz_applied(
            vec![
                Column::with_row(490, Row::new(WindowId(1), 1000)),
                Column::with_row(490, Row::new(WindowId(2), 1000)),
            ],
            0,
            10,
        );
        // Seam left boundary is inclusive: mx=500 -> Column{1}.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                500,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 1 }),
        );
        // Last seam pixel (seam_right=510 is exclusive): mx=509 -> Column{1}.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                509,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Column { col: 1 }),
        );
        // mx past col1's left edge band (band=72, so body starts at 510+72=582)
        // -> col1 body row split (NOT a Column insert). col_rect.y=10 (gap
        // shifts the column down), so my=500 -> rel_y=490 -> row 0.
        assert_eq!(
            rdz_legacy(
                &layout,
                &rdz_monitor(),
                WindowId(99),
                750,
                500,
                30,
                0.18,
                72
            ),
            Some(DropZone::Row { col: 1, row: 0 }),
        );
    }

    /// A drop onto a column that already holds `max_rows` rows is refused:
    /// the preview is `None`, nothing moves.
    #[test]
    fn row_zone_into_a_full_column_is_refused() {
        let config = MutationConfig {
            max_rows: 2,
            ..test_config()
        };
        // [A] [B, C]: drag A onto the stacked column.
        let layout = two_col_layout();
        assert_eq!(layout.columns[1].rows.len(), 2);
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::Row { col: 1, row: 1 },
                &config,
                &test_monitor()
            )
            .is_none()
        );
        // A column zone is still fine.
        assert!(
            preview_move(
                &layout,
                WindowId(1),
                DropZone::Column { col: 2 },
                &config,
                &test_monitor()
            )
            .is_some()
        );
    }

    // =================================================================
    // ADR-0018 zone map: outer thirds, per-window halves, row bands,
    // edge bands as a twelfth, cross-screen
    // =================================================================

    /// The shipped defaults on a strip with `visible_rows` slots.
    fn defaults(visible_rows: u32) -> ZoneParams {
        ZoneParams {
            edge_scroll_px: 30,
            edge_scroll_ratio: 0.0833,
            col_edge_ratio: 0.33,
            col_edge_max_px: 0,
            visible_rows,
        }
    }

    fn applied_on(
        vl: VirtualLayout,
        monitor: &MonitorInfo,
        gap: i32,
        visible_rows: u32,
    ) -> AppliedLayout {
        let actual = projection::project(&vl, monitor, &rdz_padding(gap), visible_rows);
        AppliedLayout {
            virtual_layout: vl,
            actual_layout: actual,
        }
    }

    /// Three 300-px single-row columns at x 0, 300, 600 (gap 0); nothing
    /// to scroll to.
    fn three_narrow_columns() -> AppliedLayout {
        let vl = VirtualLayout::with_columns(
            (1..=3)
                .map(|i| Column::with_row(300, Row::new(WindowId(i), 1000)))
                .collect(),
            0,
        );
        applied_on(vl, &rdz_monitor(), 0, 0)
    }

    /// One 300-px column (x 300..600) of `heights.len()` rows, after a
    /// single-row column at x 0..300.
    fn stack(heights: &[i32], row_offset: usize, visible_rows: u32) -> AppliedLayout {
        let rows = heights
            .iter()
            .enumerate()
            .map(|(i, &h)| Row::new(WindowId(10 + i as isize), h))
            .collect();
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_row(300, Row::new(WindowId(1), 1000)),
                Column::with_rows(300, rows).with_row_offset(row_offset),
            ],
            0,
        );
        applied_on(vl, &rdz_monitor(), 0, visible_rows)
    }

    fn zone(applied: &AppliedLayout, mx: i32, my: i32, params: &ZoneParams) -> Option<DropZone> {
        resolve_drop_zone(applied, &rdz_monitor(), mx, my, params)
    }

    #[test]
    fn outer_third_of_a_column_inserts_a_column_on_that_side() {
        // Middle column x 300..600, band = 0.33 * 300 = 99.
        let l = three_narrow_columns();
        let p = defaults(0);
        assert_eq!(zone(&l, 300, 500, &p), Some(DropZone::Column { col: 1 }));
        assert_eq!(zone(&l, 398, 500, &p), Some(DropZone::Column { col: 1 }));
        assert_eq!(
            zone(&l, 399, 500, &p),
            Some(DropZone::Row { col: 1, row: 1 })
        );
        assert_eq!(
            zone(&l, 450, 499, &p),
            Some(DropZone::Row { col: 1, row: 0 })
        );
        assert_eq!(
            zone(&l, 500, 500, &p),
            Some(DropZone::Row { col: 1, row: 1 })
        );
        assert_eq!(zone(&l, 501, 500, &p), Some(DropZone::Column { col: 2 }));
        assert_eq!(zone(&l, 599, 500, &p), Some(DropZone::Column { col: 2 }));
    }

    #[test]
    fn a_third_of_a_640_px_column_is_not_capped_by_default() {
        assert_eq!(compute_col_edge_band(640, 0.33, 0), 211);
        assert_eq!(
            compute_col_edge_band(640, 0.33, 120),
            120,
            "an explicit cap still caps"
        );
        assert_eq!(
            compute_col_edge_band(640, 0.9, 0),
            320,
            "the ratio clamps to a half"
        );
    }

    #[test]
    fn one_row_column_splits_at_the_windows_half() {
        let l = stack(&[1000], 0, 0);
        let p = defaults(0);
        assert_eq!(
            zone(&l, 450, 499, &p),
            Some(DropZone::Row { col: 1, row: 0 })
        );
        assert_eq!(
            zone(&l, 450, 500, &p),
            Some(DropZone::Row { col: 1, row: 1 })
        );
    }

    #[test]
    fn three_row_column_splits_each_window_into_halves() {
        // Rows at y 0..333, 333..666, 666..1000 (mids 166, 499, 833).
        let l = stack(&[333, 333, 334], 0, 0);
        let p = defaults(0);
        // top half of the top window: above all; bottom half of the top
        // window and top half of the middle one: between them; ...; bottom
        // half of the bottom window: below all.
        for (my, row) in [
            (0, 0),
            (165, 0),
            (166, 1),
            (400, 1),
            (499, 2),
            (700, 2),
            (833, 3),
            (999, 3),
        ] {
            assert_eq!(
                zone(&l, 450, my, &p),
                Some(DropZone::Row { col: 1, row }),
                "my={my}"
            );
        }
    }

    /// Four rows of 500, two visible: rows `off` and `off + 1` fill the
    /// column (y 0..500 and 500..1000); the others are parked.
    fn four_rows_two_visible(off: usize) -> AppliedLayout {
        stack(&[500, 500, 500, 500], off, 2)
    }

    #[test]
    fn halves_use_the_visible_rows_not_the_parked_ones() {
        let l = four_rows_two_visible(1);
        let p = defaults(2);
        assert_eq!(
            zone(&l, 450, 200, &p),
            Some(DropZone::Row { col: 1, row: 1 })
        );
        assert_eq!(
            zone(&l, 450, 300, &p),
            Some(DropZone::Row { col: 1, row: 2 })
        );
        assert_eq!(
            zone(&l, 450, 600, &p),
            Some(DropZone::Row { col: 1, row: 2 })
        );
        assert_eq!(
            zone(&l, 450, 800, &p),
            Some(DropZone::Row { col: 1, row: 3 })
        );
    }

    #[test]
    fn row_bands_appear_only_toward_parked_rows() {
        // Row band = max(30, round(0.0833 * 1000)) = 83 px.
        let p = defaults(2);
        let mid = four_rows_two_visible(1);
        assert_eq!(zone(&mid, 450, 0, &p), Some(DropZone::ScrollUp { col: 1 }));
        assert_eq!(zone(&mid, 450, 82, &p), Some(DropZone::ScrollUp { col: 1 }));
        assert_eq!(
            zone(&mid, 450, 83, &p),
            Some(DropZone::Row { col: 1, row: 1 })
        );
        assert_eq!(
            zone(&mid, 450, 917, &p),
            Some(DropZone::ScrollDown { col: 1 })
        );
        assert_eq!(
            zone(&mid, 450, 916, &p),
            Some(DropZone::Row { col: 1, row: 3 })
        );
        // At the top of the content: no up band, the top half is "above all".
        let top = four_rows_two_visible(0);
        assert_eq!(
            zone(&top, 450, 10, &p),
            Some(DropZone::Row { col: 1, row: 0 })
        );
        assert_eq!(
            zone(&top, 450, 990, &p),
            Some(DropZone::ScrollDown { col: 1 })
        );
        // At the bottom: no down band, the bottom half is "below all".
        let bottom = four_rows_two_visible(2);
        assert_eq!(
            zone(&bottom, 450, 10, &p),
            Some(DropZone::ScrollUp { col: 1 })
        );
        assert_eq!(
            zone(&bottom, 450, 990, &p),
            Some(DropZone::Row { col: 1, row: 4 })
        );
        // The outer thirds stay column inserts even inside the band height.
        assert_eq!(zone(&mid, 310, 10, &p), Some(DropZone::Column { col: 1 }));
    }

    #[test]
    fn no_row_bands_when_every_row_is_visible() {
        let l = stack(&[500, 500], 0, 2);
        let p = defaults(2);
        assert_eq!(zone(&l, 450, 5, &p), Some(DropZone::Row { col: 1, row: 0 }));
        assert_eq!(
            zone(&l, 450, 995, &p),
            Some(DropZone::Row { col: 1, row: 2 })
        );
    }

    #[test]
    fn scroll_column_rows_is_bounded_by_content() {
        use crate::layout::mutations::scroll_column_rows;
        let mut config = test_config();
        config.visible_rows = 2;
        let vl = four_rows_two_visible(1).virtual_layout;
        let down = scroll_column_rows(&vl, 1, true, &config).expect("one down");
        assert_eq!(down.columns[1].row_offset, 2);
        assert!(
            scroll_column_rows(&down, 1, true, &config).is_none(),
            "content edge"
        );
        let up = scroll_column_rows(&vl, 1, false, &config).expect("one up");
        assert_eq!(up.columns[1].row_offset, 0);
        assert!(scroll_column_rows(&up, 1, false, &config).is_none());
        assert!(
            scroll_column_rows(&vl, 0, true, &config).is_none(),
            "nothing parked"
        );
        assert!(
            scroll_column_rows(&vl, 9, true, &config).is_none(),
            "no such column"
        );
    }

    /// Three 490-px columns, gap 10: the third is off screen right.
    fn scrollable_right(monitor: &MonitorInfo) -> AppliedLayout {
        let vl = VirtualLayout::with_columns(
            (1..=3)
                .map(|i| Column::with_row(490, Row::new(WindowId(i), 1000)))
                .collect(),
            0,
        );
        applied_on(vl, monitor, 10, 0)
    }

    #[test]
    fn edge_bands_are_a_twelfth_of_the_work_area() {
        // 1000 px wide: band = max(30, 83) = 83, so mx >= 917 scrolls.
        let m = rdz_monitor();
        let l = scrollable_right(&m);
        let p = defaults(0);
        assert_eq!(
            resolve_drop_zone(&l, &m, 917, 500, &p),
            Some(DropZone::ScrollRight)
        );
        assert_ne!(
            resolve_drop_zone(&l, &m, 916, 500, &p),
            Some(DropZone::ScrollRight)
        );
        // `edge_scroll_width` is the floor.
        let wide = ZoneParams {
            edge_scroll_px: 200,
            ..p
        };
        assert_eq!(
            resolve_drop_zone(&l, &m, 800, 500, &wide),
            Some(DropZone::ScrollRight)
        );
    }

    #[test]
    fn an_edge_shared_with_a_neighbour_keeps_the_pixel_band() {
        let mut m = rdz_monitor();
        m.neighbour_right = true;
        let l = scrollable_right(&m);
        let p = defaults(0);
        assert_ne!(
            resolve_drop_zone(&l, &m, 917, 500, &p),
            Some(DropZone::ScrollRight)
        );
        assert_eq!(
            resolve_drop_zone(&l, &m, 970, 500, &p),
            Some(DropZone::ScrollRight)
        );
    }

    /// Monitor 1 of a side-by-side pair: work area x 1000..2000.
    fn right_monitor() -> MonitorInfo {
        let mut m = MonitorInfo::single(Rect {
            x: 1000,
            y: 0,
            width: 1000,
            height: 1000,
        });
        m.neighbour_left = true;
        m
    }

    #[test]
    fn cross_screen_resolves_against_the_target_strip() {
        let target = right_monitor();
        let vl = VirtualLayout::with_columns(
            vec![
                Column::with_row(300, Row::new(WindowId(5), 1000)),
                Column::with_rows(
                    300,
                    vec![Row::new(WindowId(6), 500), Row::new(WindowId(7), 500)],
                ),
            ],
            0,
        );
        let l = applied_on(vl, &target, 0, 0);
        let p = defaults(0);
        // Column 1 sits at x 1300..1600 on the target.
        assert_eq!(
            resolve_drop_zone(&l, &target, 1310, 500, &p),
            Some(DropZone::Column { col: 1 })
        );
        assert_eq!(
            resolve_drop_zone(&l, &target, 1450, 600, &p),
            Some(DropZone::Row { col: 1, row: 1 })
        );
        assert_eq!(
            resolve_drop_zone(&l, &target, 1450, 800, &p),
            Some(DropZone::Row { col: 1, row: 2 })
        );
        assert_eq!(
            resolve_drop_zone(&l, &target, 1800, 500, &p),
            Some(DropZone::Column { col: 2 })
        );
        // The same point is no zone of the source strip (monitor 0).
        let src = three_narrow_columns();
        assert_eq!(resolve_drop_zone(&src, &rdz_monitor(), 1450, 600, &p), None);
        // An empty target strip takes the window as column 0.
        let empty = applied_on(VirtualLayout::new(), &target, 0, 0);
        assert_eq!(
            resolve_drop_zone(&empty, &target, 1500, 500, &p),
            Some(DropZone::Column { col: 0 })
        );
    }

    #[test]
    fn preview_insert_seats_a_window_from_another_strip() {
        let config = test_config();
        let monitor = test_monitor();
        let layout = two_col_layout();
        let w = WindowId(42);
        let col = preview_insert(&layout, w, DropZone::Column { col: 1 }, &config, &monitor)
            .expect("column insert");
        assert_eq!(col.virtual_layout.columns.len(), 3);
        assert_eq!(col.virtual_layout.columns[1].rows[0].window_id, w);
        assert!(col.actual_layout.find(w).is_some(), "projected");
        let row = preview_insert(
            &layout,
            w,
            DropZone::Row { col: 1, row: 1 },
            &config,
            &monitor,
        )
        .expect("row insert");
        let ids: Vec<_> = row.virtual_layout.columns[1]
            .rows
            .iter()
            .map(|r| r.window_id)
            .collect();
        assert_eq!(ids, vec![WindowId(2), w, WindowId(3)]);
        let empty = preview_insert(
            &VirtualLayout::new(),
            w,
            DropZone::Column { col: 0 },
            &config,
            &monitor,
        )
        .expect("empty strip");
        assert_eq!(empty.virtual_layout.columns.len(), 1);
        // Refusals: a window already seated, a scroll zone, a missing or
        // full column.
        let refused = |vl: &VirtualLayout, id: WindowId, z: DropZone, c: &MutationConfig| {
            preview_insert(vl, id, z, c, &monitor).is_none()
        };
        assert!(refused(
            &layout,
            WindowId(2),
            DropZone::Column { col: 0 },
            &config
        ));
        assert!(refused(&layout, w, DropZone::ScrollUp { col: 1 }, &config));
        assert!(refused(
            &layout,
            w,
            DropZone::Row { col: 7, row: 0 },
            &config
        ));
        let full = MutationConfig {
            max_rows: 2,
            ..config
        };
        assert!(refused(&layout, w, DropZone::Row { col: 1, row: 0 }, &full));
    }

    #[test]
    fn a_drop_into_a_stack_reveals_the_dropped_row() {
        // Moving window 1 below the visible pair of a 4-row, 2-visible
        // column scrolls the column so the dropped row is on screen.
        let mut config = test_config();
        config.visible_rows = 2;
        let monitor = test_monitor();
        let vl = four_rows_two_visible(0).virtual_layout;
        let p = preview_move(
            &vl,
            WindowId(1),
            DropZone::Row { col: 1, row: 4 },
            &config,
            &monitor,
        )
        .expect("moved");
        let (c, row) = p.virtual_layout.find_window(WindowId(1)).expect("seated");
        let col = &p.virtual_layout.columns[c];
        assert!(
            col.row_visible(row, 2),
            "dropped row {row} visible at offset {}",
            col.row_offset
        );
    }

    #[test]
    fn a_row_drop_into_a_fullscreen_column_is_refused_but_a_column_beside_it_is_not() {
        let mut layout = two_col_layout();
        // Column 0 = [A] fullscreen; drag B out of column 1.
        layout.columns[0].fullscreen = true;
        for row in [0, 1] {
            assert!(
                preview_move(
                    &layout,
                    WindowId(2),
                    DropZone::Row { col: 0, row },
                    &test_config(),
                    &test_monitor(),
                )
                .is_none()
            );
        }
        let beside = preview_move(
            &layout,
            WindowId(2),
            DropZone::Column { col: 1 },
            &test_config(),
            &test_monitor(),
        )
        .expect("a column beside it");
        assert_eq!(beside.virtual_layout.find_window(WindowId(2)), Some((1, 0)));
        assert!(beside.virtual_layout.columns[0].fullscreen);
        assert!(!beside.virtual_layout.columns[1].fullscreen);
    }

    // --- Book mode: pile top cards (ADR-0022) ---

    /// `n` columns of 485 px (1/2 on the 1000 px rdz monitor, gap 10),
    /// ids 1..=n, the view starting at column `first` (0-based), projected
    /// in `mode`.
    fn book_applied(
        n: usize,
        first: usize,
        mode: crate::layout::types::LayoutMode,
    ) -> AppliedLayout {
        let cols: Vec<Column> = (1..=n)
            .map(|i| Column::with_row(485, Row::new(WindowId(i as isize), 980)))
            .collect();
        let vl = VirtualLayout::with_columns(cols, first as i32 * 495);
        let actual = projection::project_with(&vl, &rdz_monitor(), &rdz_padding(10), 0, mode);
        AppliedLayout {
            virtual_layout: vl,
            actual_layout: actual,
        }
    }

    #[test]
    fn a_pile_top_is_one_card_zone_and_the_seam_and_bands_stay() {
        use crate::layout::types::LayoutMode;
        let p = defaults(0);
        let m = rdz_monitor();
        let book = book_applied(4, 1, LayoutMode::Book);
        // Anywhere on the left top (column 1, window 2): a new left top.
        for x in [100, 250, 480] {
            assert_eq!(
                resolve_drop_zone(&book, &m, x, 500, &p),
                Some(DropZone::Card {
                    col: 2,
                    side: PileSide::Left
                }),
                "x={x}"
            );
        }
        // Anywhere on the right top (column 2, window 3): a new right top.
        for x in [510, 700, 900] {
            assert_eq!(
                resolve_drop_zone(&book, &m, x, 500, &p),
                Some(DropZone::Card {
                    col: 2,
                    side: PileSide::Right
                }),
                "x={x}"
            );
        }
        // The seam between them is still a plain new column; the edge bands
        // still flip.
        assert_eq!(
            resolve_drop_zone(&book, &m, 500, 500, &p),
            Some(DropZone::Column { col: 2 })
        );
        assert_eq!(
            resolve_drop_zone(&book, &m, 20, 500, &p),
            Some(DropZone::ScrollLeft)
        );
        assert_eq!(
            resolve_drop_zone(&book, &m, 980, 500, &p),
            Some(DropZone::ScrollRight)
        );
        // The same strip in strip mode keeps the thirds and the row halves.
        let strip = book_applied(4, 1, LayoutMode::Strip);
        assert_eq!(
            resolve_drop_zone(&strip, &m, 250, 300, &p),
            Some(DropZone::Row { col: 1, row: 0 })
        );
    }

    #[test]
    fn an_edge_column_with_an_empty_pile_keeps_the_strip_zones() {
        use crate::layout::types::LayoutMode;
        let p = defaults(0);
        let m = rdz_monitor();
        // The view on the last two columns: no right pile.
        let book = book_applied(3, 1, LayoutMode::Book);
        assert_eq!(
            resolve_drop_zone(&book, &m, 700, 300, &p),
            Some(DropZone::Row { col: 2, row: 0 })
        );
        assert!(matches!(
            resolve_drop_zone(&book, &m, 250, 300, &p),
            Some(DropZone::Card {
                side: PileSide::Left,
                ..
            })
        ));
    }

    #[test]
    fn a_lone_visible_column_is_the_card_of_both_piles_by_half() {
        use crate::layout::types::LayoutMode;
        let cols: Vec<Column> = (1..=3)
            .map(|i| Column::with_row(980, Row::new(WindowId(i), 980)))
            .collect();
        let vl = VirtualLayout::with_columns(cols, 990);
        let actual =
            projection::project_with(&vl, &rdz_monitor(), &rdz_padding(10), 0, LayoutMode::Book);
        let applied = AppliedLayout {
            virtual_layout: vl,
            actual_layout: actual,
        };
        let p = defaults(0);
        let m = rdz_monitor();
        assert_eq!(
            resolve_drop_zone(&applied, &m, 300, 500, &p),
            Some(DropZone::Card {
                col: 2,
                side: PileSide::Left
            })
        );
        assert_eq!(
            resolve_drop_zone(&applied, &m, 700, 500, &p),
            Some(DropZone::Card {
                col: 1,
                side: PileSide::Right
            })
        );
    }

    #[test]
    fn a_card_drop_that_keeps_the_order_is_still_offered() {
        use crate::layout::types::LayoutMode;
        let book = book_applied(4, 1, LayoutMode::Book);
        // The right top (3) onto the left top's card: [1 2 3 4] again, but
        // the camera (the caller's) makes 3 the left top.
        let p = preview_move(
            &book.virtual_layout,
            WindowId(3),
            DropZone::Card {
                col: 2,
                side: PileSide::Left,
            },
            &test_config(),
            &rdz_monitor(),
        )
        .expect("offered");
        assert_eq!(p.virtual_layout.find_window(WindowId(3)), Some((2, 0)));
    }
}
