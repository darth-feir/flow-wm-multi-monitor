//! The restore seat: a tile minimized from its strip comes back into the
//! column it left, and the view follows it. ADR-0027;
//! `docs/src/dev-guide/event-pipelines.md`, *Minimize / Restore*.

use crate::common::WindowId;
use crate::registry::RestoreSeat;
use crate::registry::win32 as registry_win32;

use super::types::FlowWM;

/// Where a restored tile is seated in the strip of the monitor it comes
/// back to: the arguments of
/// [`ScrollingSpace::restore_window_at`](crate::workspace::ScrollingSpace::restore_window_at).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SeatPlan {
    /// Column index, at most the strip's column count (= append).
    pub col: usize,
    /// Row within the column when it rejoins one.
    pub row: usize,
    /// It becomes a column of its own.
    pub alone: bool,
    /// The new column's width.
    pub width_px: i32,
}

/// The seat for a tile coming back to monitor `home`, whose active
/// workspace is `active_ws` with `columns` columns, from the `seat` it left
/// (`None`: none recorded, e.g. registered minimized). `None` means the
/// fallback, appended at the far right. A seat on another monitor (the
/// window comes back elsewhere) has nothing to say about this strip. A seat
/// on another workspace of this monitor (switched away, or the workspace is
/// gone) lands in the active workspace at the same column index as a column
/// of its own. The index is clamped to the column count: columns closed
/// while it was minimized move it towards the end, never past it.
#[must_use]
pub(super) fn plan_restore_seat(
    seat: Option<RestoreSeat>,
    home: usize,
    active_ws: u32,
    columns: usize,
) -> Option<SeatPlan> {
    let seat = seat?;
    if seat.monitor != home {
        return None;
    }
    let col = seat.col.min(columns);
    Some(SeatPlan {
        col,
        row: seat.row,
        alone: seat.alone || seat.workspace != active_ws || col == columns,
        width_px: seat.width_px,
    })
}

impl FlowWM {
    /// Record the seat `window` has in its strip right now (monitor,
    /// workspace, column, row, alone, width) as its restore seat; cleared
    /// when it is not a seated tile. Called by the minimize hook before the
    /// window leaves the strip.
    pub(super) fn capture_restore_seat(&mut self, window: WindowId) {
        let seat = self.monitor_index_containing(window).and_then(|idx| {
            let ws_id = self.monitors[idx].find_workspace_containing(window)?;
            let layout = self.monitors[idx]
                .workspace(ws_id)?
                .scrolling
                .virtual_layout();
            let (col, row) = layout.find_window(window)?;
            let column = layout.columns.get(col)?;
            Some(RestoreSeat {
                monitor: idx,
                workspace: ws_id.0,
                col,
                row,
                alone: column.rows.len() == 1,
                width_px: column.width_px,
            })
        });
        if let Some(s) = seat {
            log::debug!(
                "window {:#x} minimized from monitor {} workspace {} column {} row {}",
                window.0,
                s.monitor,
                s.workspace,
                s.col,
                s.row
            );
        }
        self.registry.set_restore_seat(window.0, seat);
    }

    /// A restored tile that Windows made the foreground is the focus: the
    /// registry and its border follow, its monitor becomes the active one,
    /// and a pending focus hand-off (armed when it was minimized) is
    /// dropped so it cannot send the foreground back.
    pub(super) fn focus_restored_tile(&mut self, hwnd: isize, idx: usize) {
        if registry_win32::get_foreground_window() != Some(hwnd) {
            return;
        }
        self.focus_handoff = None;
        self.set_active_monitor(idx);
        let prev = self.registry.focused().map(|id| id.0);
        self.registry.set_focused(hwnd);
        if let Some(p) = prev
            && p != hwnd
        {
            self.refresh_border_for(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SeatPlan, plan_restore_seat};
    use crate::registry::RestoreSeat;

    fn seat(monitor: usize, workspace: u32, col: usize, row: usize, alone: bool) -> RestoreSeat {
        RestoreSeat {
            monitor,
            workspace,
            col,
            row,
            alone,
            width_px: 640,
        }
    }

    #[test]
    fn no_seat_or_another_monitor_falls_back_to_the_end() {
        assert_eq!(plan_restore_seat(None, 0, 1, 3), None);
        assert_eq!(
            plan_restore_seat(Some(seat(1, 1, 1, 0, true)), 0, 1, 3),
            None
        );
    }

    #[test]
    fn the_old_column_comes_back() {
        assert_eq!(
            plan_restore_seat(Some(seat(0, 1, 1, 0, true)), 0, 1, 2),
            Some(SeatPlan {
                col: 1,
                row: 0,
                alone: true,
                width_px: 640
            })
        );
        // A stacked tile rejoins its column at its row.
        assert_eq!(
            plan_restore_seat(Some(seat(0, 1, 1, 2, false)), 0, 1, 3),
            Some(SeatPlan {
                col: 1,
                row: 2,
                alone: false,
                width_px: 640
            })
        );
    }

    #[test]
    fn a_seat_past_the_end_is_clamped_to_a_new_last_column() {
        // Minimized from column 4 of 5; two columns closed since.
        let p = plan_restore_seat(Some(seat(0, 1, 4, 1, false)), 0, 1, 3).expect("plan");
        assert_eq!((p.col, p.alone), (3, true));
        // An empty strip: column 0.
        let p = plan_restore_seat(Some(seat(0, 1, 2, 0, true)), 0, 1, 0).expect("plan");
        assert_eq!((p.col, p.alone), (0, true));
    }

    #[test]
    fn a_seat_on_another_workspace_lands_in_the_active_one_as_its_own_column() {
        let p = plan_restore_seat(Some(seat(0, 2, 1, 1, false)), 0, 1, 3).expect("plan");
        assert_eq!((p.col, p.row, p.alone), (1, 1, true));
        let p = plan_restore_seat(Some(seat(0, 7, 9, 0, true)), 0, 1, 2).expect("plan");
        assert_eq!((p.col, p.alone), (2, true));
    }
}
