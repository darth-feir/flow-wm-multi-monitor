//! The placement acceptance check: a tile that keeps its own size instead
//! of its slot's is released to the floating layer (ADR-0036).
//!
//! Every submit path records the windows it commanded; once the animator
//! is idle and [`SETTLE_MS`] have passed since the last submit, each of
//! them that is still an active tile has its rect read back once. A tile
//! whose size is still exactly the size it had when flow met it, while its
//! slot is larger by more than [`REFUSAL_MIN_PX`] in width or height, has
//! refused the slot: it leaves the strip and floats at the size it insists
//! on. A window that grew to a size of its own (a minimum track size wider
//! than the column) is never released: in no dimension is it smaller than
//! asked. A hung or slow window is never judged.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::common::{Rect, Size, WindowId};
use crate::registry::types::{TilingState, WindowState};
use crate::registry::win32 as registry_win32;
use crate::workspace::FloatingSpace;

use super::animation::{PrevRects, tile_geometry};
use super::types::FlowWM;

/// How long after the last submit (and with the animator idle) the
/// commanded tiles are read back.
pub(super) const SETTLE_MS: u64 = 500;

/// How far a size may drift from the size the window had when flow met it
/// and still count as "kept its own size".
pub(super) const KEEP_TOLERANCE_PX: i32 = 2;

/// How much smaller than its slot (in width or height) a window must stay
/// to count as refusing it; less is a fit (a cell-snapping terminal, a
/// rounding app).
pub(super) const REFUSAL_MIN_PX: i32 = 32;

/// How long the window's thread may take to answer before it is not
/// judged (stage 41's probe).
const ANSWER_MS: u32 = 50;

/// Whether a tile at `actual` (window rect, read back after its placement)
/// refused the window rect `expected`: its size is still `own` (the size it
/// had when flow met it, within [`KEEP_TOLERANCE_PX`]) and it is smaller
/// than `expected` by more than [`REFUSAL_MIN_PX`] in width or height. A
/// window as large as asked or larger in both dimensions (clamped to its
/// minimum size) never refused.
#[must_use]
pub(super) fn refused_its_rect(own: Size, actual: Rect, expected: Rect) -> bool {
    let kept = (actual.width - own.w).abs() <= KEEP_TOLERANCE_PX
        && (actual.height - own.h).abs() <= KEEP_TOLERANCE_PX;
    let short = expected.width - actual.width > REFUSAL_MIN_PX
        || expected.height - actual.height > REFUSAL_MIN_PX;
    kept && short
}

