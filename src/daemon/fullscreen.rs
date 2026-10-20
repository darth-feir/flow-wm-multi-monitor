//! `set-window fullscreen` — a managed fullscreen mode for the focused window.
//!
//! A fullscreen window leaves the layout the way a float does and is placed
//! over its monitor's **full screen rect** (taskbar included), kept above the
//! tiles with `HWND_TOPMOST` and shown without a border. It stays a member of
//! its workspace — it sits in that workspace's [`FloatingSpace`] at the screen
//! rect, so a workspace switch parks and unparks it with the workspace like
//! any float — while [`FlowWM::fullscreen`] remembers where it came from
//! ([`FullscreenOrigin`]): the monitor, workspace, column, row and column
//! width of a tile, or the rect of a float. Issuing the command again puts
//! it back there (same column and row when that column still exists, else
//! appended; a float goes back to its rect).
//!
//! The registry records a fullscreen window as `Floating(Active { rect:
//! screen_rect })`; the daemon-side map is what tells it apart from a float:
//! no border ([`border_style_for`](super::FlowWM::border_style_for)), no
//! float-location tracking, `mode: "fullscreen"` in `flow query all`, and
//! no inset for the border ring in the animation bridge.
//!
//! Lifecycle: closing the window drops its origin with it; minimizing or
//! hiding keeps the origin so the window comes back fullscreen; `tiling off`
//! turns every fullscreen window into a plain float inside its work area
//! (the origin is forgotten, so `tiling on` treats it as a float); the
//! shutdown rescue clears the topmost flag.

use windows::Win32::Foundation::HWND;

use crate::common::{Rect, WindowId};
use crate::ipc::message::SocketResponse;
use crate::registry::hooks::remove_float_hwnd;
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32 as registry_win32;
use crate::workspace::{WorkspaceId, WorkspaceParking};

use super::animation::PrevRects;
use super::types::FlowWM;

/// Where a fullscreen window came from, so it can be put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FullscreenOrigin {
    /// It was a tile at `(col, row)` of `workspace` on `monitor`, in a
    /// column `width_px` wide that it had to itself when `alone`.
    Tile {
        monitor: usize,
        workspace: WorkspaceId,
        col: usize,
        row: usize,
        width_px: i32,
        alone: bool,
    },
    /// It was a float at `rect` on `workspace` of `monitor`.
    Float {
        monitor: usize,
        workspace: WorkspaceId,
        rect: Rect,
    },
}

impl FullscreenOrigin {
    /// The monitor the window came from.
    #[must_use]
    pub(super) fn monitor(self) -> usize {
        match self {
            Self::Tile { monitor, .. } | Self::Float { monitor, .. } => monitor,
        }
    }
}

/// Where `set-window` sends a window that leaves fullscreen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FullscreenExit {
    /// Back where it came from (`set-window fullscreen` again).
    Origin,
    /// Into the strip (`set-window tile` / `cycle`): its old seat when it
    /// was a tile there, else after the strip's last focused window.
    Tile,
    /// Floating (`set-window float`): its old rect when it was a float,
    /// else centred.
    Float,
}

impl FlowWM {
    /// Whether `hwnd` is a window the daemon put fullscreen.
    #[must_use]
    pub(super) fn is_fullscreen(&self, hwnd: isize) -> bool {
        self.fullscreen.contains_key(&hwnd)
    }

    /// Forget `hwnd`'s fullscreen origin (the window is gone or became a
    /// plain float). Clears the topmost flag when asked.
    pub(super) fn forget_fullscreen(&mut self, hwnd: isize, clear_topmost: bool) {
        if self.fullscreen.remove(&hwnd).is_some() && clear_topmost {
            let _ = registry_win32::set_topmost(hwnd, false);
        }
    }

