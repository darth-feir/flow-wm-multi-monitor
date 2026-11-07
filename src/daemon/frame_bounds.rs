//! Invisible frame borders re-measured when a window is placed.
//!
//! A window's invisible borders (`Window::invisible_bounds`) are measured
//! at registration, but some apps (WPF / VS-shell windows such as SSMS and
//! Visual Studio) apply their chrome after the window exists, so DWM still
//! reports frame == window rect then. The daemon re-measures every window
//! a layout places, the dragged window at drag start, a float whose rect
//! it reads back, and the focused window on the periodic foreground sync,
//! which also re-places it. See ADR-0028 and
//! `docs/src/dev-guide/window-registry.md`, *Invisible bounds*.

use windows::Win32::Foundation::HWND;

use crate::common::{InvisibleBounds, Rect};
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::types::FlowWM;

/// The Win32 side of a frame re-measurement, injectable for tests.
pub(super) trait FrameProbe {
    /// Whether DWM can be asked for the frame now (visible, not iconic,
    /// not maximized, not cloaked).
    fn measurable(&self, hwnd: isize) -> bool;
    /// The invisible borders as they are now; `None` when a read failed
    /// or the reading is implausible.
    fn measure(&self, hwnd: isize) -> Option<InvisibleBounds>;
    /// Move the window to `rect` (window-rect space) at once.
    fn place(&self, hwnd: isize, rect: Rect) -> bool;
}

/// The live desktop.
pub(super) struct Win32Frame;

impl FrameProbe for Win32Frame {
    fn measurable(&self, hwnd: isize) -> bool {
        registry_win32::frame_measurable(HWND(hwnd as *mut _))
    }
    fn measure(&self, hwnd: isize) -> Option<InvisibleBounds> {
        registry_win32::measure_invisible_bounds(HWND(hwnd as *mut _))
    }
    fn place(&self, hwnd: isize, rect: Rect) -> bool {
        registry_win32::set_window_rect(hwnd, rect.x, rect.y, rect.width, rect.height)
    }
}

/// The bounds to store after a re-measurement: the fresh reading when one
/// was taken and differs from `stored`, else `None` (keep `stored`).
#[must_use]
pub(super) fn changed_bounds(
    stored: InvisibleBounds,
    measured: Option<InvisibleBounds>,
) -> Option<InvisibleBounds> {
    measured.filter(|m| *m != stored)
}

fn edges(b: InvisibleBounds) -> String {
    format!("L{} T{} R{} B{}", b.left, b.top, b.right, b.bottom)
}

impl FlowWM {
    /// Re-measure the invisible borders of `hwnds` before a placement uses
    /// them; returns how many stored values changed. See
    /// [`remeasure_frame_with`](Self::remeasure_frame_with).
    pub(super) fn remeasure_frames(&mut self, hwnds: impl IntoIterator<Item = isize>) -> usize {
        let probe = Win32Frame;
        hwnds
            .into_iter()
            .filter(|&h| self.remeasure_frame_with(&probe, h, "re-placed"))
            .count()
    }

    /// Re-measure `hwnd`'s invisible borders and store them when they
    /// changed; `true` when they did. Only an active tile or float that the
    /// app does not hold in its own full screen is measured, only while the
    /// animator is idle (a window mid-tween reads as a moved frame), never the
    /// window a drag owns, and only
    /// when the probe calls it measurable; a failed or implausible reading
    /// keeps the stored value. `what` ends the debug line.
    pub(super) fn remeasure_frame_with<P: FrameProbe>(
        &mut self,
        probe: &P,
        hwnd: isize,
        what: &str,
    ) -> bool {
        if self.animator.is_animating()
            || self.is_app_fullscreen(hwnd)
            || self.drag_state.as_ref().map(|d| d.dragged_hwnd()) == Some(hwnd)
        {
            return false;
        }
        let Some(window) = self.registry.get_window(HWND(hwnd as *mut _)) else {
            return false;
        };
        if !matches!(
            window.state,
            WindowState::Tiling(TilingState::Active { .. })
                | WindowState::Floating(FloatingState::Active { .. })
        ) {
            return false;
        }
        let stored = window.invisible_bounds;
        if !probe.measurable(hwnd) {
            return false;
        }
        let Some(fresh) = changed_bounds(stored, probe.measure(hwnd)) else {
            return false;
        };
        log::debug!(
            "window {hwnd:#x} ({}) invisible bounds changed {} -> {}; {what}",
            window.exe,
            edges(stored),
            edges(fresh)
        );
        self.registry.set_invisible_bounds(hwnd, fresh);
        true
    }

