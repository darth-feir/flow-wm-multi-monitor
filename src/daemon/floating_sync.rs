//! Floating-window location synchronization.
//!
//! Handles `EVENT_OBJECT_LOCATIONCHANGE` for the floats on every monitor's
//! active workspace: reads the window's current screen rect, translates it
//! back into [`FloatingSpace`]'s visible-rect space, and writes it into both
//! the owning monitor's active-workspace [`FloatingSpace`] (re-homing the
//! float when it was dragged onto another monitor) and the registry's
//! [`FloatingState::Active`](crate::registry::types::FloatingState::Active)
//! mirror.
//!
//! See (`docs/src/dev-guide/floating-space.md`) for why both copies are kept in
//! sync and how the timer suppression avoids capturing flow's own animation
//! moves as user drags.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::common::{Rect, WindowId};
use crate::registry::hooks::{
    float_hwnds_snapshot, float_tracking_active, is_float_hwnd, set_float_hwnds,
    set_float_tracking_active,
};
use crate::registry::types::{FloatingState, WindowState};
use crate::workspace::Monitor;

use super::borders::float_border_rect;
use super::drag::{DragProbe, Win32Drag};
use super::types::FlowWM;

impl FlowWM {
    /// Apply a location-change notification for a floating window.
    ///
    /// Re-reads the window's rect via `GetWindowRect`, converts it from
    /// window-rect space to visible-rect space, and upserts it into the active
    /// workspace's [`FloatingSpace`] and the registry's
    /// [`FloatingState::Active`](crate::registry::types::FloatingState::Active).
    ///
    /// The hook callback already filters by `FLOAT_HWNS` membership and
    /// `FLOAT_TRACKING_ACTIVE`; this handler re-checks both on the main thread
    /// to close the race between the callback forwarding an event and the
    /// daemon flipping tracking off (flow float animation) or removing the
    /// window from the float set (tile/destroy/switch).
    pub(super) fn on_float_location_changed(&mut self, hwnd: isize) {
        // Race-closing re-check #1: tracking may have been turned off after
        // the callback forwarded this event.
        if !float_tracking_active() {
            return;
        }
        // Race-closing re-check #2: the window may have left the float set
        // (tiled / destroyed / workspace switched away) in the interim.
        if !is_float_hwnd(hwnd) {
            return;
        }
        self.store_float_rect(hwnd);
    }

    /// Read `hwnd`'s current on-screen rect and store it as the window's
    /// float position, in both the active workspace's [`FloatingSpace`] and the
    /// registry's [`FloatingState::Active`] mirror.
    ///
    /// Unlike [`on_float_location_changed`](Self::on_float_location_changed),
    /// this does NOT check `FLOAT_TRACKING_ACTIVE` — the caller gates that.
    /// The resume poll uses this entry point so it can store rects while the
    /// flag is intentionally off (poll-then-enable ordering).
    ///
    /// Skips silently if `hwnd` is untracked, not an active float, or not in
    /// any monitor's active-workspace `FloatingSpace` (see
    /// [`relocate_float`](Self::relocate_float), which also re-homes a float
    /// dragged onto another monitor).
    pub(super) fn store_float_rect(&mut self, hwnd: isize) {
        self.store_float_rect_with(&Win32Drag, hwnd);
    }