/// Whether `inner` lies entirely inside `outer`.
fn inside(inner: Rect, outer: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

impl FlowWM {
    /// Note the windows a submit path just commanded for the placement
    /// check, due [`SETTLE_MS`] from now.
    pub(super) fn note_placements(&mut self, hwnds: impl IntoIterator<Item = isize>) {
        let before = self.placement_checks.len();
        let mut any = false;
        for hwnd in hwnds {
            self.placement_checks.insert(hwnd);
            any = true;
        }
        if any || self.placement_checks.len() > before {
            self.placement_check_due = Some(Instant::now() + Duration::from_millis(SETTLE_MS));
        }
    }

    /// The main loop's hook: when the check is due and the animator idle,
    /// read back every noted window once; while the animator still runs,
    /// look again shortly.
    pub(super) fn maybe_check_placements(&mut self) {
        let Some(due) = self.placement_check_due else {
            return;
        };
        let now = Instant::now();
        if now < due {
            return;
        }
        if self.animator.is_animating() {
            self.placement_check_due = Some(now + Duration::from_millis(100));
            return;
        }
        self.placement_check_due = None;
        let hwnds: Vec<isize> = self.placement_checks.drain().collect();
        if self.tiling_paused || self.registry.session_locked() {
            return;
        }
        for hwnd in hwnds {
            self.check_placement(hwnd);
        }
    }

    /// Read `hwnd` back and release it when it refused its slot.
    fn check_placement(&mut self, hwnd: isize) {
        let handle = HWND(hwnd as *mut _);
        let Some(window) = self.registry.get_window(handle) else {
            return;
        };
        if !matches!(
            window.state,
            WindowState::Tiling(TilingState::Active { .. })
        ) || self.is_fullscreen(hwnd)
            || self.is_app_fullscreen(hwnd)
            || self.drag_state.as_ref().map(|d| d.dragged_hwnd()) == Some(hwnd)
        {
            return;
        }
        let own = window.last_natural_size;
        let bounds = window.invisible_bounds;
        let exe = window.exe.clone();
        let class = window.class.clone();
        let Some(slot) = self.last_submitted_rect(hwnd) else {
            return;
        };
        let expected = tile_geometry(
            false,
            bounds,
            slot,
            self.config.borders.thickness as i32,
            self.config.borders.overlap as i32,
        );
        if !registry_win32::is_window_visible(handle) || registry_win32::is_iconic(handle) {
            return;
        }
        let Ok(actual) = registry_win32::get_window_rect(handle) else {
            return;
        };
        if !refused_its_rect(own, actual, expected) {
            return;
        }
        // A window that does not answer has not had the chance to take its
        // slot (ADR-0035 parks it instead).
        if registry_win32::is_hung(hwnd) || registry_win32::misses_message_within(hwnd, ANSWER_MS) {
            log::debug!("placement check: window {hwnd:#x} ({exe}) does not answer; not judged");
            return;
        }
        // Answered: anything it had queued is done. Read once more.
        let Ok(actual) = registry_win32::get_window_rect(handle) else {
            return;
        };
        if !refused_its_rect(own, actual, expected) {
            return;
        }
        log::info!(
            "window {hwnd:#x} ({exe} {class:?}) kept its own size {}x{} for a {}x{} slot; released to the floating layer (refused its rect)",
            actual.width,
            actual.height,
            expected.width,
            expected.height
        );
        self.release_refused_tile(hwnd, actual);
    }

    /// Take `hwnd` out of its strip and float it at `actual` (its window
    /// rect), where it is when that lies inside its screen's work area,
    /// else centred there at its own size. One batch: the strip closes the
    /// gap while the float stays put.
    fn release_refused_tile(&mut self, hwnd: isize, actual: Rect) {
        let id = WindowId(hwnd);
        let Some(idx) = self.monitors.iter().position(|m| {
            m.active_scrolling()
                .virtual_layout()
                .find_window(id)
                .is_some()
        }) else {
            return;
        };
        let bounds = self
            .registry
            .get_window(HWND(hwnd as *mut _))
            .map(|w| w.invisible_bounds)
            .unwrap_or_default();
        let visible = bounds.window_to_visible(actual);
        let work_area = self.monitors[idx].active_scrolling().monitor().work_area;
        let rect = if inside(visible, work_area) {
            visible
        } else {
            FloatingSpace::centered_rect(
                Size {
                    w: visible.width,
                    h: visible.height,
                },
                work_area,
            )
        };
        let source = self.monitors[idx].active_scrolling_mut().remove_window(id);
        if let Some(w) = self.registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.refused_rect = true;
        }
        let float_actual = self.register_float(id, rect);
        self.animate_workspaces_on(
            idx,
            &[(source.actual_layout.clone(), 0), (float_actual, 0)],
            PrevRects::Registry,
        );
        self.registry
            .update_tiling_slots_from_layout(&source.virtual_layout);
        self.registry.update_tiled_rects(&source.actual_layout);
        self.refresh_border_for(hwnd);
    }
}

#[cfg(test)]
mod tests {
    use super::refused_its_rect;
    use crate::common::{Rect, Size};

    fn r(w: i32, h: i32) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_window_that_kept_its_own_size_in_a_larger_slot_refused_it() {
        // The share control bar: 297x35 in a 900x1030 slot.
        assert!(refused_its_rect(
            Size { w: 297, h: 35 },
            r(297, 35),
            r(900, 1030)
        ));
        // Wider than its slot but far shorter: still refused.
        assert!(refused_its_rect(
            Size { w: 1200, h: 400 },
            r(1200, 400),
            r(900, 1030)
        ));
    }

    #[test]
    fn a_window_that_took_its_slot_or_nearly_did_not_refuse_it() {
        assert!(!refused_its_rect(
            Size { w: 600, h: 400 },
            r(900, 1030),
            r(900, 1030)
        ));
        // A cell-snapping terminal a few pixels short.
        assert!(!refused_its_rect(
            Size { w: 600, h: 400 },
            r(896, 1020),
            r(900, 1030)
        ));
        // Its own size, but within the refusal margin of its slot.
        assert!(!refused_its_rect(
            Size { w: 880, h: 1010 },
            r(880, 1010),
            r(900, 1030)
        ));
    }

    #[test]
    fn a_window_clamped_to_its_minimum_size_did_not_refuse_its_slot() {
        // Minimum width 1300 in a 600-wide column: the height followed.
        assert!(!refused_its_rect(
            Size { w: 1300, h: 700 },
            r(1300, 1030),
            r(600, 1030)
        ));
        // Met at its minimum size, larger than the slot both ways.
        assert!(!refused_its_rect(
            Size { w: 1300, h: 1100 },
            r(1300, 1100),
            r(600, 1030)
        ));
    }
}