    /// The periodic foreground sync's frame check: when the focused
    /// window's invisible borders changed while it sat idle (no layout
    /// event re-measured it), store the new ones and move the window so its
    /// content fills its slot (a tile) or its stored rect (a float) again.
    /// One instant move, no layout animation.
    pub(super) fn recheck_focused_frame(&mut self) {
        self.recheck_focused_frame_with(&Win32Frame);
    }

    /// [`recheck_focused_frame`](Self::recheck_focused_frame) over `probe`.
    pub(super) fn recheck_focused_frame_with<P: FrameProbe>(&mut self, probe: &P) {
        if self.tiling_paused || self.drag_state.is_some() {
            return;
        }
        let Some(focused) = self.registry.focused().map(|id| id.0) else {
            return;
        };
        // Where the window belongs, in visible-rect space, before anything
        // is stored: a tile's last commanded slot, a float's stored rect.
        let Some(window) = self.registry.get_window(HWND(focused as *mut _)) else {
            return;
        };
        let target = match window.state {
            WindowState::Tiling(TilingState::Active { .. }) => self
                .last_submitted_rect(focused)
                .or(window.tiled_rect)
                .map(|slot| (slot, false)),
            WindowState::Floating(FloatingState::Active { rect }) => Some((rect, true)),
            _ => None,
        };
        let Some((visible, is_float)) = target else {
            return;
        };
        if !self.remeasure_frame_with(probe, focused, "re-placed") {
            return;
        }
        let bounds = self
            .registry
            .get_window(HWND(focused as *mut _))
            .map(|w| w.invisible_bounds)
            .unwrap_or_default();
        let rect = if is_float {
            bounds.visible_to_window(visible)
        } else {
            super::animation::tile_geometry(
                self.is_fullscreen(focused),
                bounds,
                visible,
                self.config.borders.thickness as i32,
                self.config.borders.overlap as i32,
            )
        };
        if !probe.place(focused, rect) {
            log::debug!("frame recheck: could not move {focused:#x} to {rect:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::{FrameProbe, changed_bounds};
    use crate::common::{InvisibleBounds, Rect, WindowId};
    use crate::config::types::WindowRulesConfig;
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::registry::WindowRegistry;
    use crate::registry::types::{FloatingState, TilingState, WindowState};
    use crate::registry::win32 as registry_win32;
    use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};

    use super::super::animation::PrevRects;
    use super::super::types::FlowWM;
    use windows::Win32::Foundation::HWND;

    const TILE: isize = 0x7351;
    const FLOAT: isize = 0x7352;
    const SSMS: InvisibleBounds = InvisibleBounds {
        left: 5,
        top: 0,
        right: 5,
        bottom: 5,
    };

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// A scripted desktop: per-window readings, measurability and the
    /// moves made.
    #[derive(Default)]
    struct Desk {
        readings: HashMap<isize, Option<InvisibleBounds>>,
        unmeasurable: Vec<isize>,
        placed: RefCell<Vec<(isize, Rect)>>,
    }

    impl FrameProbe for Desk {
        fn measurable(&self, hwnd: isize) -> bool {
            !self.unmeasurable.contains(&hwnd)
        }
        fn measure(&self, hwnd: isize) -> Option<InvisibleBounds> {
            self.readings.get(&hwnd).copied().flatten()
        }
        fn place(&self, hwnd: isize, rect: Rect) -> bool {
            self.placed.borrow_mut().push((hwnd, rect));
            true
        }
    }

    fn register(registry: &mut WindowRegistry, hwnd: isize, state: WindowState) {
        let info = registry_win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: "W".into(),
            class: "HwndWrapper".into(),
            rect: r(0, 0, 10, 10),
            exe: "late.exe".into(),
            process_path: r"C:\late.exe".into(),
            is_visible: true,
            is_maximized: false,
            is_fullscreen: false,
            is_iconic: false,
            is_elevated: false,
        };
        registry.register_window_from_info(&info);
        if let Some(w) = registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = state;
            // Measured at registration before the app applied its chrome.
            w.invisible_bounds = InvisibleBounds::zero();
        }
    }