    /// Put the focused tile or float fullscreen on its monitor.
    pub(super) fn set_window_to_fullscreen(&mut self, focused: WindowId) -> SocketResponse {
        let hwnd = focused.0;
        let Some(state) = self
            .registry
            .get_window(HWND(hwnd as *mut _))
            .map(|w| w.state.clone())
        else {
            return SocketResponse::Error {
                message: "focused window not found in registry".into(),
            };
        };
        let idx = self.monitor_index_for_window(focused);
        self.set_active_monitor(idx);
        let ws_id = self.monitors[idx]
            .find_workspace_containing(focused)
            .unwrap_or_else(|| self.monitors[idx].active_workspace_id());
        let offset = self.parking_offset(idx, ws_id);

        let mut scroll_applied = None;
        let origin = match state {
            WindowState::Tiling(TilingState::Active { .. }) => {
                let Some(ws) = self.monitors[idx].workspace_mut(ws_id) else {
                    return SocketResponse::Error {
                        message: "workspace disappeared mid-dispatch".into(),
                    };
                };
                let vl = ws.scrolling.virtual_layout();
                let Some((col, row)) = vl.find_window(focused) else {
                    return SocketResponse::Error {
                        message: "focused window is not in its strip".into(),
                    };
                };
                let width_px = vl.columns[col].width_px;
                let alone = vl.columns[col].rows.len() == 1;
                scroll_applied = Some(ws.scrolling.remove_window(focused));
                FullscreenOrigin::Tile {
                    monitor: idx,
                    workspace: ws_id,
                    col,
                    row,
                    width_px,
                    alone,
                }
            }
            WindowState::Floating(FloatingState::Active { rect }) => {
                self.remove_float_from_all_spaces(focused);
                remove_float_hwnd(hwnd);
                FullscreenOrigin::Float {
                    monitor: idx,
                    workspace: ws_id,
                    rect,
                }
            }
            other => {
                return SocketResponse::Error {
                    message: format!(
                        "window is in state {other:?} (only active tiling/floating can go fullscreen)"
                    ),
                };
            }
        };

        let screen = self.monitors[idx].screen_rect();
        let float_actual = self.seat_fullscreen(focused, idx, ws_id, screen);
        self.fullscreen.insert(hwnd, origin);
        if !registry_win32::set_topmost(hwnd, true) {
            log::debug!("fullscreen: could not set hwnd {hwnd} topmost");
        }
        log::info!(
            "fullscreen: hwnd {hwnd} -> monitor {idx} ({},{},{},{}) from {origin:?}",
            screen.x,
            screen.y,
            screen.width,
            screen.height
        );

        let mut batches = Vec::with_capacity(2);
        if let Some(applied) = &scroll_applied {
            batches.push((applied.actual_layout.clone(), offset));
        }
        batches.push((float_actual, offset));
        self.animate_workspaces_on(idx, &batches, PrevRects::Registry);
        if let Some(applied) = &scroll_applied {
            self.registry
                .update_tiling_slots_from_layout(&applied.virtual_layout);
            self.registry.update_tiled_rects(&applied.actual_layout);
        }
        // No overlay for a fullscreen window: the refresh drops it.
        self.refresh_border_for(hwnd);
        SocketResponse::Ok
    }

