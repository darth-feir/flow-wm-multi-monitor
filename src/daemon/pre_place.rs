//! Pre-placement: a new window that Windows created on another screen is
//! moved onto the focused screen at `EVENT_OBJECT_CREATE`, before its first
//! show, so it never paints on the wrong screen. ADR-0023, *Pre-placement
//! before the first paint*; `docs/src/dev-guide/event-pipelines.md`,
//! *Created*.

use windows::Win32::Foundation::HWND;

use crate::common::Rect;
use crate::config::types::NewWindowMonitor;
use crate::registry::win32::{self as registry_win32, PrePlaceFacts};

use super::types::FlowWM;

/// How long the thread of a window about to be pre-placed may take to
/// answer a `WM_NULL` before the window is left alone.
const PRE_PLACE_PROBE_MS: u32 = 50;

/// Whether a window with `facts` may be pre-placed: not shown yet, and a
/// plausible top-level app window (a root window with no owner, not a
/// child, not a tool window, with a caption or a sizing frame, neither
/// minimized nor maximized).
#[must_use]
pub(super) fn is_pre_place_candidate(facts: &PrePlaceFacts) -> bool {
    !facts.visible
        && facts.root
        && !facts.owned
        && !facts.child
        && !facts.tool_window
        && facts.chrome
        && !facts.min_or_max
}

/// Where a window at `rect` on the screen with work area `from` goes on
/// the screen with work area `to`: the same offset from the work area's
/// origin, its size kept, shifted so it lies inside `to` (at `to`'s left
/// or top edge when it is wider or taller than `to`).
#[must_use]
pub(super) fn pre_place_origin(rect: Rect, from: Rect, to: Rect) -> (i32, i32) {
    let fit = |pos: i32, size: i32, lo: i32, span: i32| {
        if size >= span {
            lo
        } else {
            pos.clamp(lo, lo + span - size)
        }
    };
    (
        fit(rect.x - from.x + to.x, rect.width, to.x, to.width),
        fit(rect.y - from.y + to.y, rect.height, to.y, to.height),
    )
}

impl FlowWM {
    /// Pre-place `hwnd`, whose `Created` event failed classification (not
    /// shown yet), on the focused monitor when it is a candidate
    /// ([`is_pre_place_candidate`]) on another monitor and no rule would
    /// ignore it. At most once per window ([`FlowWM::pre_placed`]). Never
    /// while tiling is off or with `[focus] new_window_monitor = "placed"`.
    pub(super) fn pre_place_created(&mut self, hwnd: isize) {
        if self.tiling_paused
            || self.config.focus.new_window_monitor != NewWindowMonitor::Focused
            || self.monitors.len() < 2
            || self.pre_placed.contains(&hwnd)
            || self.registry.is_tracked(hwnd)
        {
            return;
        }
        if !registry_win32::pre_place_facts(hwnd).is_some_and(|f| is_pre_place_candidate(&f)) {
            return;
        }
        let to = self.active_monitor;
        let from = self.home_monitor_for_hwnd(hwnd);
        if from == to {
            return;
        }
        let handle = HWND(hwnd as *mut _);
        let Ok(rect) = registry_win32::get_window_rect(handle) else {
            return;
        };
        if rect.width <= 0 || rect.height <= 0 {
            return;
        }
        let class = registry_win32::get_class_name(handle).unwrap_or_default();
        let (exe, path) = registry_win32::get_window_thread_process_id(handle)
            .and_then(registry_win32::get_process_exe_and_path)
            .unwrap_or_else(|_| ("unknown".to_owned(), String::new()));
        if self.registry.would_ignore_untitled(&exe, &class, &path) {
            log::debug!("window {hwnd:#x} ({exe}) not pre-placed: a rule ignores it");
            return;
        }
        // UIPI refuses the move of an elevated window (ADR-0025).
        if registry_win32::window_is_elevated(handle) {
            log::debug!("window {hwnd:#x} ({exe}) not pre-placed: it is elevated");
            return;
        }
        // The move is synchronous: a window whose thread is not pumping
        // (still initializing) would stall the daemon until it does.
        if !registry_win32::answers_within(hwnd, PRE_PLACE_PROBE_MS) {
            log::debug!(
                "window {hwnd:#x} ({exe}) not pre-placed: its thread did not answer within {PRE_PLACE_PROBE_MS} ms"
            );
            return;
        }
        let (x, y) = pre_place_origin(
            rect,
            self.monitors[from].work_area(),
            self.monitors[to].work_area(),
        );
        if registry_win32::move_window_keep_size(hwnd, x, y) {
            self.pre_placed.insert(hwnd);
            log::debug!(
                "pre-placed window {hwnd:#x} ({exe}) on the focused monitor {to} before show (placed on {from})"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn app_window() -> PrePlaceFacts {
        PrePlaceFacts {
            visible: false,
            root: true,
            owned: false,
            child: false,
            tool_window: false,
            chrome: true,
            min_or_max: false,
        }
    }

    /// Positive: a hidden, unowned, captioned root window is a candidate.
    #[test]
    fn a_hidden_top_level_app_window_is_a_candidate() {
        assert!(is_pre_place_candidate(&app_window()));
    }

    /// Negative: any one disqualifying fact rules the window out — shown
    /// already, not a root, owned, a child, a tool window, no chrome,
    /// minimized or maximized.
    #[test]
    fn each_disqualifying_fact_rules_the_window_out() {
        let cases = [
            PrePlaceFacts {
                visible: true,
                ..app_window()
            },
            PrePlaceFacts {
                root: false,
                ..app_window()
            },
            PrePlaceFacts {
                owned: true,
                ..app_window()
            },
            PrePlaceFacts {
                child: true,
                ..app_window()
            },
            PrePlaceFacts {
                tool_window: true,
                ..app_window()
            },
            PrePlaceFacts {
                chrome: false,
                ..app_window()
            },
            PrePlaceFacts {
                min_or_max: true,
                ..app_window()
            },
        ];
        for facts in cases {
            assert!(!is_pre_place_candidate(&facts), "{facts:?}");
        }
    }

    /// Positive: the offset from the work area's origin is kept.
    #[test]
    fn the_offset_from_the_work_area_is_kept() {
        let right = r(1920, 0, 1920, 1040);
        let left = r(0, 0, 1920, 1040);
        assert_eq!(
            pre_place_origin(r(2020, 100, 800, 600), right, left),
            (100, 100)
        );
    }

    /// Boundary: a window that would stick out of the target work area is
    /// shifted inside it, keeping its size.
    #[test]
    fn a_window_sticking_out_is_shifted_inside() {
        let from = r(0, 0, 2560, 1400);
        let to = r(-1920, 0, 1920, 1040);
        assert_eq!(
            pre_place_origin(r(2000, 900, 800, 600), from, to),
            (-800, 440)
        );
    }

    /// Boundary: a window larger than the target work area goes to its
    /// top-left corner (its size is kept, so it cannot fit).
    #[test]
    fn a_window_larger_than_the_target_goes_to_its_corner() {
        let from = r(0, 0, 2560, 1400);
        let to = r(-1920, 20, 1920, 1040);
        assert_eq!(
            pre_place_origin(r(100, 50, 2400, 1300), from, to),
            (-1920, 20)
        );
    }

    /// Boundary: a window left of its own work area (negative offset) is
    /// clamped to the target's left edge.
    #[test]
    fn a_negative_offset_is_clamped_to_the_edge() {
        let from = r(0, 0, 1920, 1040);
        let to = r(1920, 0, 1920, 1040);
        assert_eq!(pre_place_origin(r(-50, -8, 800, 600), from, to), (1920, 0));
    }
}