    /// [`store_float_rect`](Self::store_float_rect) against `probe`.
    pub(super) fn store_float_rect_with<P: DragProbe>(&mut self, probe: &P, hwnd: isize) {
        // Tiling off: floats are Windows' to place; the bookkeeping rect is
        // refreshed from the desktop when tiling resumes (`daemon::tiling`).
        if self.tiling_paused {
            return;
        }
        let hwnd_handle = HWND(hwnd as *mut _);

        // A minimize moves the window to its iconic (-32000, -32000) rect
        // and fires LOCATIONCHANGE for it — sometimes before MINIMIZESTART
        // has flipped the registry state below. Never record that rect: a
        // float stored off the virtual screen is "clamped on-screen" at
        // icon size by `tiling off` and the hotplug reconcile.
        if probe.is_iconic(hwnd) {
            return;
        }

        // Read the window-rect (full, including invisible borders).
        let Some(window_rect) = probe.window_rect(hwnd) else {
            return;
        };

        // Bounds + state come from the registry. A non-Active float (minimized
        // or hidden) is skipped so its stored rect isn't clobbered by a stale
        // GetWindowRect reading.
        let Some(window) = self.registry.get_window(hwnd_handle) else {
            return;
        };
        if !matches!(
            window.state,
            WindowState::Floating(FloatingState::Active { .. })
        ) {
            return;
        }
        let invisible_bounds = window.invisible_bounds;

        // Convert window-rect → visible-rect (the space FloatingSpace stores).
        let visible_rect = invisible_bounds.window_to_visible(window_rect);
        let wid = WindowId(hwnd);

        // Seat the overlay at the visible rect outset by `(thickness − overlap)`
        // — the float counterpart of the tile side's content inset — so the
        // ring lands in the surrounding gap like a tiled border. The formula is
        // shared with the creation path via `float_border_rect`, keeping the two
        // paths visually identical.
        let border_rect = float_border_rect(
            visible_rect,
            self.config.borders.thickness,
            self.config.borders.overlap,
        );

        // A float the keymap is moving (`drag begin`): the ring follows the
        // window, the bookkeeping waits for `drag end`, which records the
        // final rect once (`daemon::drag_session`).
        if self.manual_float_drag_hwnd() == Some(hwnd) {
            if let Some(border) = window.border.as_ref() {
                border.set_geometry(border_rect);
            }
            return;
        }

        // FLOAT_HWNS holds the active-workspace floats of every monitor, so
        // the window must be in SOME monitor's active-workspace FloatingSpace.
        // Guard the upsert so a stray event can never append a bogus entry,
        // and re-home the float when the user dragged it onto another
        // monitor.
        if !self.relocate_float(wid, visible_rect) {
            return;
        }

        // Mirror into the registry so the two copies never drift.
        if let Some(win) = self.registry.get_window_mut(hwnd_handle) {
            win.state = WindowState::Floating(FloatingState::Active { rect: visible_rect });
            // Update the border overlay to match the new float position.
            // The border follows the window's real OS-reported rect (via
            // the LOCATIONCHANGE hook), not the layout engine's commanded
            // rect — this is the float-side counterpart to the animator's
            // tile-side border flattening. set_geometry takes &self
            // (interior mutability) so this is sound through &mut Window.
            if let Some(border) = win.border.as_ref() {
                border.set_geometry(border_rect);
            }
        }

        log::trace!(
            "float location: hwnd={hwnd} stored visible rect ({},{},{},{})",
            visible_rect.x,
            visible_rect.y,
            visible_rect.width,
            visible_rect.height
        );
    }

    /// Store `visible_rect` as `wid`'s float position, re-homing the float to
    /// the monitor its centre now lies on.
    ///
    /// The float must be in the active workspace's [`FloatingSpace`] of the
    /// monitor that owns it (see
    /// [`monitor_index_containing`](Self::monitor_index_containing)); a
    /// window that is not — untracked, tiled, or on a parked workspace —
    /// is left alone and `false` is returned. When the rect's centre lies on
    /// a different monitor than the owner (the user dragged it across), the
    /// entry moves to that monitor's active workspace, the owning-monitor
    /// memory is updated and the float set is rebuilt, so `move-to-monitor`
    /// and the next workspace switch reason from the right monitor (B9).
    /// When the re-homed float is the registry's focused window the active
    /// monitor follows it too: the user dragged their focus over there, and
    /// no focus event will say so.
    ///
    /// Pure bookkeeping (no Win32): the caller reads the rect.
    pub(super) fn relocate_float(&mut self, wid: WindowId, visible_rect: Rect) -> bool {
        let Some(owner) = self.monitor_index_containing(wid) else {
            return false;
        };
        if !self.monitors[owner]
            .active_workspace()
            .floating
            .contains(wid)
        {
            return false;
        }
        let target = self
            .monitor_index_for_rect_opt(visible_rect)
            .unwrap_or(owner);
        if target != owner {
            log::debug!(
                "float {}: re-homed from monitor {owner} to monitor {target}",
                wid.0
            );
            self.monitors[owner]
                .active_workspace_mut()
                .floating
                .remove(wid);
        }
        self.monitors[target]
            .active_workspace_mut()
            .floating
            .add(wid, visible_rect);
        self.remember_monitor(wid, target);
        if target != owner {
            self.rebuild_float_hwnds();
            if self.registry.focused() == Some(wid) {
                self.set_active_monitor(target);
            }
        }
        true
    }

    /// Replace the float-tracking set with the floats of every monitor's
    /// active workspace (see [`float_hwnds_union`]). Called whenever a
    /// workspace becomes active on any monitor, after a loadout restore and
    /// after a float changes monitor, so that floats visible on the other
    /// monitors never drop out of tracking (B8).
    pub(super) fn rebuild_float_hwnds(&self) {
        // Fullscreen windows live in the floating spaces but are placed by
        // the daemon, never dragged: they are not tracked.
        let hwnds: Vec<isize> = float_hwnds_union(&self.monitors)
            .into_iter()
            .filter(|h| !self.is_fullscreen(*h))
            .collect();
        set_float_hwnds(&hwnds);
    }