    /// Take `focused` out of fullscreen: back to its origin, into the strip
    /// or into a float, per `exit`.
    pub(super) fn restore_from_fullscreen(
        &mut self,
        focused: WindowId,
        exit: FullscreenExit,
    ) -> SocketResponse {
        let hwnd = focused.0;
        let Some(origin) = self.fullscreen.get(&hwnd).copied() else {
            return SocketResponse::Error {
                message: "window is not fullscreen".into(),
            };
        };
        if !matches!(
            self.registry
                .get_window(HWND(hwnd as *mut _))
                .map(|w| &w.state),
            Some(WindowState::Floating(FloatingState::Active { .. }))
        ) {
            return SocketResponse::Error {
                message: "fullscreen window is minimized or hidden; restore it first".into(),
            };
        }
        // Where it is now (it may have been moved to another monitor while
        // fullscreen), else where it came from.
        let idx = self
            .monitor_index_containing(focused)
            .unwrap_or_else(|| origin.monitor().min(self.monitors.len() - 1));
        let ws_id = self.monitors[idx]
            .find_workspace_containing(focused)
            .unwrap_or_else(|| self.monitors[idx].active_workspace_id());
        let offset = self.parking_offset(idx, ws_id);
        self.set_active_monitor(idx);

        self.remove_float_from_all_spaces(focused);
        self.fullscreen.remove(&hwnd);
        if !registry_win32::set_topmost(hwnd, false) {
            log::debug!("fullscreen: could not clear topmost on hwnd {hwnd}");
        }

        let to_float = match (exit, origin) {
            (FullscreenExit::Float, _) => true,
            (FullscreenExit::Tile, _) => false,
            (FullscreenExit::Origin, FullscreenOrigin::Float { .. }) => true,
            (FullscreenExit::Origin, FullscreenOrigin::Tile { .. }) => false,
        };

        if to_float {
            let work_area = self.monitors[idx].work_area();
            let rect = match origin {
                FullscreenOrigin::Float { rect, .. } => rect.clamped_into(work_area),
                FullscreenOrigin::Tile { .. } => self.centered_float_rect(focused),
            };
            // `register_float` seats by the rect's monitor and arms tracking.
            let float_actual = self.register_float(focused, rect);
            self.animate_workspaces_on(idx, &[(float_actual, offset)], PrevRects::Registry);
            self.refresh_border_for(hwnd);
            log::info!("fullscreen: hwnd {hwnd} back to a float at {rect:?}");
            return SocketResponse::Ok;
        }

        // Into the strip of the workspace it is on: its old seat when that
        // is where it came from, else after the strip's last focused window.
        let presets = self.config.columns.is_some();
        let Some(ws) = self.monitors[idx].workspace_mut(ws_id) else {
            return SocketResponse::Error {
                message: "workspace disappeared mid-dispatch".into(),
            };
        };
        let applied = match origin {
            FullscreenOrigin::Tile {
                monitor,
                workspace,
                col,
                row,
                width_px,
                alone,
            } if monitor == idx && workspace == ws_id => {
                // Widths follow the monitor: with presets the strip's
                // current base width wins over the remembered one.
                let width = if presets { None } else { Some(width_px) };
                ws.scrolling
                    .restore_window_at(col, row, alone, focused, width)
            }
            _ => ws.scrolling.insert_window(focused),
        };
        self.remember_monitor(focused, idx);
        let float_actual = self.monitors[idx]
            .workspace(ws_id)
            .map(|ws| ws.floating.to_actual_layout())
            .unwrap_or_default();
        self.animate_workspaces_on(
            idx,
            &[
                (applied.actual_layout.clone(), offset),
                (float_actual, offset),
            ],
            PrevRects::Registry,
        );
        self.registry
            .update_tiling_slots_from_layout(&applied.virtual_layout);
        self.registry.update_tiled_rects(&applied.actual_layout);
        self.refresh_border_for(hwnd);
        log::info!("fullscreen: hwnd {hwnd} back in the strip of monitor {idx}");
        SocketResponse::Ok
    }

    /// Seat `window` in `ws_id`'s floating space on monitor `idx` at `rect`
    /// (the screen rect) and mirror it into the registry — without the
    /// float-location tracking a real float gets. Returns the floating
    /// layout of that workspace.
    fn seat_fullscreen(
        &mut self,
        window: WindowId,
        idx: usize,
        ws_id: WorkspaceId,
        rect: Rect,
    ) -> crate::layout::types::ActualLayout {
        if let Some(ws) = self.monitors[idx].workspace_mut(ws_id) {
            ws.floating.add(window, rect);
        }
        if let Some(w) = self.registry.get_window_mut(HWND(window.0 as *mut _)) {
            w.state = WindowState::Floating(FloatingState::Active { rect });
        }
        self.remember_monitor(window, idx);
        self.monitors[idx]
            .workspace(ws_id)
            .map(|ws| ws.floating.to_actual_layout())
            .unwrap_or_default()
    }

