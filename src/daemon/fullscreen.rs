//! `set-window fullscreen` — fullscreen is a column of the strip.
//!
//! The focused window's column is flagged fullscreen
//! ([`Column::fullscreen`](crate::layout::types::Column::fullscreen)): while
//! it starts the view it covers the monitor's full screen rect, and
//! scrolling or focusing off it shows the columns beside it under the
//! screen's width pattern while it is parked like any column. A stacked
//! window is expelled into a column of its own first and merged back on
//! the way out; a float joins the strip after the focused column and goes
//! back to floating on the way out. The window is kept above the tiles with
//! `HWND_TOPMOST` for its whole fullscreen life and shown without a border;
//! [`FlowWM::fullscreen`] remembers where it came from
//! ([`FullscreenOrigin`]) and is the authority on which windows are
//! fullscreen (the column flag follows it). Why, and the projection and pin
//! rules, are in ADR-0021 (`docs/adr/0021-fullscreen-is-a-column.md`);
//! the lifecycle is in `docs/src/dev-guide/floating-space.md`,
//! *Fullscreen windows*.
//!
//! A window under a `fullscreen = "app"` rule is put fullscreen by the app
//! itself (`SC_MAXIMIZE` / `SC_RESTORE`): the same column and bookkeeping,
//! but the daemon never sizes its frame, only moves it, and the app's own
//! switches are adopted ([`app_fullscreen_step`]). Why, and the state
//! machine, are in ADR-0020 (`docs/adr/0020-app-fullscreen.md`).

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::common::{InvisibleBounds, Rect, WindowId};
use crate::config::types::FullscreenStyle;
use crate::ipc::message::SocketResponse;
use crate::layout::types::{ActualLayout, AppliedLayout};
use crate::registry::hooks::{remove_float_hwnd, set_app_fullscreen_watch};
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32 as registry_win32;
use crate::workspace::{WorkspaceId, WorkspaceParking};

use super::animation::PrevRects;
use super::types::FlowWM;

/// Where a fullscreen window came from, so it can be put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FullscreenOrigin {
    /// It was a tile of `workspace` on `monitor`, at row `row` of a stack
    /// that also held `stack` (`None`: it had its column to itself), with
    /// the strip's view at `viewport_offset`.
    Tile {
        monitor: usize,
        workspace: WorkspaceId,
        row: usize,
        stack: Option<WindowId>,
        viewport_offset: i32,
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

/// How long the daemon waits for an app to finish switching its own full
/// screen on or off after `SC_MAXIMIZE` / `SC_RESTORE` (ADR-0020).
pub(super) const APP_FULLSCREEN_WAIT: Duration = Duration::from_millis(2000);

/// What an app-fullscreen window's live state asks of the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AppFullscreenStep {
    /// The app went full screen by itself (title-bar maximize): enter
    /// FlowWM's fullscreen mode for it without asking it again.
    Adopt,
    /// The app left its full screen by itself (connection-bar restore,
    /// Ctrl+Alt+Break): leave fullscreen and re-seat it.
    Leave,
}

/// The decision behind [`FlowWM::sync_app_fullscreen`], pure: `known` =
/// the daemon has it in app fullscreen, `seatable` = it is an active tile
/// or float not in any fullscreen, `live_fullscreen` = it covers its
/// monitor without chrome, `chrome` = it has a caption / thick frame
/// (`None`: gone). Leaving is judged by the chrome alone, so the daemon's
/// own parking move of a fullscreen window (off its monitor, still
/// chromeless) never reads as a leave.
#[must_use]
pub(super) fn app_fullscreen_step(
    known: bool,
    seatable: bool,
    live_fullscreen: bool,
    chrome: Option<bool>,
) -> Option<AppFullscreenStep> {
    match (known, chrome) {
        (_, None) => None,
        (true, Some(true)) => Some(AppFullscreenStep::Leave),
        (false, Some(false)) if seatable && live_fullscreen => Some(AppFullscreenStep::Adopt),
        _ => None,
    }
}

/// The Win32 side of an app's own full screen, injectable for tests.
pub(super) trait AppFullscreenProbe {
    /// Post `SC_MAXIMIZE` (`enter`) or `SC_RESTORE`; `true` when queued.
    fn request(&self, hwnd: isize, enter: bool) -> bool;
    /// Wait (bounded by [`APP_FULLSCREEN_WAIT`]) until the window is full
    /// screen on its monitor (`enter`) or has its chrome back; `true` when
    /// it got there.
    fn wait(&self, hwnd: isize, enter: bool) -> bool;
    /// Covers its monitor's full rect without chrome.
    fn is_fullscreen_live(&self, hwnd: isize) -> bool;
    /// Has a caption or thick frame; `None` when the window is gone.
    fn has_chrome(&self, hwnd: isize) -> Option<bool>;
    /// Its invisible frame borders as they are now.
    fn invisible_bounds(&self, hwnd: isize) -> InvisibleBounds;
}

/// The production [`AppFullscreenProbe`]: plain Win32, polling every 15 ms.
pub(super) struct Win32AppFullscreen;

impl AppFullscreenProbe for Win32AppFullscreen {
    fn request(&self, hwnd: isize, enter: bool) -> bool {
        registry_win32::post_syscommand(hwnd, enter)
    }