    /// Suppress float-location tracking for the duration of one flow-initiated
    /// float animation.
    ///
    /// Turns `FLOAT_TRACKING_ACTIVE` off and arms `float_resume_deadline` to
    /// `now + animation duration + epsilon`. Resetting the deadline on each
    /// call means overlapping animations extend the suppression rather than
    /// letting an earlier timer expire mid-flight.
    ///
    /// Callers must ensure this only runs when animation is enabled: a 0 ms
    /// instant snap fires LOCATIONCHANGE at the final rect, so no suppression
    /// is needed.
    pub(super) fn arm_float_tracking_suppression(&mut self) {
        set_float_tracking_active(false);
        // Epsilon: one DwmFlush frame (~16 ms) of slack so the resume fires
        // just after the animator's last frame, not before it.
        const RESUME_EPSILON_MS: u64 = 16;
        let duration_ms = u64::from(self.config.animation.duration_ms);
        self.float_resume_deadline =
            Some(Instant::now() + Duration::from_millis(duration_ms + RESUME_EPSILON_MS));
        log::debug!(
            "float tracking suppressed: resume in {} ms ({} ms duration + {} ms epsilon)",
            duration_ms + RESUME_EPSILON_MS,
            duration_ms,
            RESUME_EPSILON_MS,
        );
    }

    /// Resume float-location tracking if the suppression deadline has passed.
    ///
    /// Polls every tracked float's true on-screen rect and stores it, then
    /// re-enables `FLOAT_TRACKING_ACTIVE`. Polling runs while the flag is
    /// still off, so no LOCATIONCHANGE can race the poll — the poll-then-
    /// enable ordering the design requires.
    ///
    /// No-op when no suppression is armed or the deadline hasn't arrived.
    pub(super) fn maybe_resume_float_tracking(&mut self) {
        let Some(deadline) = self.float_resume_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }

        let hwnds = float_hwnds_snapshot();
        log::debug!(
            "float tracking resume: polling {} float window(s) before re-enabling",
            hwnds.len(),
        );
        for hwnd in hwnds {
            self.store_float_rect(hwnd);
        }

        set_float_tracking_active(true);
        self.float_resume_deadline = None;
        log::debug!("float tracking resumed");
    }
}