    /// The vertical parking offset of `ws_id` on monitor `idx` (0 when it
    /// is the active workspace).
    fn parking_offset(&self, idx: usize, ws_id: WorkspaceId) -> i32 {
        let m = &self.monitors[idx];
        let gap = m.active_scrolling().padding().window_gap;
        WorkspaceParking::new(m.active_scrolling().monitor(), m.screen_rect(), gap)
            .offset(ws_id, m.active_workspace_id())
    }

    /// A fullscreen window came back from minimized / hidden: the registry
    /// restored it as a float at its pre-manage rect, so put the screen
    /// rect and the topmost flag back. No-op for other windows.
    pub(super) fn reassert_fullscreen(&mut self, hwnd: isize) {
        if !self.fullscreen.contains_key(&hwnd) {
            return;
        }
        let wid = WindowId(hwnd);
        let Some(idx) = self.monitor_index_containing(wid) else {
            return;
        };
        let Some(ws_id) = self.monitors[idx].find_workspace_containing(wid) else {
            return;
        };
        let screen = self.monitors[idx].screen_rect();
        let offset = self.parking_offset(idx, ws_id);
        let float_actual = self.seat_fullscreen(wid, idx, ws_id, screen);
        let _ = registry_win32::set_topmost(hwnd, true);
        self.animate_workspaces_on(idx, &[(float_actual, offset)], PrevRects::Registry);
    }

    /// Carry a fullscreen window to monitor `target`: it stays fullscreen,
    /// now over that monitor's screen rect, on its active workspace. Its
    /// origin is kept, so leaving fullscreen on the new monitor appends it
    /// to that strip (the old seat is on another monitor).
    pub(super) fn move_fullscreen_to_monitor(
        &mut self,
        id: WindowId,
        from: usize,
        target: usize,
    ) -> SocketResponse {
        if from == target {
            return SocketResponse::Ok;
        }
        self.remove_float_from_all_spaces(id);
        self.set_active_monitor(target);
        let ws_id = self.monitors[target].active_workspace_id();
        let screen = self.monitors[target].screen_rect();
        let float_actual = self.seat_fullscreen(id, target, ws_id, screen);
        self.animate_workspaces_on(target, &[(float_actual, 0)], PrevRects::Registry);
        if !registry_win32::set_foreground_window(id.0) {
            log::warn!(
                "move_fullscreen_to_monitor: SetForegroundWindow failed for hwnd {}",
                id.0
            );
        }
        self.registry.set_focused(id.0);
        SocketResponse::Ok
    }

    /// Every fullscreen window becomes a plain float inside its monitor's
    /// work area: origin forgotten, topmost cleared, float rect and registry
    /// mirror set to the clamped rect. Returns `(hwnd, visible rect)` for
    /// each so the caller can move the window there. Used by `tiling off`
    /// (the resume then sees ordinary floats) and the shutdown rescue.
    pub(super) fn demote_all_fullscreen_to_floats(&mut self) -> Vec<(isize, Rect)> {
        let hwnds: Vec<isize> = self.fullscreen.keys().copied().collect();
        let mut out = Vec::with_capacity(hwnds.len());
        for hwnd in hwnds {
            self.fullscreen.remove(&hwnd);
            let _ = registry_win32::set_topmost(hwnd, false);
            let wid = WindowId(hwnd);
            let Some(idx) = self.monitor_index_containing(wid) else {
                continue;
            };
            let work_area = self.monitors[idx].work_area();
            let Some(ws_id) = self.monitors[idx].find_workspace_containing(wid) else {
                continue;
            };
            let Some(ws) = self.monitors[idx].workspace_mut(ws_id) else {
                continue;
            };
            let Some(current) = ws
                .floating
                .windows()
                .iter()
                .find(|e| e.window_id == wid)
                .map(|e| e.rect)
            else {
                continue;
            };
            let rect = current.clamped_into(work_area);
            ws.floating.add(wid, rect);
            if let Some(w) = self.registry.get_window_mut(HWND(hwnd as *mut _))
                && matches!(w.state, WindowState::Floating(FloatingState::Active { .. }))
            {
                w.state = WindowState::Floating(FloatingState::Active { rect });
            }
            out.push((hwnd, rect));
        }
        out
    }