    /// One late-chrome tile and one float, ring thickness 3 / overlap 1.
    fn flow(float_rect: Rect) -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        register(
            &mut registry,
            TILE,
            WindowState::Tiling(TilingState::Active { col: 0, row: 0 }),
        );
        register(
            &mut registry,
            FLOAT,
            WindowState::Floating(FloatingState::Active { rect: float_rect }),
        );
        let screen = r(0, 0, 1920, 1080);
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let mut scrolling =
            ScrollingSpace::new(MonitorInfo::single(screen), 960, 320, 100, 100, padding, 4);
        scrolling.add_window(WindowId(TILE));
        let mut ws = Workspace::new(WorkspaceId(1), scrolling);
        ws.floating.add(WindowId(FLOAT), float_rect);
        let monitor = Monitor::new(screen, screen, vec![ws], 0);
        let mut flow = FlowWM::new_for_test(registry, vec![monitor]);
        flow.config.borders.thickness = 3;
        flow.config.borders.overlap = 1;
        flow.registry.set_focused(TILE);
        flow
    }

    fn bounds(flow: &FlowWM, hwnd: isize) -> InvisibleBounds {
        flow.registry
            .get_window(HWND(hwnd as *mut _))
            .map(|w| w.invisible_bounds)
            .expect("tracked")
    }

    fn window_target(flow: &FlowWM, hwnd: isize) -> Rect {
        let tiles = flow.monitors[0].active_scrolling().actual_layout().clone();
        let floats = flow.monitors[0]
            .active_workspace()
            .floating
            .to_actual_layout();
        let out = flow.workspace_targets(0, &[(tiles, 0), (floats, 0)], PrevRects::Registry);
        let t = out
            .targets
            .iter()
            .find(|t| t.window_ref == crate::animation::types::WindowRef(hwnd))
            .expect("a target for the window");
        r(
            t.final_position.x,
            t.final_position.y,
            t.final_size.x,
            t.final_size.y,
        )
    }

    fn slot(flow: &FlowWM) -> Rect {
        flow.monitors[0]
            .active_scrolling()
            .actual_layout()
            .entries
            .iter()
            .find(|e| e.window_id == WindowId(TILE))
            .expect("tile entry")
            .rect
    }

    #[test]
    fn changed_bounds_keeps_the_stored_value_unless_a_reading_differs() {
        let zero = InvisibleBounds::zero();
        assert_eq!(changed_bounds(zero, Some(SSMS)), Some(SSMS));
        assert_eq!(changed_bounds(SSMS, Some(SSMS)), None);
        // A failed or implausible reading (None) keeps what is stored.
        assert_eq!(changed_bounds(SSMS, None), None);
        // A window that lost its chrome goes back to zero.
        assert_eq!(changed_bounds(SSMS, Some(zero)), Some(zero));
    }

    #[test]
    fn a_placement_after_the_remeasure_uses_the_fresh_bounds() {
        let mut flow = flow(r(600, 300, 500, 400));
        let stale = window_target(&flow, TILE);
        let desk = Desk {
            readings: HashMap::from([(TILE, Some(SSMS))]),
            ..Desk::default()
        };
        assert!(flow.remeasure_frame_with(&desk, TILE, "re-placed"));
        assert_eq!(bounds(&flow, TILE), SSMS);
        let fresh = window_target(&flow, TILE);
        assert_eq!(fresh, SSMS.visible_to_window(slot(&flow)).inset(2));
        assert_eq!(fresh.width, stale.width + 10, "grown by L5 + R5");
        assert_eq!(fresh.height, stale.height + 5, "grown by B5");
        // Measuring the same frame again changes nothing.
        assert!(!flow.remeasure_frame_with(&desk, TILE, "re-placed"));
    }

    #[test]
    fn a_failed_measurement_keeps_the_stored_bounds() {
        let mut flow = flow(r(600, 300, 500, 400));
        flow.registry.set_invisible_bounds(TILE, SSMS);
        let desk = Desk {
            readings: HashMap::from([(TILE, None)]),
            ..Desk::default()
        };
        assert!(!flow.remeasure_frame_with(&desk, TILE, "re-placed"));
        assert_eq!(bounds(&flow, TILE), SSMS);
    }

    #[test]
    fn iconic_hidden_or_minimized_windows_are_never_measured() {
        let mut flow = flow(r(600, 300, 500, 400));
        // The probe says not measurable (iconic / hidden / cloaked).
        let desk = Desk {
            readings: HashMap::from([(TILE, Some(garbage()))]),
            unmeasurable: vec![TILE],
            ..Desk::default()
        };
        assert!(!flow.remeasure_frame_with(&desk, TILE, "re-placed"));
        assert_eq!(bounds(&flow, TILE), InvisibleBounds::zero());
        // A minimized record is skipped whatever the probe says.
        if let Some(w) = flow.registry.get_window_mut(HWND(TILE as *mut _)) {
            w.state = WindowState::Tiling(TilingState::Minimized);
        }
        let desk = Desk {
            readings: HashMap::from([(TILE, Some(garbage()))]),
            ..Desk::default()
        };
        assert!(!flow.remeasure_frame_with(&desk, TILE, "re-placed"));
        assert_eq!(bounds(&flow, TILE), InvisibleBounds::zero());
    }

    fn garbage() -> InvisibleBounds {
        InvisibleBounds {
            left: 8,
            top: 8,
            right: 8,
            bottom: 8,
        }
    }

    #[test]
    fn the_focused_recheck_re_places_a_tile_into_its_slot() {
        let mut flow = flow(r(600, 300, 500, 400));
        let s = slot(&flow);
        flow.record_submitted(&[(TILE, s)]);
        let desk = Desk {
            readings: HashMap::from([(TILE, Some(SSMS))]),
            ..Desk::default()
        };
        flow.recheck_focused_frame_with(&desk);
        assert_eq!(bounds(&flow, TILE), SSMS);
        assert_eq!(
            *desk.placed.borrow(),
            vec![(TILE, SSMS.visible_to_window(s).inset(2))]
        );
        // Unchanged on the next tick: no move.
        flow.recheck_focused_frame_with(&desk);
        assert_eq!(desk.placed.borrow().len(), 1);
    }

    #[test]
    fn the_focused_recheck_keeps_a_floats_visible_rect() {
        let float_rect = r(600, 300, 500, 400);
        let mut flow = flow(float_rect);
        flow.registry.set_focused(FLOAT);
        let desk = Desk {
            readings: HashMap::from([(FLOAT, Some(SSMS))]),
            ..Desk::default()
        };
        flow.recheck_focused_frame_with(&desk);
        assert_eq!(
            *desk.placed.borrow(),
            vec![(FLOAT, SSMS.visible_to_window(float_rect))]
        );
        assert_eq!(
            window_target(&flow, FLOAT),
            SSMS.visible_to_window(float_rect)
        );
    }

    #[test]
    fn the_focused_recheck_leaves_a_normal_window_alone() {
        let mut flow = flow(r(600, 300, 500, 400));
        flow.registry.set_invisible_bounds(TILE, SSMS);
        flow.record_submitted(&[(TILE, slot(&flow))]);
        let desk = Desk {
            readings: HashMap::from([(TILE, Some(SSMS))]),
            ..Desk::default()
        };
        flow.recheck_focused_frame_with(&desk);
        assert!(desk.placed.borrow().is_empty());
    }
}