/// The HWNDs of every float on the *active* workspace of every monitor —
/// the contents of the float-tracking set. Parked workspaces' floats are
/// excluded (they are off-screen and can never be dragged).
#[must_use]
pub(super) fn float_hwnds_union(monitors: &[Monitor]) -> Vec<isize> {
    monitors
        .iter()
        .flat_map(|m| m.active_workspace().floating.windows().iter())
        .map(|e| e.window_id.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::WindowRulesConfig;
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::registry::WindowRegistry;
    use crate::registry::win32 as registry_win32;
    use crate::workspace::{ScrollingSpace, Workspace, WorkspaceId};
    use windows::Win32::Foundation::HWND;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// A monitor at `x` with two workspaces (ids 1 and 2), workspace 1 active.
    fn monitor_at(x: i32) -> Monitor {
        let screen = rect(x, 0, 1920, 1080);
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let ws = |id: u32| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(MonitorInfo::single(screen), 960, 320, 100, 100, padding, 4),
            )
        };
        Monitor::new(screen, screen, vec![ws(1), ws(2)], 0)
    }

    /// Register `hwnd` as an active float in the registry (no Win32).
    fn float_window(registry: &mut WindowRegistry, hwnd: isize, r: Rect) {
        let info = registry_win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: "Float".into(),
            class: "FloatClass".into(),
            rect: r,
            exe: "float.exe".into(),
            process_path: "C:\\float.exe".into(),
            is_visible: true,
            is_maximized: false,
            is_fullscreen: false,
            is_iconic: false,
        };
        registry.register_window_from_info(&info);
        if let Some(w) = registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = WindowState::Floating(FloatingState::Active { rect: r });
        }
    }

    #[test]
    fn union_covers_every_monitors_active_workspace_only() {
        let mut m0 = monitor_at(0);
        let mut m1 = monitor_at(1920);
        m0.active_workspace_mut()
            .floating
            .add(WindowId(1), rect(0, 0, 10, 10));
        m1.active_workspace_mut()
            .floating
            .add(WindowId(2), rect(2000, 0, 10, 10));
        // Parked workspace float on monitor 0: never tracked.
        m0.workspace_mut(WorkspaceId(2))
            .expect("ws 2")
            .floating
            .add(WindowId(3), rect(0, 0, 10, 10));

        let mut union = float_hwnds_union(&[m0, m1]);
        union.sort_unstable();
        assert_eq!(union, vec![1, 2]);
    }

    #[test]
    fn union_of_no_floats_is_empty() {
        assert!(float_hwnds_union(&[monitor_at(0), monitor_at(1920)]).is_empty());
    }

    #[test]
    fn rehoming_the_focused_float_moves_the_active_monitor_with_it() {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        float_window(&mut registry, 0x11, rect(100, 100, 400, 300));
        float_window(&mut registry, 0x12, rect(100, 500, 400, 300));
        registry.set_focused(0x11);
        let mut flow = FlowWM::new_for_test(registry, vec![monitor_at(0), monitor_at(1920)]);
        for hwnd in [0x11, 0x12] {
            flow.monitors[0]
                .active_workspace_mut()
                .floating
                .add(WindowId(hwnd), rect(100, 100, 400, 300));
            flow.remember_monitor(WindowId(hwnd), 0);
        }
        assert_eq!(flow.active_monitor, 0);
        // An unfocused float dragged over: the active monitor stays.
        assert!(flow.relocate_float(WindowId(0x12), rect(2500, 300, 400, 300)));
        assert_eq!(flow.active_monitor, 0);
        // The focused float dragged over: the active monitor follows.
        assert!(flow.relocate_float(WindowId(0x11), rect(2500, 300, 400, 300)));
        assert_eq!(flow.active_monitor, 1);
    }

    #[test]
    fn relocate_within_the_owning_monitor_updates_the_rect() {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        float_window(&mut registry, 0x11, rect(100, 100, 400, 300));
        let mut flow = FlowWM::new_for_test(registry, vec![monitor_at(0), monitor_at(1920)]);
        flow.monitors[0]
            .active_workspace_mut()
            .floating
            .add(WindowId(0x11), rect(100, 100, 400, 300));

        assert!(flow.relocate_float(WindowId(0x11), rect(300, 300, 400, 300)));
        let entry = &flow.monitors[0].active_workspace().floating.windows()[0];
        assert_eq!(entry.rect, rect(300, 300, 400, 300));
        assert_eq!(flow.monitor_index_containing(WindowId(0x11)), Some(0));
        assert_eq!(flow.registry.remembered_monitor(0x11), Some(0));
    }

    #[test]
    fn relocate_across_monitors_rehomes_the_float() {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        float_window(&mut registry, 0x11, rect(100, 100, 400, 300));
        let mut flow = FlowWM::new_for_test(registry, vec![monitor_at(0), monitor_at(1920)]);
        flow.monitors[0]
            .active_workspace_mut()
            .floating
            .add(WindowId(0x11), rect(100, 100, 400, 300));
        flow.remember_monitor(WindowId(0x11), 0);

        // Dragged so its centre is on monitor 1.
        let moved = rect(2500, 100, 400, 300);
        assert!(flow.relocate_float(WindowId(0x11), moved));
        assert!(flow.monitors[0].active_workspace().floating.is_empty());
        let entry = &flow.monitors[1].active_workspace().floating.windows()[0];
        assert_eq!(entry.window_id, WindowId(0x11));
        assert_eq!(entry.rect, moved);
        assert_eq!(flow.monitor_index_containing(WindowId(0x11)), Some(1));
        assert_eq!(flow.registry.remembered_monitor(0x11), Some(1));
    }

    #[test]
    fn relocate_ignores_windows_that_are_not_active_floats() {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        float_window(&mut registry, 0x11, rect(100, 100, 400, 300));
        let mut flow = FlowWM::new_for_test(registry, vec![monitor_at(0), monitor_at(1920)]);
        // Not in any FloatingSpace at all.
        assert!(!flow.relocate_float(WindowId(0x11), rect(300, 300, 400, 300)));
        // On a parked workspace: left alone.
        flow.monitors[0]
            .workspace_mut(WorkspaceId(2))
            .expect("ws 2")
            .floating
            .add(WindowId(0x11), rect(100, 100, 400, 300));
        assert!(!flow.relocate_float(WindowId(0x11), rect(2500, 300, 400, 300)));
        assert_eq!(flow.monitor_index_containing(WindowId(0x11)), Some(0));
    }

    #[test]
    fn relocate_with_a_parked_rect_keeps_the_owner() {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        float_window(&mut registry, 0x11, rect(100, 100, 400, 300));
        let mut flow = FlowWM::new_for_test(registry, vec![monitor_at(0), monitor_at(1920)]);
        flow.monitors[1]
            .active_workspace_mut()
            .floating
            .add(WindowId(0x11), rect(2100, 100, 400, 300));
        // A rect beyond every monitor (mid-animation parking) must not move
        // the float to the active monitor.
        assert!(flow.relocate_float(WindowId(0x11), rect(100, 9000, 400, 300)));
        assert_eq!(flow.monitor_index_containing(WindowId(0x11)), Some(1));
    }
}