    /// After a display change, put every fullscreen window back over the
    /// full screen rect of the monitor it now sits on (the reconcile
    /// clamps floats into the work area).
    pub(super) fn reseat_fullscreen_after_hotplug(&mut self) {
        let hwnds: Vec<isize> = self.fullscreen.keys().copied().collect();
        for hwnd in hwnds {
            let wid = WindowId(hwnd);
            let Some(idx) = self.monitor_index_containing(wid) else {
                self.fullscreen.remove(&hwnd);
                let _ = registry_win32::set_topmost(hwnd, false);
                continue;
            };
            let Some(ws_id) = self.monitors[idx].find_workspace_containing(wid) else {
                continue;
            };
            let screen = self.monitors[idx].screen_rect();
            let _ = self.seat_fullscreen(wid, idx, ws_id, screen);
        }
    }

    /// `"fullscreen"`, `"tiled"`, `"floating"`, `"minimized"`, `"hidden"` or
    /// `"ignored"` for `flow query all`.
    #[must_use]
    pub(super) fn window_mode(&self, hwnd: isize) -> &'static str {
        if self.is_fullscreen(hwnd) {
            return "fullscreen";
        }
        match self
            .registry
            .get_window(HWND(hwnd as *mut _))
            .map(|w| &w.state)
        {
            Some(WindowState::Tiling(TilingState::Active { .. })) => "tiled",
            Some(WindowState::Floating(FloatingState::Active { .. })) => "floating",
            Some(
                WindowState::Tiling(TilingState::Minimized)
                | WindowState::Floating(FloatingState::Minimized),
            ) => "minimized",
            Some(
                WindowState::Tiling(TilingState::Hidden)
                | WindowState::Floating(FloatingState::Hidden),
            ) => "hidden",
            Some(WindowState::Ignored(_)) => "ignored",
            None => "untracked",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::WindowRulesConfig;
    use crate::ipc::message::{SocketMessage, WindowMode};
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::registry::WindowRegistry;
    use crate::workspace::{Monitor, ScrollingSpace, Workspace};

    const GAP: i32 = 16;
    const A: isize = 0xA;
    const B: isize = 0xB;
    const C: isize = 0xC;
    const F: isize = 0xF;

    const PRIMARY: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    const PRIMARY_WORK: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1040,
    };
    const LEFT: Rect = Rect {
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
    };
    const F_RECT: Rect = Rect {
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    };