    fn wait(&self, hwnd: isize, enter: bool) -> bool {
        let deadline = Instant::now() + APP_FULLSCREEN_WAIT;
        loop {
            let reached = if enter {
                self.is_fullscreen_live(hwnd)
            } else {
                self.has_chrome(hwnd) != Some(false)
            };
            if reached {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn is_fullscreen_live(&self, hwnd: isize) -> bool {
        registry_win32::is_fullscreen_on_its_monitor(hwnd)
    }

    fn has_chrome(&self, hwnd: isize) -> Option<bool> {
        registry_win32::has_chrome(hwnd)
    }

    fn invisible_bounds(&self, hwnd: isize) -> InvisibleBounds {
        registry_win32::get_invisible_bounds(HWND(hwnd as *mut _))
    }
}

/// Where `set-window` sends a window that leaves fullscreen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FullscreenExit {
    /// Back where it came from (`set-window fullscreen` again).
    Origin,
    /// Stays in the strip (`set-window tile` / `cycle`): merged back into
    /// its stack when it came from one.
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
        self.app_fullscreen.remove(&hwnd);
        if self.fullscreen.remove(&hwnd).is_some() && clear_topmost {
            let _ = registry_win32::set_topmost(hwnd, false);
        }
    }

    /// Whether `hwnd` is fullscreen by the app's own full screen (ADR-0020).
    #[must_use]
    pub(super) fn is_app_fullscreen(&self, hwnd: isize) -> bool {
        self.app_fullscreen.contains_key(&hwnd)
    }

    /// Whether a batch would send `hwnd` exactly where the app put its own
    /// full screen frame: an app in its own full screen owns its rect, so
    /// only a move somewhere new (parking) is sent for it (ADR-0020).
    #[must_use]
    pub(super) fn app_owns_rect(&self, hwnd: isize, visible: Rect) -> bool {
        self.is_app_fullscreen(hwnd) && self.last_submitted_rect(hwnd) == Some(visible)
    }

    /// Watch `hwnd` for the app switching its own full screen when a
    /// `fullscreen = "app"` rule governs it (the hook callback then
    /// forwards its location changes); stop watching otherwise.
    pub(super) fn update_app_fullscreen_watch(&self, hwnd: isize) {
        let app = self.registry.is_tracked(hwnd)
            && self.registry.fullscreen_style(hwnd) == FullscreenStyle::App;
        set_app_fullscreen_watch(hwnd, app);
    }

    /// Put the focused tile or float fullscreen on its monitor: by sizing
    /// its frame, or, under a `fullscreen = "app"` rule, by asking the app
    /// for its own full screen.
    pub(super) fn set_window_to_fullscreen(&mut self, focused: WindowId) -> SocketResponse {
        if self.registry.fullscreen_style(focused.0) == FullscreenStyle::App {
            return self.enter_app_fullscreen_with(focused, &Win32AppFullscreen, true);
        }
        self.enter_fullscreen(focused, None)
    }

    /// App fullscreen for `focused`: post `SC_MAXIMIZE` and wait for the
    /// app to get there when `request` (Mod+F), or take the app's own
    /// switch as it is (adoption). The daemon's bookkeeping is then that of
    /// any fullscreen window, minus every placement of the frame.
    pub(super) fn enter_app_fullscreen_with<P: AppFullscreenProbe>(
        &mut self,
        focused: WindowId,
        probe: &P,
        request: bool,
    ) -> SocketResponse {
        let hwnd = focused.0;
        let seatable = matches!(
            self.registry
                .get_window(HWND(hwnd as *mut _))
                .map(|w| &w.state),
            Some(
                WindowState::Tiling(TilingState::Active { .. })
                    | WindowState::Floating(FloatingState::Active { .. })
            )
        );
        if !seatable || self.is_fullscreen(hwnd) {
            // The generic path reports the state.
            return self.enter_fullscreen(focused, None);
        }
        set_app_fullscreen_watch(hwnd, true);
        if request {
            if !probe.request(hwnd, true) {
                return SocketResponse::Error {
                    message: "could not post SC_MAXIMIZE to the window".into(),
                };
            }
            if !probe.wait(hwnd, true) {
                log::warn!(
                    "fullscreen: hwnd {hwnd} did not switch to its own full screen within {} ms",
                    APP_FULLSCREEN_WAIT.as_millis()
                );
                return SocketResponse::Error {
                    message: format!(
                        "the app did not switch to its own full screen within {} ms",
                        APP_FULLSCREEN_WAIT.as_millis()
                    ),
                };
            }
        }
        // The borders of the captioned frame, for the way back.
        let windowed = self
            .registry
            .get_window(HWND(hwnd as *mut _))
            .map(|w| w.invisible_bounds)
            .unwrap_or_default();
        // Chromeless now: its frame has no invisible borders, so a parking
        // move by the screen rect lands exactly where the app put it.
        self.registry
            .set_invisible_bounds(hwnd, probe.invisible_bounds(hwnd));
        let response = self.enter_fullscreen(focused, Some(windowed));
        if response != SocketResponse::Ok {
            self.registry.set_invisible_bounds(hwnd, windowed);
            return response;
        }
        log::info!(
            "fullscreen: hwnd {hwnd} in the app's own full screen ({})",
            if request { "asked" } else { "adopted" }
        );
        response
    }

    /// The app full screen of `hwnd` ends (it is leaving fullscreen mode):
    /// post `SC_RESTORE` and wait for the chrome when it is still
    /// chromeless, re-measure its invisible borders, and forget the flag.
    fn leave_app_fullscreen_with<P: AppFullscreenProbe>(&mut self, hwnd: isize, probe: &P) {
        let Some(windowed) = self.app_fullscreen.remove(&hwnd) else {
            return;
        };
        if probe.has_chrome(hwnd) == Some(false) {
            if !probe.request(hwnd, false) {
                log::warn!("fullscreen: could not post SC_RESTORE to hwnd {hwnd}");
            } else if !probe.wait(hwnd, false) {
                log::warn!(
                    "fullscreen: hwnd {hwnd} still has no chrome {} ms after SC_RESTORE",
                    APP_FULLSCREEN_WAIT.as_millis()
                );
            }
        }
        // Right after the switch DWM may still report the chromeless frame
        // (no borders): the captioned frame's borders from before stand in.
        let measured = probe.invisible_bounds(hwnd);
        let bounds = if measured == InvisibleBounds::default() {
            windowed
        } else {
            measured
        };
        self.registry.set_invisible_bounds(hwnd, bounds);
        // The app put its frame back somewhere the daemon never sent it:
        // the next batch moves it from the registry rect.
        self.forget_submitted(hwnd);
    }

    /// A watched window's style or rect changed: adopt the app's own full
    /// screen, or follow it out of it ([`app_fullscreen_step`]). Cheap for
    /// every other window (one set lookup).
    pub(super) fn sync_app_fullscreen(&mut self, hwnd: isize) {
        self.sync_app_fullscreen_with(hwnd, &Win32AppFullscreen);
    }

    /// [`sync_app_fullscreen`](Self::sync_app_fullscreen) over `probe`.
    pub(super) fn sync_app_fullscreen_with<P: AppFullscreenProbe>(
        &mut self,
        hwnd: isize,
        probe: &P,
    ) {
        if self.tiling_paused {
            return;
        }
        let known = self.is_app_fullscreen(hwnd);
        if !known && !crate::registry::hooks::is_app_fullscreen_watched(hwnd) {
            return;
        }
        let seatable = !self.is_fullscreen(hwnd)
            && matches!(
                self.registry
                    .get_window(HWND(hwnd as *mut _))
                    .map(|w| &w.state),
                Some(
                    WindowState::Tiling(TilingState::Active { .. })
                        | WindowState::Floating(FloatingState::Active { .. })
                )
            );
        let chrome = probe.has_chrome(hwnd);
        // The live geometry is only asked when it can matter.
        let live = !known && seatable && chrome == Some(false) && probe.is_fullscreen_live(hwnd);
        match app_fullscreen_step(known, seatable, live, chrome) {
            Some(AppFullscreenStep::Adopt) => {
                log::info!("fullscreen: hwnd {hwnd} went full screen by itself; adopting");
                let _ = self.enter_app_fullscreen_with(WindowId(hwnd), probe, false);
            }
            Some(AppFullscreenStep::Leave) => {
                log::info!("fullscreen: hwnd {hwnd} left its own full screen; re-seating");
                let _ = self.restore_from_fullscreen_with(
                    WindowId(hwnd),
                    FullscreenExit::Origin,
                    probe,
                );
            }
            None => {}
        }
    }

    /// Put the focused tile or float fullscreen on its monitor: a tile's
    /// column is flagged (a stacked window expelled into its own column
    /// first), a float joins the strip after the focused column first; the
    /// view starts at the column. `app`: the app is there already, so its
    /// frame is never placed; it carries the captioned frame's invisible
    /// borders for the way back.
    fn enter_fullscreen(
        &mut self,
        focused: WindowId,
        app: Option<InvisibleBounds>,
    ) -> SocketResponse {
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

        let mut float_actual = None;
        let origin = match state {
            WindowState::Tiling(TilingState::Active { .. }) => {
                let Some(ws) = self.monitors[idx].workspace(ws_id) else {
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
                let stack = vl.columns[col]
                    .rows
                    .iter()
                    .map(|r| r.window_id)
                    .find(|&w| w != focused);
                FullscreenOrigin::Tile {
                    monitor: idx,
                    workspace: ws_id,
                    row,
                    stack,
                    viewport_offset: vl.viewport_offset,
                }
            }
            WindowState::Floating(FloatingState::Active { rect }) => {
                // Into the strip after its focused column; `tiling off`
                // puts it back where it floated.
                if let Some(w) = self.registry.get_window(HWND(hwnd as *mut _)) {
                    let anchor = w.invisible_bounds.visible_to_window(rect);
                    self.registry.note_pre_tiling_rect(hwnd, anchor);
                }
                self.remove_float_from_all_spaces(focused);
                remove_float_hwnd(hwnd);
                let Some(ws) = self.monitors[idx].workspace_mut(ws_id) else {
                    return SocketResponse::Error {
                        message: "workspace disappeared mid-dispatch".into(),
                    };
                };
                let _ = ws.scrolling.insert_window(focused);
                float_actual = Some(ws.floating.to_actual_layout());
                self.remember_monitor(focused, idx);
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

        let applied = self.monitors[idx]
            .workspace_mut(ws_id)
            .and_then(|ws| ws.scrolling.enter_fullscreen(focused));
        let Some(applied) = applied else {
            return SocketResponse::Error {
                message: "focused window is not in its strip".into(),
            };
        };
        self.fullscreen.insert(hwnd, origin);
        if let Some(windowed) = app {
            self.app_fullscreen.insert(hwnd, windowed);
        }
        if self.is_app_fullscreen(hwnd) {
            self.note_app_frame_at_screen(hwnd, idx, offset);
        }
        self.raise_fullscreen(hwnd);
        log::info!(
            "fullscreen: hwnd {hwnd} -> a fullscreen column on monitor {idx} from {origin:?}"
        );

        let mut batches: Vec<(ActualLayout, i32)> = vec![(applied.actual_layout.clone(), offset)];
        if let Some(floats) = float_actual {
            batches.push((floats, offset));
        }
        self.animate_workspaces_on(idx, &batches, PrevRects::Registry);
        self.registry
            .update_tiling_slots_from_layout(&applied.virtual_layout);
        self.registry.update_tiled_rects(&applied.actual_layout);
        // No overlay for a fullscreen window: the refresh drops it.
        self.refresh_border_for(hwnd);
        SocketResponse::Ok
    }

    /// An app in its own full screen put its frame over monitor `idx`'s
    /// screen rect (its workspace parked at `offset`): count that rect as
    /// sent, so no batch sizes the frame there (ADR-0020).
    pub(super) fn note_app_frame_at_screen(&mut self, hwnd: isize, idx: usize, offset: i32) {
        let screen = self.monitors[idx].screen_rect();
        self.record_submitted(&[(
            hwnd,
            Rect {
                y: screen.y + offset,
                ..screen
            },
        )]);
    }

    /// The window side of a column becoming fullscreen: out of the floating
    /// layer, into the topmost band.
    fn raise_fullscreen(&mut self, hwnd: isize) {
        // A float leaves the floating layer here (its original topmost
        // flag put back): from now on the fullscreen code owns the flag.
        self.sync_top_layer(hwnd);
        if !registry_win32::set_topmost(hwnd, true) {
            log::debug!("fullscreen: could not set hwnd {hwnd} topmost");
        }
    }

    /// Take `focused` out of fullscreen: back to its origin, into the strip
    /// or into a float, per `exit`.
    pub(super) fn restore_from_fullscreen(
        &mut self,
        focused: WindowId,
        exit: FullscreenExit,
    ) -> SocketResponse {
        self.restore_from_fullscreen_with(focused, exit, &Win32AppFullscreen)
    }

    /// [`restore_from_fullscreen`](Self::restore_from_fullscreen) over
    /// `probe`, through which an app full screen is ended first.
    pub(super) fn restore_from_fullscreen_with<P: AppFullscreenProbe>(
        &mut self,
        focused: WindowId,
        exit: FullscreenExit,
        probe: &P,
    ) -> SocketResponse {
        let hwnd = focused.0;
        let Some(origin) = self.fullscreen.get(&hwnd).copied() else {
            return SocketResponse::Error {
                message: "window is not fullscreen".into(),
            };
        };
        if !self.registry.is_tiling_active(hwnd) {
            return SocketResponse::Error {
                message: "fullscreen window is minimized or hidden; restore it first".into(),
            };
        }
        // Where it is now (it may have been moved to another monitor or
        // workspace while fullscreen), else where it came from.
        let idx = self
            .monitor_index_containing(focused)
            .unwrap_or_else(|| origin.monitor().min(self.monitors.len() - 1));
        let ws_id = self.monitors[idx]
            .find_workspace_containing(focused)
            .unwrap_or_else(|| self.monitors[idx].active_workspace_id());
        let offset = self.parking_offset(idx, ws_id);
        self.set_active_monitor(idx);

        // An app full screen ends first: the app gives its frame back
        // before the daemon seats it.
        self.leave_app_fullscreen_with(hwnd, probe);
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

        let Some(ws) = self.monitors[idx].workspace_mut(ws_id) else {
            return SocketResponse::Error {
                message: "workspace disappeared mid-dispatch".into(),
            };
        };

        if to_float {
            let strip = ws.scrolling.remove_window(focused);
            let work_area = self.monitors[idx].work_area();
            let rect = match origin {
                FullscreenOrigin::Float { rect, .. } => rect.clamped_into(work_area),
                FullscreenOrigin::Tile { .. } => self.centered_float_rect(focused),
            };
            // `register_float` seats by the rect's monitor and arms tracking.
            let float_actual = self.register_float(focused, rect);
            self.animate_workspaces_on(
                idx,
                &[
                    (strip.actual_layout.clone(), offset),
                    (float_actual, offset),
                ],
                PrevRects::Registry,
            );
            self.registry
                .update_tiling_slots_from_layout(&strip.virtual_layout);
            self.registry.update_tiled_rects(&strip.actual_layout);
            self.refresh_border_for(hwnd);
            log::info!("fullscreen: hwnd {hwnd} back to a float at {rect:?}");
            return SocketResponse::Ok;
        }

        // Stays in the strip: back into its stack and to the view it was
        // entered from when it is still on that strip.
        let (merge, viewport) = match origin {
            FullscreenOrigin::Tile {
                monitor,
                workspace,
                row,
                stack,
                viewport_offset,
            } if monitor == idx && workspace == ws_id => {
                (stack.map(|s| (s, row)), Some(viewport_offset))
            }
            _ => (None, None),
        };
        let applied = match ws.scrolling.leave_fullscreen(focused, merge, viewport) {
            Some(applied) => applied,
            None => AppliedLayout {
                virtual_layout: ws.scrolling.virtual_layout().clone(),
                actual_layout: ws.scrolling.actual_layout().clone(),
            },
        };
        self.remember_monitor(focused, idx);
        self.animate_workspaces_on(
            idx,
            &[(applied.actual_layout.clone(), offset)],
            PrevRects::Registry,
        );
        self.registry
            .update_tiling_slots_from_layout(&applied.virtual_layout);
        self.registry.update_tiled_rects(&applied.actual_layout);
        self.refresh_border_for(hwnd);
        log::info!("fullscreen: hwnd {hwnd} back in the strip of monitor {idx}");
        SocketResponse::Ok
    }

    /// The vertical parking offset of `ws_id` on monitor `idx` (0 when it
    /// is the active workspace).
    pub(super) fn parking_offset(&self, idx: usize, ws_id: WorkspaceId) -> i32 {
        let m = &self.monitors[idx];
        let gap = m.active_scrolling().padding().window_gap;
        WorkspaceParking::new(m.active_scrolling().monitor(), m.screen_rect(), gap)
            .offset(ws_id, m.active_workspace_id())
    }

    /// A fullscreen window was just seated in the strip of workspace
    /// `ws_id` on monitor `idx` again (restored, shown, moved to another
    /// monitor or workspace, re-seated by a hotplug or a loadout): flag its
    /// column, start the view at it when `view`, and raise it. Returns the
    /// strip's new layout when anything changed; the caller animates it.
    /// No-op for other windows.
    pub(super) fn reflag_fullscreen(
        &mut self,
        window: WindowId,
        idx: usize,
        ws_id: WorkspaceId,
        view: bool,
    ) -> Option<AppliedLayout> {
        if !self.is_fullscreen(window.0) {
            return None;
        }
        let strip = &mut self.monitors[idx].workspace_mut(ws_id)?.scrolling;
        let applied = if view {
            strip
                .enter_fullscreen(window)
                .or_else(|| strip.ensure_focused_visible())
        } else {
            strip.mark_fullscreen(window, true)
        };
        self.raise_fullscreen(window.0);
        applied
    }

    /// Bring every strip's column flags in line with the fullscreen map
    /// after a display change or a loadout rebuilt the strips: a
    /// fullscreen window seated in a strip gets its column flagged (the
    /// view starting at it when it is that strip's focus), a flagged column
    /// whose window is no longer fullscreen is cleared, and a fullscreen
    /// window the registry no longer knows is forgotten. The strips are
    /// re-projected in place; the caller seats them.
    pub(super) fn resync_fullscreen_columns(&mut self) {
        let gone: Vec<isize> = self
            .fullscreen
            .keys()
            .copied()
            .filter(|h| !self.registry.is_tracked(*h))
            .collect();
        for hwnd in gone {
            self.forget_fullscreen(hwnd, false);
        }
        for mi in 0..self.monitors.len() {
            let ids: Vec<WorkspaceId> = self.monitors[mi]
                .workspaces()
                .iter()
                .map(|w| w.id)
                .collect();
            for ws_id in ids {
                let Some(ws) = self.monitors[mi].workspace(ws_id) else {
                    continue;
                };
                let focus = ws.scrolling.last_focused_window();
                // (window, flag wanted, start the view at it)
                let mut marks: Vec<(WindowId, bool, bool)> = Vec::new();
                for c in &ws.scrolling.virtual_layout().columns {
                    if c.fullscreen && c.rows.len() > 1 {
                        marks.push((c.rows[0].window_id, false, false));
                    }
                    for r in &c.rows {
                        let wanted = self.is_fullscreen(r.window_id.0);
                        if wanted != (c.fullscreen && c.rows.len() == 1) {
                            marks.push((r.window_id, wanted, Some(r.window_id) == focus));
                        }
                    }
                }
                for (id, on, view) in marks {
                    if on {
                        let _ = self.reflag_fullscreen(id, mi, ws_id, view);
                    } else if let Some(ws) = self.monitors[mi].workspace_mut(ws_id) {
                        let _ = ws.scrolling.mark_fullscreen(id, false);
                    }
                }
            }
        }
    }

    /// `tiling off` (ADR-0008): every fullscreen window leaves fullscreen
    /// where it is — an app's own full screen ended (`SC_RESTORE`), the
    /// topmost flag cleared, its column a plain column again — so the pause
    /// puts it on screen like any tile. `tiling on` finds an ordinary tile.
    pub(super) fn leave_all_fullscreen_for_pause(&mut self) {
        let hwnds: Vec<isize> = self.fullscreen.keys().copied().collect();
        for hwnd in hwnds {
            self.leave_app_fullscreen_with(hwnd, &Win32AppFullscreen);
            self.fullscreen.remove(&hwnd);
            let _ = registry_win32::set_topmost(hwnd, false);
            let wid = WindowId(hwnd);
            for m in &mut self.monitors {
                for ws in m.workspaces_mut() {
                    let _ = ws.scrolling.mark_fullscreen(wid, false);
                }
            }
        }
    }

    /// The shutdown rescue's share of fullscreen (ADR-0009): every
    /// fullscreen window leaves it (an app's own full screen ended, the
    /// topmost flag cleared, the map emptied) and the one in view on its
    /// monitor is returned with that monitor's work area, so the caller
    /// moves it off the taskbar. Parked ones are left to the stranded-window
    /// pass.
    pub(super) fn release_all_fullscreen(&mut self) -> Vec<(isize, Rect)> {
        let hwnds: Vec<isize> = self.fullscreen.keys().copied().collect();
        let mut out = Vec::new();
        for hwnd in hwnds {
            self.leave_app_fullscreen_with(hwnd, &Win32AppFullscreen);
            self.fullscreen.remove(&hwnd);
            let _ = registry_win32::set_topmost(hwnd, false);
            let wid = WindowId(hwnd);
            let Some(idx) = self.monitor_index_containing(wid) else {
                continue;
            };
            let screen = self.monitors[idx].screen_rect();
            let in_view = self
                .registry
                .get_window(HWND(hwnd as *mut _))
                .and_then(|w| w.tiled_rect)
                .is_some_and(|r| r.overlaps(screen))
                && self.monitors[idx].find_workspace_containing(wid)
                    == Some(self.monitors[idx].active_workspace_id());
            if in_view {
                out.push((hwnd, self.monitors[idx].work_area()));
            }
        }
        out
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
            is_iconic: false,
            is_elevated: false,
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

    /// The rect the active strip of monitor `mi` projects for `hwnd`.
    fn strip_rect(flow: &FlowWM, mi: usize, hwnd: isize) -> Option<Rect> {
        flow.monitors[mi]
            .active_scrolling()
            .actual_layout()
            .find(WindowId(hwnd))
            .map(|e| e.rect)
    }

    /// Whether `hwnd`'s column in the active strip of `mi` is fullscreen.
    fn column_fullscreen(flow: &FlowWM, mi: usize, hwnd: isize) -> bool {
        flow.monitors[mi]
            .active_scrolling()
            .is_fullscreen_column(WindowId(hwnd))
    }

    fn parked(rect: Option<Rect>) -> bool {
        rect.is_some_and(|r| !r.overlaps(PRIMARY) && !r.overlaps(LEFT))
    }

    fn tiled_rect(flow: &FlowWM, hwnd: isize) -> Option<Rect> {
        flow.registry
            .get_window(HWND(hwnd as *mut _))
            .and_then(|w| w.tiled_rect)
    }

    #[test]
    fn a_stacked_tile_is_expelled_into_a_fullscreen_column_and_merged_back_at_its_row() {
        let mut flow = fixture();
        assert_eq!(strip(&flow, 0).find_window(WindowId(B)), Some((0, 1)));
        let vp_before = strip(&flow, 0).viewport_offset;

        assert_eq!(
            set_window(&mut flow, B, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        // Its own column right after the stack, flagged, at the stack's
        // width, the view starting at it.
        let vl = strip(&flow, 0);
        assert_eq!(vl.columns.len(), 3);
        assert_eq!(vl.find_window(WindowId(B)), Some((1, 0)));
        assert!(vl.columns[1].fullscreen);
        assert_eq!(vl.columns[1].width_px, 936);
        assert_eq!(vl.viewport_offset, 16 + 936 + 16 - GAP);
        // Over the SCREEN rect (taskbar covered); every other column parked.
        assert_eq!(strip_rect(&flow, 0, B), Some(PRIMARY));
        assert_eq!(tiled_rect(&flow, B), Some(PRIMARY));
        assert!(parked(strip_rect(&flow, 0, A)));
        assert!(parked(strip_rect(&flow, 0, C)));
        // Still a tile; known as fullscreen; no ring.
        assert!(flow.registry.is_tiling_active(B));
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
                row: 1,
                stack: Some(WindowId(A)),
                viewport_offset: vp_before,
            })
        );
        // The query reports the mode and the column flag.
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
        assert_eq!(payload["monitors"][0]["columns"][1]["fullscreen"], true);
        assert_eq!(payload["monitors"][0]["columns"][0]["fullscreen"], false);

        // Again: back to column 0, row 1, the view as it was.
        assert_eq!(
            set_window(&mut flow, B, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert!(!flow.is_fullscreen(B));
        let vl = strip(&flow, 0);
        assert_eq!(vl.columns.len(), 2);
        assert!(vl.columns.iter().all(|c| !c.fullscreen));
        assert_eq!(vl.find_window(WindowId(B)), Some((0, 1)));
        assert_eq!(vl.find_window(WindowId(A)), Some((0, 0)));
        assert_eq!(vl.viewport_offset, vp_before);
        assert!(matches!(
            flow.registry
                .get_window(HWND(B as *mut _))
                .map(|w| &w.state),
            Some(WindowState::Tiling(TilingState::Active { col: 0, row: 1 }))
        ));
        assert_eq!(flow.window_mode(B), "tiled");
    }

    #[test]
    fn a_lone_column_keeps_its_place_and_its_width() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let vl = strip(&flow, 0);
        assert_eq!(vl.columns.len(), 2);
        assert_eq!(vl.find_window(WindowId(C)), Some((1, 0)));
        assert!(vl.columns[1].fullscreen);
        assert_eq!(vl.columns[1].width_px, 936);
        assert_eq!(strip_rect(&flow, 0, C), Some(PRIMARY));
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let vl = strip(&flow, 0);
        assert_eq!(vl.find_window(WindowId(C)), Some((1, 0)));
        assert_eq!(vl.columns[1].width_px, 936);
        assert!(!vl.columns[1].fullscreen);
        assert_eq!(vl.viewport_offset, 0);
    }

    #[test]
    fn a_float_joins_the_strip_as_a_fullscreen_column_and_goes_back_to_its_rect() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, F, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert!(flow.is_fullscreen(F));
        assert_eq!(float_rect(&flow, 0, F), None);
        assert!(!crate::registry::hooks::is_float_hwnd(F));
        assert!(flow.registry.is_tiling_active(F));
        assert!(column_fullscreen(&flow, 0, F));
        assert_eq!(strip(&flow, 0).window_count(), 4);
        assert_eq!(strip_rect(&flow, 0, F), Some(PRIMARY));

        assert_eq!(
            set_window(&mut flow, F, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert!(!flow.is_fullscreen(F));
        assert_eq!(float_rect(&flow, 0, F), Some(F_RECT));
        assert_eq!(registry_rect(&flow, F), Some(F_RECT));
        assert_eq!(flow.window_mode(F), "floating");
        assert_eq!(strip(&flow, 0).window_count(), 3);
        assert!(strip(&flow, 0).columns.iter().all(|c| !c.fullscreen));
    }

    #[test]
    fn cycle_and_explicit_modes_leave_fullscreen_as_documented() {
        let mut flow = fixture();
        // cycle on a fullscreen float-origin window: it stays in the strip
        // as a plain tile, not back to a float.
        assert_eq!(
            set_window(&mut flow, F, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(
            set_window(&mut flow, F, WindowMode::Cycle),
            SocketResponse::Ok
        );
        assert!(strip(&flow, 0).find_window(WindowId(F)).is_some());
        assert!(!column_fullscreen(&flow, 0, F));
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

        // `set-window tile` on a fullscreen tile-origin window: back into
        // its stack.
        assert_eq!(
            set_window(&mut flow, A, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert_eq!(
            set_window(&mut flow, A, WindowMode::Tile),
            SocketResponse::Ok
        );
        assert_eq!(strip(&flow, 0).find_window(WindowId(A)), Some((0, 0)));
    }

    #[test]
    fn column_commands_are_refused_on_a_fullscreen_column_and_into_it() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        flow.registry.set_focused(C);
        for msg in [
            SocketMessage::ExpandColumn,
            SocketMessage::ShrinkColumn,
            SocketMessage::SetColumnWidth { width_px: 600 },
            SocketMessage::ToggleMonocle,
            SocketMessage::MergeColumn {
                direction: crate::common::Direction::Left,
            },
            SocketMessage::ConsumeOrExpel {
                direction: crate::common::Direction::Left,
            },
            SocketMessage::SwapLeft,
        ] {
            let SocketResponse::Error { message } = flow.dispatch(&msg) else {
                panic!("{msg:?} must be refused");
            };
            assert!(message.contains("fullscreen"), "{msg:?}: {message}");
        }
        assert!(column_fullscreen(&flow, 0, C));
        assert_eq!(strip(&flow, 0).columns[1].width_px, 936);

        // A merge into it from the stack next to it: refused, nothing moves.
        flow.registry.set_focused(A);
        let SocketResponse::Error { message } = flow.dispatch(&SocketMessage::MergeColumn {
            direction: crate::common::Direction::Right,
        }) else {
            panic!("merge into a fullscreen column must be refused");
        };
        assert!(message.contains("fullscreen"), "{message}");
        assert_eq!(strip(&flow, 0).find_window(WindowId(A)), Some((0, 0)));
        assert_eq!(strip(&flow, 0).columns[1].rows.len(), 1);
    }

    #[test]
    fn focus_off_a_fullscreen_column_shows_its_neighbours_and_back_shows_it_again() {
        let mut flow = fixture();
        // [A] [B fs] [C]
        assert_eq!(
            set_window(&mut flow, B, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        // Right: C starts the view; B parked left at the screen rect's size
        // (a pure move later).
        flow.registry.set_focused(B);
        assert_eq!(
            flow.dispatch(&SocketMessage::FocusRight),
            SocketResponse::Ok
        );
        assert_eq!(
            flow.monitors[0].active_scrolling().last_focused_window(),
            Some(WindowId(C))
        );
        assert_eq!(strip_rect(&flow, 0, C).map(|r| r.x), Some(GAP));
        let b = strip_rect(&flow, 0, B).expect("B");
        assert!(parked(Some(b)));
        assert_eq!((b.width, b.height, b.y), (1920, 1080, 0));
        assert!(b.right() <= LEFT.x);

        // Left again: B full screen, C parked.
        flow.registry.set_focused(C);
        assert_eq!(flow.dispatch(&SocketMessage::FocusLeft), SocketResponse::Ok);
        assert_eq!(strip_rect(&flow, 0, B), Some(PRIMARY));
        assert!(parked(strip_rect(&flow, 0, C)));

        // Left from B: A in view, B parked right (screen size).
        flow.registry.set_focused(B);
        assert_eq!(flow.dispatch(&SocketMessage::FocusLeft), SocketResponse::Ok);
        assert_eq!(strip_rect(&flow, 0, A).map(|r| r.x), Some(GAP));
        let b = strip_rect(&flow, 0, B).expect("B");
        assert!(parked(Some(b)));
        assert!(b.x >= PRIMARY.right());
        assert_eq!((b.width, b.height), (1920, 1080));
        assert!(flow.is_fullscreen(B));

        // Center on a fullscreen column snaps the view to it.
        flow.registry.set_focused(B);
        flow.monitors[0]
            .active_scrolling_mut()
            .set_focus(WindowId(B));
        assert_eq!(flow.dispatch(&SocketMessage::Center), SocketResponse::Ok);
        assert_eq!(strip_rect(&flow, 0, B), Some(PRIMARY));
    }

    #[test]
    fn closing_minimizing_and_restoring_leave_nothing_stale() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, B, WindowMode::Fullscreen),
            SocketResponse::Ok
        );

        // Minimize: out of the strip, still remembered; restore: a
        // fullscreen column again, in view.
        flow.on_window_minimized(B);
        assert!(flow.is_fullscreen(B));
        assert_eq!(flow.window_mode(B), "fullscreen");
        assert!(strip(&flow, 0).find_window(WindowId(B)).is_none());
        flow.on_window_restored(B);
        assert!(column_fullscreen(&flow, 0, B));
        assert_eq!(strip_rect(&flow, 0, B), Some(PRIMARY));
        assert!(flow.is_fullscreen(B));

        // Destroyed: gone from the strip and from the fullscreen map.
        flow.on_window_destroyed(B);
        assert!(!flow.is_fullscreen(B));
        assert!(!flow.registry.is_tracked(B));
        assert_eq!(strip(&flow, 0).window_count(), 2);
        assert!(strip(&flow, 0).columns.iter().all(|c| !c.fullscreen));
    }

    #[test]
    fn fullscreen_columns_park_with_their_workspace_and_leave_via_tiling_off() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        // Workspace switch (layout only): it stays a fullscreen column of
        // workspace 1's strip.
        assert!(flow.switch_workspace_layout(WorkspaceId(2), WorkspaceId(1)));
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(2));
        assert_eq!(
            flow.monitors[0].find_workspace_containing(WindowId(C)),
            Some(WorkspaceId(1))
        );
        assert!(flow.is_fullscreen(C));
        assert!(flow.switch_workspace_layout(WorkspaceId(1), WorkspaceId(2)));
        assert!(column_fullscreen(&flow, 0, C));

        // Tiling off: a plain column again, origin forgotten.
        flow.leave_all_fullscreen_for_pause();
        assert!(!flow.is_fullscreen(C));
        assert!(!column_fullscreen(&flow, 0, C));
        assert_eq!(flow.window_mode(C), "tiled");
        assert!(!strip_rect(&flow, 0, C).is_some_and(|r| r == PRIMARY));
    }

    #[test]
    fn the_shutdown_release_returns_the_column_in_view_with_its_work_area() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let released = flow.release_all_fullscreen();
        assert_eq!(released, vec![(C, PRIMARY_WORK)]);
        assert!(!flow.is_fullscreen(C));
    }

    #[test]
    fn move_to_monitor_keeps_it_a_fullscreen_column_there_and_leaving_stays_there() {
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
        assert_eq!(strip(&flow, 0).find_window(WindowId(C)), None);
        assert!(column_fullscreen(&flow, 1, C));
        assert_eq!(strip_rect(&flow, 1, C), Some(LEFT));
        assert!(flow.is_fullscreen(C));
        assert_eq!(flow.active_monitor, 1);
        assert!(strip(&flow, 0).columns.iter().all(|c| !c.fullscreen));

        // Leaving on the other monitor: a plain column of that strip.
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        assert!(!flow.is_fullscreen(C));
        assert_eq!(strip(&flow, 1).find_window(WindowId(C)), Some((0, 0)));
        assert!(!column_fullscreen(&flow, 1, C));
        assert_eq!(flow.registry.remembered_monitor(C), Some(1));
    }

    #[test]
    fn a_fullscreen_window_seated_in_another_strip_is_flagged_there_in_view() {
        // What `move-to-workspace` does once the window is in the other
        // strip (the dispatch itself also switches workspaces, whose focus
        // push drops the fake handles of this fixture).
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let _ = flow.monitors[0]
            .active_scrolling_mut()
            .remove_window(WindowId(C));
        let ws2 = flow.monitors[0]
            .workspace_mut(WorkspaceId(2))
            .expect("workspace 2");
        let _ = ws2.scrolling.insert_window(WindowId(C));
        let applied = flow
            .reflag_fullscreen(WindowId(C), 0, WorkspaceId(2), true)
            .expect("flagged");
        assert!(applied.virtual_layout.columns[0].fullscreen);
        assert_eq!(
            applied.actual_layout.find(WindowId(C)).map(|e| e.rect),
            Some(PRIMARY)
        );
        // Nothing to do for a window that is not fullscreen.
        assert!(
            flow.reflag_fullscreen(WindowId(A), 0, WorkspaceId(1), true)
                .is_none()
        );
    }

    #[test]
    fn a_hotplug_or_loadout_rebuild_flags_the_column_again() {
        let mut flow = fixture();
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        // The strip loses the flag (a rebuild); the map still has C.
        let _ = flow.monitors[0]
            .active_scrolling_mut()
            .mark_fullscreen(WindowId(C), false);
        assert!(!column_fullscreen(&flow, 0, C));
        flow.resync_fullscreen_columns();
        assert!(column_fullscreen(&flow, 0, C));
        assert_eq!(strip_rect(&flow, 0, C), Some(PRIMARY));
        // A flag without a fullscreen window is cleared.
        let _ = flow.monitors[0]
            .active_scrolling_mut()
            .mark_fullscreen(WindowId(A), true);
        flow.resync_fullscreen_columns();
        assert!(!column_fullscreen(&flow, 0, A));
    }

    #[test]
    fn with_presets_a_column_back_from_fullscreen_takes_the_monitors_current_preset_width() {
        let mut flow = fixture();
        flow.config.min_column_width_px = 320;
        flow.config.columns =
            Some(toml::from_str("presets = [\"1/3\", \"1/2\", \"1/1\"]").expect("columns"));
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        // The monitor moves to 1/3 while C is fullscreen: C stays
        // fullscreen, its way-back width follows.
        flow.apply_width_preset(0, 0);
        assert!(column_fullscreen(&flow, 0, C));
        assert_eq!(strip_rect(&flow, 0, C), Some(PRIMARY));
        assert_eq!(
            set_window(&mut flow, C, WindowMode::Fullscreen),
            SocketResponse::Ok
        );
        let vl = strip(&flow, 0);
        let (col, _) = vl.find_window(WindowId(C)).expect("C back");
        assert_eq!(vl.columns[col].width_px, 618);
    }

    // --- app fullscreen (ADR-0020) ---

    use std::cell::{Cell, RefCell};

    /// A scripted app: `SC_MAXIMIZE` takes its chrome away and fills the
    /// monitor (when `obeys`), `SC_RESTORE` gives the chrome back.
    struct FakeApp {
        obeys: bool,
        /// DWM still reports the chromeless frame after the switch back.
        stale: Cell<bool>,
        chrome: Cell<Option<bool>>,
        live: Cell<bool>,
        requests: RefCell<Vec<(isize, bool)>>,
    }

    impl FakeApp {
        fn new(obeys: bool) -> Self {
            Self {
                obeys,
                stale: Cell::new(false),
                chrome: Cell::new(Some(true)),
                live: Cell::new(false),
                requests: RefCell::new(Vec::new()),
            }
        }
        fn set(&self, chrome: bool, live: bool) {
            self.chrome.set(Some(chrome));
            self.live.set(live);
        }
    }

    impl AppFullscreenProbe for FakeApp {
        fn request(&self, hwnd: isize, enter: bool) -> bool {
            self.requests.borrow_mut().push((hwnd, enter));
            if self.obeys {
                self.set(!enter, enter);
            }
            true
        }
        fn wait(&self, _hwnd: isize, enter: bool) -> bool {
            if enter {
                self.live.get()
            } else {
                self.chrome.get() != Some(false)
            }
        }
        fn is_fullscreen_live(&self, _hwnd: isize) -> bool {
            self.live.get()
        }
        fn has_chrome(&self, _hwnd: isize) -> Option<bool> {
            self.chrome.get()
        }
        fn invisible_bounds(&self, _hwnd: isize) -> InvisibleBounds {
            if self.chrome.get() == Some(false) || self.stale.get() {
                InvisibleBounds::default()
            } else {
                FRAME
            }
        }
    }

    const FRAME: InvisibleBounds = InvisibleBounds {
        left: 7,
        top: 0,
        right: 7,
        bottom: 7,
    };

    fn bounds(flow: &FlowWM, hwnd: isize) -> InvisibleBounds {
        flow.registry
            .get_window(HWND(hwnd as *mut _))
            .map(|w| w.invisible_bounds)
            .expect("tracked")
    }

    fn strip_layout(flow: &FlowWM) -> crate::layout::types::ActualLayout {
        flow.monitors[0].active_scrolling().actual_layout().clone()
    }

    fn targets_for(
        t: &super::super::animation::WorkspaceTargets,
        hwnd: isize,
    ) -> Vec<crate::animation::types::WindowTarget> {
        t.targets
            .iter()
            .filter(|t| t.window_ref == crate::animation::types::WindowRef(hwnd))
            .cloned()
            .collect()
    }

    #[test]
    fn app_fullscreen_step_adopts_and_leaves_by_the_chrome() {
        use AppFullscreenStep::{Adopt, Leave};
        // Gone: nothing (the destroy event cleans up).
        assert_eq!(app_fullscreen_step(true, false, false, None), None);
        // Known and the chrome is back: the app left.
        assert_eq!(
            app_fullscreen_step(true, false, false, Some(true)),
            Some(Leave)
        );
        // Known, chromeless, parked off its monitor: still fullscreen.
        assert_eq!(app_fullscreen_step(true, false, false, Some(false)), None);
        // Not known: a seated window that went chromeless over its monitor.
        assert_eq!(
            app_fullscreen_step(false, true, true, Some(false)),
            Some(Adopt)
        );
        // ... but not mid-switch (not covering yet), not a window that is
        // not seated (minimized, frame fullscreen), not one with chrome.
        assert_eq!(app_fullscreen_step(false, true, false, Some(false)), None);
        assert_eq!(app_fullscreen_step(false, false, true, Some(false)), None);
        assert_eq!(app_fullscreen_step(false, true, true, Some(true)), None);
    }

    #[test]
    fn mod_f_asks_the_app_and_never_places_its_frame_then_merges_it_back_into_its_stack() {
        let mut flow = fixture();
        if let Some(w) = flow.registry.get_window_mut(HWND(B as *mut _)) {
            w.invisible_bounds = FRAME;
        }
        let app = FakeApp::new(true);
        assert_eq!(
            flow.enter_app_fullscreen_with(WindowId(B), &app, true),
            SocketResponse::Ok
        );
        assert_eq!(*app.requests.borrow(), vec![(B, true)]);
        // Same column and bookkeeping as a frame fullscreen ...
        assert!(flow.is_fullscreen(B));
        assert!(flow.is_app_fullscreen(B));
        assert_eq!(flow.window_mode(B), "fullscreen");
        assert!(column_fullscreen(&flow, 0, B));
        assert_eq!(strip_rect(&flow, 0, B), Some(PRIMARY));
        // ... but its chromeless frame has no invisible borders and the rect
        // the app chose counts as sent: a batch of its strip has nothing
        // for it.
        assert_eq!(bounds(&flow, B), InvisibleBounds::default());
        assert_eq!(flow.last_submitted_rect(B), Some(PRIMARY));
        let t = flow.workspace_targets(0, &[(strip_layout(&flow), 0)], PrevRects::Registry);
        assert!(targets_for(&t, B).is_empty());

        // Mod+F again: SC_RESTORE, the chrome's borders re-measured, back
        // in column 0 row 1 as a tile.
        assert_eq!(
            flow.restore_from_fullscreen_with(WindowId(B), FullscreenExit::Origin, &app),
            SocketResponse::Ok
        );
        assert_eq!(*app.requests.borrow(), vec![(B, true), (B, false)]);
        assert!(!flow.is_fullscreen(B));
        assert!(!flow.is_app_fullscreen(B));
        assert_eq!(bounds(&flow, B), FRAME);
        assert_eq!(strip(&flow, 0).find_window(WindowId(B)), Some((0, 1)));
        assert_eq!(flow.window_mode(B), "tiled");
        crate::registry::hooks::set_app_fullscreen_watch(B, false);
    }

    #[test]
    fn an_app_that_ignores_sc_maximize_stays_where_it_was() {
        let mut flow = fixture();
        let app = FakeApp::new(false);
        assert!(matches!(
            flow.enter_app_fullscreen_with(WindowId(C), &app, true),
            SocketResponse::Error { .. }
        ));
        assert_eq!(*app.requests.borrow(), vec![(C, true)]);
        assert!(!flow.is_fullscreen(C));
        assert_eq!(strip(&flow, 0).find_window(WindowId(C)), Some((1, 0)));
        assert!(!column_fullscreen(&flow, 0, C));
        crate::registry::hooks::set_app_fullscreen_watch(C, false);
    }

    #[test]
    fn the_apps_own_switches_are_adopted_and_followed() {
        const R: isize = 0x5E1;
        let mut flow = fixture();
        fake_window(&mut flow.registry, R, tile());
        flow.registry.set_invisible_bounds(R, FRAME);
        flow.registry.set_monitor(R, 0);
        let _ = flow.monitors[0]
            .active_scrolling_mut()
            .add_window(WindowId(R));
        let app = FakeApp::new(true);

        // Not watched (no `app` rule): a chromeless full-screen switch is
        // none of the daemon's business.
        app.set(false, true);
        flow.sync_app_fullscreen_with(R, &app);
        assert!(!flow.is_fullscreen(R));

        // Watched: the title-bar maximize is adopted, nothing is posted.
        crate::registry::hooks::set_app_fullscreen_watch(R, true);
        flow.sync_app_fullscreen_with(R, &app);
        assert!(flow.is_app_fullscreen(R));
        assert_eq!(flow.window_mode(R), "fullscreen");
        assert!(column_fullscreen(&flow, 0, R));
        assert!(app.requests.borrow().is_empty());

        // Parked (workspace switch or focus off it): chromeless, off its
        // monitor: kept.
        app.set(false, false);
        flow.sync_app_fullscreen_with(R, &app);
        assert!(flow.is_app_fullscreen(R));

        // The connection bar's restore: chrome back, followed out into a
        // plain column, again without posting anything. DWM still reports
        // no borders at that moment: the captioned frame's from before
        // stand.
        app.set(true, false);
        app.stale.set(true);
        flow.sync_app_fullscreen_with(R, &app);
        assert!(!flow.is_fullscreen(R));
        assert!(strip(&flow, 0).find_window(WindowId(R)).is_some());
        assert!(!column_fullscreen(&flow, 0, R));
        assert_eq!(flow.window_mode(R), "tiled");
        assert_eq!(bounds(&flow, R), FRAME);
        assert!(app.requests.borrow().is_empty());

        // Tiling off: nothing is adopted.
        app.set(false, true);
        flow.tiling_paused = true;
        flow.sync_app_fullscreen_with(R, &app);
        assert!(!flow.is_fullscreen(R));
        crate::registry::hooks::set_app_fullscreen_watch(R, false);
    }

    #[test]
    fn a_parked_app_fullscreen_column_moves_by_its_screen_rect_and_closing_forgets_it() {
        let mut flow = fixture();
        let app = FakeApp::new(true);
        assert_eq!(
            flow.enter_app_fullscreen_with(WindowId(C), &app, true),
            SocketResponse::Ok
        );
        // A workspace parking offset is a real move: one target, exactly
        // the screen rect shifted (no invisible-border compensation, no
        // resize).
        let t = flow.workspace_targets(0, &[(strip_layout(&flow), 2000)], PrevRects::Registry);
        let mine = targets_for(&t, C);
        assert_eq!(mine.len(), 1);
        assert_eq!(
            (mine[0].final_position.x, mine[0].final_position.y),
            (0, 2000)
        );
        assert_eq!((mine[0].final_size.x, mine[0].final_size.y), (1920, 1080));

        // Focus off it: the column parking is a move at the same size.
        flow.registry.set_focused(C);
        assert_eq!(flow.dispatch(&SocketMessage::FocusLeft), SocketResponse::Ok);
        let parked_at = strip_rect(&flow, 0, C).expect("C");
        assert!(parked(Some(parked_at)));
        assert_eq!((parked_at.width, parked_at.height), (1920, 1080));

        flow.on_window_destroyed(C);
        assert!(!flow.is_fullscreen(C));
        assert!(!flow.is_app_fullscreen(C));
        assert!(!crate::registry::hooks::is_app_fullscreen_watched(C));
    }
}