    fn fake_window(registry: &mut WindowRegistry, hwnd: isize, state: WindowState) {
        let info = crate::registry::win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: format!("win{hwnd}"),
            class: "C".into(),
            rect: Rect {
                x: 50,
                y: 60,
                width: 800,
                height: 600,
            },
            exe: "app.exe".into(),
            process_path: "C:\\app.exe".into(),
            is_visible: true,
            is_maximized: false,
            is_fullscreen: false,
        };
        registry.register_window_from_info(&info);
        if let Some(w) = registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = state;
        }
    }

    fn tile() -> WindowState {
        WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
    }

    fn monitor(screen: Rect, work: Rect, others: &[Rect]) -> Monitor {
        let vs = Rect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        let info = MonitorInfo::in_desktop(work, screen, vs, others);
        let ws = |id: u32| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(
                    info,
                    936,
                    320,
                    100,
                    100,
                    Padding {
                        window_gap: GAP,
                        up: 0,
                        down: 0,
                    },
                    4,
                ),
            )
        };
        Monitor::new(screen, work, vec![ws(1), ws(2)], 0)
    }

    /// Primary (with a 40 px taskbar) tiles A, B (stacked in column 0) and
    /// C (column 1) on workspace 1, float F; LEFT is empty. Borders off so
    /// the focus bookkeeping never creates an overlay window.
    fn fixture() -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [A, B, C] {
            fake_window(&mut registry, hwnd, tile());
        }
        fake_window(
            &mut registry,
            F,
            WindowState::Floating(FloatingState::Active { rect: F_RECT }),
        );
        let mut m0 = monitor(PRIMARY, PRIMARY_WORK, &[LEFT]);
        let m1 = monitor(LEFT, LEFT, &[PRIMARY]);
        m0.active_scrolling_mut().add_window(WindowId(A));
        m0.active_scrolling_mut().add_window(WindowId(C));
        // B joins A's column below it.
        let _ = m0
            .active_scrolling_mut()
            .restore_window_at(0, 1, false, WindowId(B), None);
        m0.active_workspace_mut().floating.add(WindowId(F), F_RECT);
        for hwnd in [A, B, C, F] {
            registry.set_monitor(hwnd, 0);
        }
        registry.set_focused(A);
        let mut flow = FlowWM::new_for_test(registry, vec![m0, m1]);
        flow.config.borders.enabled = false;
        flow.config.padding.window_gap = GAP;
        flow
    }

    fn strip(flow: &FlowWM, mi: usize) -> crate::layout::types::VirtualLayout {
        flow.monitors[mi]
            .active_scrolling()
            .virtual_layout()
            .clone()
    }

    fn float_rect(flow: &FlowWM, mi: usize, hwnd: isize) -> Option<Rect> {
        flow.monitors[mi]
            .active_workspace()
            .floating
            .windows()
            .iter()
            .find(|e| e.window_id == WindowId(hwnd))
            .map(|e| e.rect)
    }

    fn registry_rect(flow: &FlowWM, hwnd: isize) -> Option<Rect> {
        match flow
            .registry
            .get_window(HWND(hwnd as *mut _))
            .map(|w| &w.state)
        {
            Some(WindowState::Floating(FloatingState::Active { rect })) => Some(*rect),
            _ => None,
        }
    }

    fn set_window(flow: &mut FlowWM, hwnd: isize, mode: WindowMode) -> SocketResponse {
        flow.registry.set_focused(hwnd);
        flow.dispatch(&SocketMessage::SetWindow { mode })
    }

    #[test]
    fn a_tile_goes_fullscreen_over_the_full_screen_rect_and_comes_back_to_its_seat() {
        let mut flow = fixture();
        assert_eq!(strip(&flow, 0).find_window(WindowId(B)), Some((0, 1)));

        assert_eq!(
            set_window(&mut flow, B, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        // Out of the strip, into the floating space at the SCREEN rect (not
        // the work area), recorded as a float, known as fullscreen.
        let vl = strip(&flow, 0);
        assert_eq!(vl.find_window(WindowId(B)), None);
        assert_eq!(vl.columns.len(), 2);
        assert_eq!(float_rect(&flow, 0, B), Some(PRIMARY));
        assert_eq!(registry_rect(&flow, B), Some(PRIMARY));
        assert!(flow.is_fullscreen(B));
        assert_eq!(flow.window_mode(B), "fullscreen");
        assert_eq!(flow.window_mode(A), "tiled");
        assert_eq!(flow.window_mode(F), "floating");
        assert!(flow.border_style_for(B).is_none());
        assert_eq!(
            flow.fullscreen.get(&B),
            Some(&FullscreenOrigin::Tile {
                monitor: 0,
                workspace: WorkspaceId(1),
                col: 0,
                row: 1,
                width_px: 936,
                alone: false,
            })
        );
        // The query reports it.
        let SocketResponse::Data { payload } = flow.query_windows_all() else {
            panic!("query");
        };
        let entry = payload["windows"]
            .as_array()
            .expect("windows")
            .iter()
            .find(|w| w["hwnd"] == B as i64)
            .expect("B listed");
        assert_eq!(entry["mode"], "fullscreen");

        // Again: back to column 0, row 1, as a tile.
        assert_eq!(
            set_window(&mut flow, B, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert!(!flow.is_fullscreen(B));
        assert_eq!(float_rect(&flow, 0, B), None);
        let vl = strip(&flow, 0);
        assert_eq!(vl.find_window(WindowId(B)), Some((0, 1)));
        assert_eq!(vl.find_window(WindowId(A)), Some((0, 0)));
        assert!(matches!(
            flow.registry
                .get_window(HWND(B as *mut _))
                .map(|w| &w.state),
            Some(WindowState::Tiling(TilingState::Active { col: 0, row: 1 }))
        ));
        assert_eq!(flow.window_mode(B), "tiled");
    }

    #[test]
    fn a_lone_column_comes_back_as_its_own_column_or_is_appended_when_the_slot_is_gone() {
        let mut flow = fixture();
        // C is alone in column 1.
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(strip(&flow, 0).columns.len(), 1);
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let vl = strip(&flow, 0);
        assert_eq!(vl.columns.len(), 2);
        assert_eq!(vl.find_window(WindowId(C)), Some((1, 0)));
        assert_eq!(vl.columns[1].width_px, 936);

        // Fullscreen A (shares column 0 with B); meanwhile B and C leave, so
        // column 0 no longer has room... then A comes back appended.
        assert_eq!(
            set_window(&mut flow, A, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let _ = flow.monitors[0]
            .active_scrolling_mut()
            .remove_window(WindowId(B));
        let _ = flow.monitors[0]
            .active_scrolling_mut()
            .remove_window(WindowId(C));
        assert!(strip(&flow, 0).columns.is_empty());
        assert_eq!(
            set_window(&mut flow, A, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let vl = strip(&flow, 0);
        assert_eq!(vl.columns.len(), 1);
        assert_eq!(vl.find_window(WindowId(A)), Some((0, 0)));
    }

    #[test]
    fn a_float_goes_fullscreen_and_comes_back_to_its_rect() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, F, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(float_rect(&flow, 0, F), Some(PRIMARY));
        assert!(flow.is_fullscreen(F));
        assert!(!crate::registry::hooks::is_float_hwnd(F));
        // The strip is untouched.
        assert_eq!(strip(&flow, 0).window_count(), 3);

        assert_eq!(
            set_window(&mut flow, F, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert!(!flow.is_fullscreen(F));
        assert_eq!(float_rect(&flow, 0, F), Some(F_RECT));
        assert_eq!(registry_rect(&flow, F), Some(F_RECT));
        assert_eq!(flow.window_mode(F), "floating");
        assert_eq!(strip(&flow, 0).window_count(), 3);
    }

    #[test]
    fn cycle_and_explicit_modes_leave_fullscreen_as_documented() {
        let mut flow = fixture();
        // cycle on a fullscreen float-origin window: tiled, after the focus
        // cursor — not back to a float.
        assert_eq!(
            set_window(&mut flow, F, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(
            set_window(&mut flow, F, WindowMode::Cycle),
            SocketResponse::Ok
        );
        assert!(strip(&flow, 0).find_window(WindowId(F)).is_some());
        assert_eq!(flow.window_mode(F), "tiled");
        assert_eq!(float_rect(&flow, 0, F), None);

        // `set-window float` on a fullscreen tile-origin window: a float.
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Float),
            SocketResponse::Ok
        );
        assert_eq!(flow.window_mode(C), "floating");
        assert!(strip(&flow, 0).find_window(WindowId(C)).is_none());
        let rect = float_rect(&flow, 0, C).expect("C floats");
        assert_ne!(rect, PRIMARY);
        assert!(PRIMARY_WORK.overlaps(rect));

        // `set-window tile` on a fullscreen tile-origin window: its seat.
        assert_eq!(
            set_window(&mut flow, A, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(
            set_window(&mut flow, A, WindowMode::Tile),
            SocketResponse::Ok
        );
        assert_eq!(strip(&flow, 0).find_window(WindowId(A)), Some((0, 0)));

        // Tiled-only commands are silent no-ops on a fullscreen window.
        assert_eq!(
            set_window(&mut flow, A, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(
            flow.dispatch(&SocketMessage::ExpandColumn),
            SocketResponse::Ok
        );
        assert_eq!(flow.dispatch(&SocketMessage::FocusLeft), SocketResponse::Ok);
        assert!(flow.is_fullscreen(A));
    }

    #[test]
    fn closing_minimizing_and_restoring_leave_nothing_stale() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, B, WindowMode::Fullscreen),
            SocketResponse::Ok
        );

        // Minimize: still remembered; restore: fullscreen again (the
        // registry's generic restore would have put it at its pre-manage
        // rect).
        flow.on_window_minimized(B);
        assert!(flow.is_fullscreen(B));
        assert_eq!(flow.window_mode(B), "fullscreen");
        flow.on_window_restored(B);
        assert_eq!(registry_rect(&flow, B), Some(PRIMARY));
        assert_eq!(float_rect(&flow, 0, B), Some(PRIMARY));
        assert!(flow.is_fullscreen(B));

        // Destroyed: gone from every space and from the fullscreen map.
        flow.on_window_destroyed(B);
        assert!(!flow.is_fullscreen(B));
        assert_eq!(float_rect(&flow, 0, B), None);
        assert!(!flow.registry.is_tracked(B));
        assert_eq!(strip(&flow, 0).window_count(), 2);
    }

    #[test]
    fn fullscreen_windows_park_with_their_workspace_and_leave_via_tiling_off() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        // Workspace switch (layout only): it stays in workspace 1's
        // floating space, still fullscreen.
        assert!(flow.switch_workspace_layout(WorkspaceId(2), WorkspaceId(1)));
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(2));
        assert_eq!(
            flow.monitors[0].find_workspace_containing(WindowId(C)),
            Some(WorkspaceId(1))
        );
        assert!(flow.is_fullscreen(C));
        assert!(flow.switch_workspace_layout(WorkspaceId(1), WorkspaceId(2)));

        // Tiling off: a plain float inside the work area, origin forgotten.
        let demoted = flow.demote_all_fullscreen_to_floats();
        assert_eq!(demoted.len(), 1);
        assert_eq!(demoted[0].0, C);
        assert_eq!(demoted[0].1, PRIMARY_WORK);
        assert!(!flow.is_fullscreen(C));
        assert_eq!(float_rect(&flow, 0, C), Some(PRIMARY_WORK));
        assert_eq!(registry_rect(&flow, C), Some(PRIMARY_WORK));
        assert_eq!(flow.window_mode(C), "floating");
    }

    #[test]
    fn move_to_monitor_keeps_it_fullscreen_there_and_leaving_appends_to_that_strip() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(
            flow.dispatch(&SocketMessage::MoveWindowToMonitor {
                direction: crate::common::Direction::Left,
            }),
            SocketResponse::Ok
        );
        assert_eq!(float_rect(&flow, 0, C), None);
        assert_eq!(float_rect(&flow, 1, C), Some(LEFT));
        assert_eq!(registry_rect(&flow, C), Some(LEFT));
        assert!(flow.is_fullscreen(C));
        assert_eq!(flow.active_monitor, 1);

        // Leaving on the other monitor: its old seat is elsewhere, so it is
        // appended to this strip.
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert!(!flow.is_fullscreen(C));
        assert_eq!(strip(&flow, 1).find_window(WindowId(C)), Some((0, 0)));
        assert_eq!(strip(&flow, 0).find_window(WindowId(C)), None);
        assert_eq!(flow.registry.remembered_monitor(C), Some(1));
    }

    #[test]
    fn with_presets_a_restored_tile_takes_the_monitors_current_preset_width() {
        let mut flow = fixture();
        flow.config.min_column_width_px = 320;
        flow.config.columns =
            Some(toml::from_str("presets = [\"1/3\", \"1/2\", \"1/1\"]").expect("columns"));
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        // The monitor moves to 1/3 while C is fullscreen.
        flow.apply_width_preset(0, 0);
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let vl = strip(&flow, 0);
        let (col, _) = vl.find_window(WindowId(C)).expect("C back");
        assert_eq!(vl.columns[col].width_px, 618);
    }
}
