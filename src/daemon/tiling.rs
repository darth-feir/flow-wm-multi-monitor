//! Tiling on/off — pause and resume the window manager without stopping
//! the daemon.
//!
//! The user's requirement: *a toggle to turn tiling on and off; when it is
//! off the desktop goes back to plain Windows and the apps are moved back
//! on screen.* The hotkey script owns the toggle key and keeps running, so
//! this is a pause inside the daemon, not a stop: the pipe keeps answering,
//! the hooks keep the registry consistent, and `flow dispatch tiling on`
//! brings everything back without a restart.
//!
//! # Pause ([`FlowWM::pause_tiling`])
//!
//! 1. The animator is cancelled synchronously, so no tween can land after
//!    the windows have been put back.
//! 2. Every **tiled** window of every workspace of every monitor — active
//!    and parked alike, the parked ones being the "apps moved on screen"
//!    case — is sent an instant target: the window rect it had when it was
//!    first tiled this run ([`Window::pre_tiling_rect`]) when the registry
//!    has one (clamped into the work area of the monitor it lies on), else
//!    its workspace-local layout rect clamped into its monitor's work area
//!    ([`pause_target`]). A **float** is left where it is unless it sits off
//!    the virtual screen (a float of a parked workspace), in which case it
//!    is clamped into its monitor's work area.
//! 3. Minimized windows get the stage-7B placement rescue: a restore
//!    position parked beyond the screen is rewritten so the window comes
//!    back on-screen when the user restores it.
//! 4. Every border overlay is dropped.
//! 5. `tiling_paused` is set, the status file flips to `tiling: off`, and
//!    one summary line is logged. **Nothing else is destroyed**: monitors,
//!    workspaces, strips and floating spaces stay exactly as they were —
//!    they are the memory the resume rebuilds from.
//!
//! # While paused
//!
//! Hook events only keep the registry consistent (created → classified and
//! registered, destroyed → forgotten, minimize / restore / hide / show /
//! state and name changes → registry transitions, foreground → tracked
//! focus); nothing moves a window, seats a new one or pushes focus.
//! Drag handling, edge-scroll and float-location sync are no-ops. Every
//! layout-mutating command (focus, swap, scroll, resize, set-window,
//! workspace and monitor moves, config reload, loadout load) is answered
//! with [`TILING_OFF_MESSAGE`]; `SetTiling` / `ToggleTiling`, `Ping`, the
//! queries, `Stop` and the loadout save keep working. A display-topology
//! change is **deferred**: the geometry is left alone (a re-seat would move
//! windows that are Windows' to arrange) and reconciled first thing on
//! resume.
//!
//! # Resume ([`FlowWM::resume_tiling`])
//!
//! Rebuilds from the live desktop the way startup does:
//!
//! 1. A deferred display change is reconciled (`reconcile_monitors`).
//! 2. The top-level windows are re-scanned (`scan_existing_windows`), so
//!    windows created during the pause are classified through the rules;
//!    windows that no longer exist are dropped from the registry and every
//!    space, and the minimized state is re-synced with `IsIconic`.
//! 3. Every strip keeps the windows that are still live `Tiling(Active)` in
//!    their remembered (monitor, workspace, column, row) seats — row
//!    heights and column widths included — and drops the others; a float
//!    of an active workspace has its rect refreshed from the desktop (the
//!    user may have dragged it), a float that is no longer an active float
//!    leaves its space.
//! 4. The foreground window becomes the tracked focus; its monitor becomes
//!    active and, if it lives on a parked workspace, that workspace becomes
//!    the monitor's active one so the user keeps looking at it.
//! 5. Tiles seated nowhere (new, or restored from a state that had emptied
//!    their seat) are appended to the active workspace of the monitor
//!    Windows reports for them (`MonitorFromWindow`), else their remembered
//!    monitor, else the active one — the workspace step 4 just made
//!    visible, so a window that appeared during the pause stays on-screen;
//!    unseated floats go to the same monitor at their current rect, clamped
//!    into its work area. A foreground window seated here takes the focus
//!    as in step 4.
//! 6. Every monitor is re-projected and seated as one **instant** batch
//!    (the hotplug seating), the float-tracking set is rebuilt, the borders
//!    are reattached, `tiling_paused` clears, the status file flips back to
//!    `tiling: on`, and one summary line is logged.
//!
//! Resuming while already tiling (or pausing while already paused) does
//! nothing. The Win32 questions both passes ask are behind [`TilingProbe`]
//! so the whole thing is unit-tested against a mock desktop.
//!
//! [`Window::pre_tiling_rect`]: crate::registry::types::Window::pre_tiling_rect

use windows::Win32::Foundation::HWND;

use crate::animation::{IVec2, WindowRef, WindowTarget};
use crate::common::{InvisibleBounds, Rect, WindowId};
use crate::ipc::message::SocketResponse;
use crate::registry::WindowRegistry;
use crate::registry::hooks::remove_float_hwnd;
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32 as registry_win32;
use crate::workspace::WorkspaceParking;

use super::hotplug::{TopologyProbe, TopologySnapshot, Win32Topology};
use super::shutdown::RescueMonitor;
use super::types::{FlowWM, monitor_index_for_rect_in};

/// The error every layout-mutating command is answered with while tiling
/// is off.
pub const TILING_OFF_MESSAGE: &str = "tiling is off (flow dispatch tiling on)";

/// The Win32 questions the pause and the resume ask, behind a trait so
/// both passes run against a mock desktop in unit tests. The topology half
/// ([`TopologyProbe`]) serves the deferred display-change reconcile.
pub(super) trait TilingProbe: TopologyProbe {
    /// Re-scan the top-level windows into `registry`
    /// ([`WindowRegistry::scan_existing_windows`]).
    fn scan_windows(&self, registry: &mut WindowRegistry) -> Result<(), String>;
    /// `IsWindow(hwnd)`.
    fn is_window(&self, hwnd: isize) -> bool;
    /// `GetWindowRect(hwnd)` — the full window rect, or `None` when it
    /// cannot be read.
    fn window_rect(&self, hwnd: isize) -> Option<Rect>;
    /// `GetForegroundWindow()`.
    fn foreground_window(&self) -> Option<isize>;
    /// Re-anchor a minimized window whose restore position is off-screen
    /// ([`crate::rescue::rescue_minimized_placement`]); `Ok(Some)` when the
    /// placement was rewritten.
    fn fix_minimized_placement(
        &self,
        hwnd: isize,
        bounds: InvisibleBounds,
        monitors: &[RescueMonitor],
        owner: Option<usize>,
    ) -> Result<Option<(Rect, Rect)>, String>;
}

/// The production [`TilingProbe`]: plain Win32.
pub(super) struct Win32Tiling;

impl TopologyProbe for Win32Tiling {
    fn snapshot(&self) -> Result<TopologySnapshot, String> {
        Win32Topology.snapshot()
    }

    fn monitor_handle_for_window(&self, hwnd: isize) -> isize {
        Win32Topology.monitor_handle_for_window(hwnd)
    }

    fn is_iconic(&self, hwnd: isize) -> bool {
        Win32Topology.is_iconic(hwnd)
    }

    fn restore_window(&self, hwnd: isize) -> bool {
        Win32Topology.restore_window(hwnd)
    }

    fn focus_window(&self, hwnd: isize) -> bool {
        Win32Topology.focus_window(hwnd)
    }
}

impl TilingProbe for Win32Tiling {
    fn scan_windows(&self, registry: &mut WindowRegistry) -> Result<(), String> {
        registry.scan_existing_windows()
    }

    fn is_window(&self, hwnd: isize) -> bool {
        registry_win32::is_window(hwnd)
    }

    fn window_rect(&self, hwnd: isize) -> Option<Rect> {
        registry_win32::get_window_rect(HWND(hwnd as *mut _)).ok()
    }

    fn foreground_window(&self) -> Option<isize> {
        registry_win32::get_foreground_window()
    }

    fn fix_minimized_placement(
        &self,
        hwnd: isize,
        bounds: InvisibleBounds,
        monitors: &[RescueMonitor],
        owner: Option<usize>,
    ) -> Result<Option<(Rect, Rect)>, String> {
        crate::rescue::rescue_minimized_placement(HWND(hwnd as *mut _), bounds, monitors, owner)
    }
}

/// What one pause did, for the log line and the tests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PauseSummary {
    /// Tiling was already off; nothing was done.
    pub already_paused: bool,
    /// Tiled windows sent on-screen (every tile of every workspace).
    pub tiles: usize,
    /// Of those, the ones put back at their pre-tiling rect.
    pub restored: usize,
    /// Floats that sat off the virtual screen and were clamped on-screen.
    pub floats_clamped: usize,
    /// Minimized windows whose off-screen restore placement was fixed.
    pub minimized_fixed: usize,
    /// Border overlays dropped.
    pub borders: usize,
}

/// What one resume did, for the log line and the tests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResumeSummary {
    /// Tiling was already on; nothing was done.
    pub already_tiling: bool,
    /// Whether a display change deferred during the pause was reconciled.
    pub topology_reconciled: bool,
    /// Monitors after the resume.
    pub monitors: usize,
    /// Tiles that kept their remembered seat.
    pub kept: usize,
    /// Tiles appended because they had no seat (new windows, mostly).
    pub appended: usize,
    /// Floats seated or whose rect was refreshed from the desktop.
    pub floats: usize,
    /// Windows dropped from the registry because they no longer exist.
    pub dropped: usize,
    /// Animator targets submitted by the final instant seating.
    pub targets: usize,
}

/// The instant batch a pause submits, plus the counts behind it.
#[derive(Debug, Default)]
pub(super) struct PauseTargets {
    /// One instant target per window that moves.
    pub targets: Vec<WindowTarget>,
    /// The visible rect commanded for each of them, for
    /// [`FlowWM::record_submitted`].
    pub submitted: Vec<(isize, Rect)>,
    /// See [`PauseSummary::tiles`].
    pub tiles: usize,
    /// See [`PauseSummary::restored`].
    pub restored: usize,
    /// See [`PauseSummary::floats_clamped`].
    pub floats_clamped: usize,
}

/// Where a tiled window goes when tiling is turned off, as a **visible**
/// rect, and whether that is its pre-tiling rect.
///
/// `pre_tiling_visible` is the window's pre-tiling anchor translated to
/// visible space, if the registry has one: it is restored, clamped into the
/// work area of the monitor whose screen holds its centre (so a window
/// that came from a display that has since been unplugged still lands
/// on-screen), falling back to the owning monitor's. Without an anchor the
/// window's workspace-local `layout_rect` is clamped into the owning
/// monitor's work area — a visible tile stays put, a parked one (parked
/// column, parked workspace) comes to the nearest edge.
///
/// `screens` / `work_areas` are indexed like the daemon's monitor list;
/// `owner` must be a valid index into both.
#[must_use]
pub(super) fn pause_target(
    pre_tiling_visible: Option<Rect>,
    layout_rect: Rect,
    owner: usize,
    screens: &[Rect],
    work_areas: &[Rect],
) -> (Rect, bool) {
    let owner_wa = work_areas[owner];
    match pre_tiling_visible {
        Some(pre) => {
            let wa = monitor_index_for_rect_in(screens, pre)
                .and_then(|i| work_areas.get(i).copied())
                .unwrap_or(owner_wa);
            (pre.clamped_into(wa), true)
        }
        None => (layout_rect.clamped_into(owner_wa), false),
    }
}

impl FlowWM {
    /// Whether the daemon is currently tiling (`false` after `flow dispatch
    /// tiling off`). Reported by `Ping` and `flow query all`.
    #[must_use]
    pub fn tiling_enabled(&self) -> bool {
        !self.tiling_paused
    }

    /// `flow dispatch tiling on|off|toggle`: pause or resume, then answer
    /// with the resulting state as `{"tiling_enabled": bool}` so the CLI
    /// can print `flow: tiling on` / `flow: tiling off` even for a toggle.
    pub(super) fn dispatch_set_tiling(&mut self, enabled: bool) -> SocketResponse {
        if enabled {
            self.resume_tiling();
        } else {
            self.pause_tiling();
        }
        SocketResponse::Data {
            payload: serde_json::json!({ "tiling_enabled": self.tiling_enabled() }),
        }
    }

    /// Turn tiling off: put every tiled window back on-screen, drop the
    /// borders and go inert until [`resume_tiling`](Self::resume_tiling).
    /// See the `daemon::tiling` module docs for the exact steps. Idempotent.
    pub fn pause_tiling(&mut self) -> PauseSummary {
        self.pause_tiling_with(&Win32Tiling)
    }

    /// Turn tiling back on: rebuild the layout from the live desktop and
    /// seat everything. See the `daemon::tiling` module docs. Idempotent.
    pub fn resume_tiling(&mut self) -> ResumeSummary {
        let summary = self.resume_tiling_with(&Win32Tiling);
        if !summary.already_tiling {
            // Overlays are real windows: attached here, outside the
            // mock-testable core.
            self.refresh_all_border_styles();
        }
        summary
    }

    /// [`pause_tiling`](Self::pause_tiling) against `probe`.
    pub(super) fn pause_tiling_with<P: TilingProbe>(&mut self, probe: &P) -> PauseSummary {
        if self.tiling_paused {
            log::debug!("tiling: already off");
            return PauseSummary {
                already_paused: true,
                ..PauseSummary::default()
            };
        }

        // 1. No tween may land after the windows have been put back.
        self.animator.cancel_and_wait();

        // 2. Every tile on-screen, off-screen floats clamped, in one
        //    instant batch.
        let PauseTargets {
            targets,
            submitted,
            tiles,
            restored,
            floats_clamped,
        } = self.pause_targets();
        if !targets.is_empty() {
            log::debug!(
                "tiling: submitting {} instant target(s) to put windows back",
                targets.len()
            );
            if let Err(e) = self.animator.animate(targets) {
                log::warn!("tiling: pause batch error: {e}");
            }
        }
        self.record_submitted(&submitted);

        // 3. Minimized windows: a restore position parked beyond the screen
        //    is rewritten so the window comes back on-screen when restored.
        let rescue_monitors: Vec<RescueMonitor> = self
            .monitors
            .iter()
            .map(|m| RescueMonitor {
                screen_rect: m.screen_rect(),
                work_area: m.work_area(),
            })
            .collect();
        let mut minimized_fixed = 0_usize;
        for (hwnd, bounds) in self.registry.minimized_windows() {
            let wid = WindowId(hwnd);
            let owner = self
                .monitor_index_containing(wid)
                .or_else(|| self.remembered_monitor(wid));
            match probe.fix_minimized_placement(hwnd, bounds, &rescue_monitors, owner) {
                Ok(Some((from, to))) => {
                    log::info!(
                        "tiling: re-anchored minimized hwnd {hwnd} from ({},{}) to ({},{})",
                        from.x,
                        from.y,
                        to.x,
                        to.y
                    );
                    minimized_fixed += 1;
                }
                Ok(None) => {}
                Err(e) => {
                    log::debug!("tiling: placement of minimized hwnd {hwnd} not fixed: {e}");
                }
            }
        }

        // 4. Nothing of the daemon stays on screen.
        let borders = self.registry.detach_all_borders();

        // 5. Inert until resume. A display change still settling is
        //    deferred to the resume like one reported during the pause.
        if self.topology_deadline.is_some() {
            self.topology_deferred = true;
            self.topology_deadline = None;
            self.topology_retries = 0;
        }
        self.tiling_paused = true;
        self.write_tiling_status(false);

        let summary = PauseSummary {
            already_paused: false,
            tiles,
            restored,
            floats_clamped,
            minimized_fixed,
            borders,
        };
        log::info!(
            "tiling: off — {} tile(s) moved on-screen ({} to their pre-tiling rect), {} float(s) clamped, {} minimized placement(s) fixed, {} border(s) dropped; layout-mutating commands are refused until `flow dispatch tiling on`",
            summary.tiles,
            summary.restored,
            summary.floats_clamped,
            summary.minimized_fixed,
            summary.borders
        );
        summary
    }

    /// The instant batch that puts every window back when tiling is turned
    /// off (see the [module docs](self) and [`pause_target`]). Pure over the
    /// daemon's bookkeeping — no Win32 — so tests can inspect it.
    #[must_use]
    pub(super) fn pause_targets(&self) -> PauseTargets {
        let screens: Vec<Rect> = self.monitors.iter().map(|m| m.screen_rect()).collect();
        let work_areas: Vec<Rect> = self.monitors.iter().map(|m| m.work_area()).collect();
        let virtual_screen = self.active_scrolling().monitor().virtual_screen;
        let mut out = PauseTargets::default();

        for (mi, m) in self.monitors.iter().enumerate() {
            let window_gap = m.active_scrolling().padding().window_gap;
            let parking =
                WorkspaceParking::new(m.active_scrolling().monitor(), m.screen_rect(), window_gap);
            let active_id = m.active_workspace_id();
            for ws in m.workspaces() {
                let offset = parking.offset(ws.id, active_id);

                // Every tile: back to its pre-tiling rect, else on-screen.
                for entry in &ws.scrolling.actual_layout().entries {
                    let hwnd = entry.window_id.0;
                    let window = self.registry.get_window(HWND(hwnd as *mut _));
                    let bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
                    let pre = window
                        .and_then(|w| w.pre_tiling_rect)
                        .map(|r| bounds.window_to_visible(r));
                    let (target, restored) =
                        pause_target(pre, entry.rect, mi, &screens, &work_areas);
                    push_instant(&mut out.targets, hwnd, bounds.visible_to_window(target));
                    out.submitted.push((hwnd, target));
                    out.tiles += 1;
                    if restored {
                        out.restored += 1;
                    }
                }

                // Floats: left alone unless off the virtual screen (a float
                // of a parked workspace), then brought into the work area at
                // their workspace-local rect.
                for f in ws.floating.windows() {
                    let current = Rect {
                        y: f.rect.y + offset,
                        ..f.rect
                    };
                    if current.overlaps(virtual_screen) {
                        continue;
                    }
                    let hwnd = f.window_id.0;
                    let bounds = self
                        .registry
                        .get_window(HWND(hwnd as *mut _))
                        .map(|w| w.invisible_bounds)
                        .unwrap_or_default();
                    let target = f.rect.clamped_into(work_areas[mi]);
                    push_instant(&mut out.targets, hwnd, bounds.visible_to_window(target));
                    out.submitted.push((hwnd, target));
                    out.floats_clamped += 1;
                }
            }
        }
        out
    }

    /// [`resume_tiling`](Self::resume_tiling) against `probe`, without the
    /// border refresh (which creates overlay windows). This is the whole
    /// algorithm; see the [module docs](self) for the outline.
    pub(super) fn resume_tiling_with<P: TilingProbe>(&mut self, probe: &P) -> ResumeSummary {
        if !self.tiling_paused {
            log::debug!("tiling: already on");
            return ResumeSummary {
                already_tiling: true,
                ..ResumeSummary::default()
            };
        }
        let mut summary = ResumeSummary::default();
        self.animator.cancel_and_wait();

        // 1. A display change that happened while tiling was off.
        if self.topology_deferred {
            match self.reconcile_monitors_with(probe) {
                Ok(_) => summary.topology_reconciled = true,
                Err(e) => log::warn!(
                    "tiling: deferred display-change reconcile failed, keeping the current monitor list: {e}"
                ),
            }
            self.topology_deferred = false;
            self.topology_first_report = None;
        }

        // 2. The live desktop: new windows classified through the rules,
        //    dead ones forgotten, the minimized state re-synced.
        if let Err(e) = probe.scan_windows(&mut self.registry) {
            log::warn!("tiling: window re-scan failed, relying on the hooks' bookkeeping: {e}");
        }
        let tracked: Vec<isize> = self.registry.windows().map(|w| w.hwnd.0 as isize).collect();
        for hwnd in tracked {
            if !probe.is_window(hwnd) {
                self.purge_windows_from_all_spaces(&[WindowId(hwnd)]);
                remove_float_hwnd(hwnd);
                self.forget_submitted(hwnd);
                self.registry.remove_window(hwnd);
                summary.dropped += 1;
                continue;
            }
            let iconic = probe.is_iconic(hwnd);
            let state = self
                .registry
                .get_window(HWND(hwnd as *mut _))
                .map(|w| w.state.clone());
            match state {
                Some(
                    WindowState::Tiling(TilingState::Active { .. })
                    | WindowState::Floating(FloatingState::Active { .. }),
                ) if iconic => self.registry.minimize_window(hwnd),
                Some(
                    WindowState::Tiling(TilingState::Minimized)
                    | WindowState::Floating(FloatingState::Minimized),
                ) if !iconic => self.registry.restore_window(hwnd),
                _ => {}
            }
        }

        // 3. Every strip keeps its live active tiles in their seats and
        //    drops the rest; floats likewise, with an active workspace's
        //    floats re-read from the desktop.
        for mi in 0..self.monitors.len() {
            let work_area = self.monitors[mi].work_area();
            let active_id = self.monitors[mi].active_workspace_id();
            for wsi in 0..self.monitors[mi].workspaces().len() {
                let ws = &mut self.monitors[mi].workspaces_mut()[wsi];
                let tiles: Vec<WindowId> = ws
                    .scrolling
                    .virtual_layout()
                    .columns
                    .iter()
                    .flat_map(|c| c.rows.iter().map(|r| r.window_id))
                    .collect();
                for wid in tiles {
                    if self.registry.is_tiling_active(wid.0) {
                        summary.kept += 1;
                    } else {
                        let _ = ws.scrolling.remove_window(wid);
                        log::debug!(
                            "tiling: window {} left its seat on monitor {mi} workspace {} (no longer an active tile)",
                            wid.0,
                            ws.id.0
                        );
                    }
                }
                let floats: Vec<(WindowId, Rect)> = ws
                    .floating
                    .windows()
                    .iter()
                    .map(|e| (e.window_id, e.rect))
                    .collect();
                for (wid, rect) in floats {
                    let window = self.registry.get_window(HWND(wid.0 as *mut _));
                    let Some(w) = window else {
                        ws.floating.remove(wid);
                        continue;
                    };
                    if !matches!(w.state, WindowState::Floating(FloatingState::Active { .. })) {
                        ws.floating.remove(wid);
                        continue;
                    }
                    if ws.id != active_id {
                        continue;
                    }
                    // Visible float: where the user left it.
                    let live = probe
                        .window_rect(wid.0)
                        .map(|r| w.invisible_bounds.window_to_visible(r))
                        .map(|r| r.clamped_into(work_area));
                    if let Some(live) = live
                        && live != rect
                    {
                        ws.floating.add(wid, live);
                        if let Some(w) = self.registry.get_window_mut(HWND(wid.0 as *mut _)) {
                            w.state = WindowState::Floating(FloatingState::Active { rect: live });
                        }
                        summary.floats += 1;
                    }
                }
            }
        }

        // 4. Focus: the foreground window picks the active monitor and
        //    workspace, so the user keeps looking at what they were using
        //    — chosen *before* the append below, so a window that appeared
        //    during the pause (on-screen right now) lands on the workspace
        //    that is about to be visible.
        let foreground = probe
            .foreground_window()
            .filter(|&fg| self.registry.is_tracked(fg));
        if let Some(fg) = foreground {
            self.focus_resume_target(fg);
        }

        // 5. Windows seated nowhere: tiles appended to the active workspace
        //    of the monitor Windows reports for them, floats seated there at
        //    their current rect.
        let unseated: Vec<(isize, WindowState)> = self
            .registry
            .windows()
            .filter(|w| {
                matches!(
                    w.state,
                    WindowState::Tiling(TilingState::Active { .. })
                        | WindowState::Floating(FloatingState::Active { .. })
                )
            })
            .map(|w| (w.hwnd.0 as isize, w.state.clone()))
            .collect();
        for (hwnd, state) in unseated {
            let wid = WindowId(hwnd);
            if self.monitor_index_containing(wid).is_some() {
                continue;
            }
            let by_handle = {
                let h = probe.monitor_handle_for_window(hwnd);
                if h == 0 {
                    None
                } else {
                    self.monitors.iter().position(|m| m.hmonitor() == h)
                }
            };
            let dest = by_handle
                .or_else(|| self.remembered_monitor(wid))
                .unwrap_or(self.active_monitor);
            match state {
                WindowState::Tiling(_) => {
                    let _ = self.monitors[dest].active_scrolling_mut().add_window(wid);
                    summary.appended += 1;
                    log::debug!("tiling: window {hwnd} appended to monitor {dest}'s active strip");
                }
                WindowState::Floating(FloatingState::Active { rect }) => {
                    let bounds = self
                        .registry
                        .get_window(HWND(hwnd as *mut _))
                        .map(|w| w.invisible_bounds)
                        .unwrap_or_default();
                    let rect = probe
                        .window_rect(hwnd)
                        .map(|r| bounds.window_to_visible(r))
                        .unwrap_or(rect)
                        .clamped_into(self.monitors[dest].work_area());
                    self.monitors[dest]
                        .active_workspace_mut()
                        .floating
                        .add(wid, rect);
                    if let Some(w) = self.registry.get_window_mut(HWND(hwnd as *mut _)) {
                        w.state = WindowState::Floating(FloatingState::Active { rect });
                    }
                    summary.floats += 1;
                    log::debug!("tiling: float {hwnd} seated on monitor {dest}");
                }
                _ => {}
            }
            self.remember_monitor(wid, dest);
        }
        // The foreground window may itself have been seated just now.
        if let Some(fg) = foreground {
            self.focus_resume_target(fg);
        }

        // 6. Seat everything at once, instantly: the windows are wherever
        //    Windows and the user left them, and a tween from there would
        //    sweep across the screen.
        summary.targets = self.seat_all_monitors_instantly();
        self.rebuild_float_hwnds();
        summary.monitors = self.monitors.len();

        self.tiling_paused = false;
        self.write_tiling_status(true);
        log::info!(
            "tiling: on — {} monitor(s), {} tile(s) kept their seat, {} appended, {} float(s) seated or refreshed, {} vanished window(s) dropped, {} target(s) seated{}",
            summary.monitors,
            summary.kept,
            summary.appended,
            summary.floats,
            summary.dropped,
            summary.targets,
            if summary.topology_reconciled {
                " (display change reconciled first)"
            } else {
                ""
            }
        );
        summary
    }

    /// Make the tracked (foreground) window `fg` the daemon's focus on
    /// resume: its monitor becomes active, its workspace becomes that
    /// monitor's active one, and a tile also becomes its strip's focus
    /// cursor and is scrolled into view. A window seated nowhere only gets
    /// the registry focus. Idempotent.
    fn focus_resume_target(&mut self, fg: isize) {
        self.registry.set_focused(fg);
        let wid = WindowId(fg);
        let Some(mi) = self.monitor_index_containing(wid) else {
            return;
        };
        self.set_active_monitor(mi);
        if let Some(ws_id) = self.monitors[mi].find_workspace_containing(wid)
            && ws_id != self.monitors[mi].active_workspace_id()
        {
            let _ = self.monitors[mi].set_active_workspace(ws_id);
        }
        if self.registry.is_tiling(fg) {
            let strip = self.monitors[mi].active_scrolling_mut();
            strip.set_focus(wid);
            let _ = strip.ensure_focused_visible();
        }
    }

    /// Mirror the tiling state into the status file (`tiling: on|off`), if
    /// this daemon has a reporter (unit tests do not).
    fn write_tiling_status(&self, enabled: bool) {
        if let Some(reporter) = &self.status_reporter {
            reporter.tiling_changed(enabled);
        }
    }
}

/// Append an instant target placing `hwnd` at `window_rect` (full window
/// rect, no border inset — the overlays are gone).
fn push_instant(targets: &mut Vec<WindowTarget>, hwnd: isize, window_rect: Rect) {
    targets.push(WindowTarget::instant(
        WindowRef(hwnd),
        IVec2 {
            x: window_rect.x,
            y: window_rect.y,
        },
        IVec2 {
            x: window_rect.width,
            y: window_rect.height,
        },
    ));
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

    use windows::Win32::Foundation::HWND;

    use super::*;
    use crate::config::types::WindowRulesConfig;
    use crate::ipc::message::SocketMessage;
    use crate::layout::types::{MonitorInfo, Padding, VirtualLayout};
    use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    const PRIMARY: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    const LEFT: Rect = Rect {
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
    };

    const A: isize = 0xA;
    const B: isize = 0xB;
    const C: isize = 0xC;
    const F: isize = 0xF;
    const G: isize = 0x6;
    const M: isize = 0x3;
    const N: isize = 0x7;

    /// A's rect before it was tiled.
    const A_PRE: Rect = Rect {
        x: 300,
        y: 200,
        width: 640,
        height: 480,
    };

    fn fake_info(hwnd: isize, rect: Rect) -> crate::registry::win32::WindowInfo {
        crate::registry::win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: format!("win{hwnd}"),
            class: "C".into(),
            rect,
            exe: "app.exe".into(),
            process_path: "C:\\app.exe".into(),
            is_visible: true,
            is_maximized: false,
            is_fullscreen: false,
        }
    }

    /// Register a fake window in `state` (no Win32).
    fn fake_window(registry: &mut WindowRegistry, hwnd: isize, state: WindowState) {
        registry.register_window_from_info(&fake_info(hwnd, r(50, 60, 800, 600)));
        if let Some(w) = registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = state;
        }
    }

    fn tile() -> WindowState {
        WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
    }

    fn float(rect: Rect) -> WindowState {
        WindowState::Floating(FloatingState::Active { rect })
    }

    fn monitor(hmonitor: isize, screen: Rect, others: &[Rect], virtual_screen: Rect) -> Monitor {
        let info = MonitorInfo::in_desktop(screen, screen, virtual_screen, others);
        let ws = |id: u32| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(
                    info,
                    940,
                    320,
                    100,
                    100,
                    Padding {
                        window_gap: 4,
                        up: 0,
                        down: 0,
                    },
                    4,
                ),
            )
        };
        Monitor::new(screen, screen, vec![ws(1), ws(2)], 0).with_hmonitor(hmonitor)
    }

    /// Primary (handle 1): workspace 1 (active) tiles A (with a pre-tiling
    /// rect) and C, float F on-screen; workspace 2 (parked) tiles B and
    /// floats G; M minimized from workspace 1. LEFT (handle 2) is empty.
    fn fixture() -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [A, B, C, M] {
            fake_window(&mut registry, hwnd, tile());
        }
        fake_window(&mut registry, F, float(r(100, 100, 400, 300)));
        fake_window(&mut registry, G, float(r(200, 200, 300, 200)));
        if let Some(w) = registry.get_window_mut(HWND(A as *mut _)) {
            w.pre_tiling_rect = Some(A_PRE);
        }
        if let Some(w) = registry.get_window_mut(HWND(M as *mut _)) {
            w.state = WindowState::Tiling(TilingState::Minimized);
            w.last_virtual_slot = Some(crate::registry::types::VirtualSlot { col: 1, row: 0 });
        }
        let vs = r(-1920, 0, 3840, 1080);
        let mut m0 = monitor(1, PRIMARY, &[LEFT], vs);
        let m1 = monitor(2, LEFT, &[PRIMARY], vs);
        m0.active_scrolling_mut().add_window(WindowId(A));
        m0.active_scrolling_mut().add_window(WindowId(C));
        m0.active_workspace_mut()
            .floating
            .add(WindowId(F), r(100, 100, 400, 300));
        {
            let ws2 = m0.workspace_mut(WorkspaceId(2)).expect("workspace 2");
            ws2.scrolling.add_window(WindowId(B));
            ws2.floating.add(WindowId(G), r(200, 200, 300, 200));
        }
        for hwnd in [A, B, C, F, G, M] {
            registry.set_monitor(hwnd, 0);
        }
        registry.set_focused(A);
        FlowWM::new_for_test(registry, vec![m0, m1])
    }

    /// A scripted desktop for [`TilingProbe`].
    #[derive(Default)]
    struct MockDesktop {
        /// Windows the re-scan registers (hwnd, rect, state override).
        scan: Vec<(isize, Rect, WindowState)>,
        /// Handles `IsWindow` says are gone.
        dead: HashSet<isize>,
        iconic: HashSet<isize>,
        /// hwnd → HMONITOR Windows reports for it.
        nearest: HashMap<isize, isize>,
        /// hwnd → current `GetWindowRect`.
        rects: HashMap<isize, Rect>,
        foreground: Option<isize>,
        /// Every placement fix attempted, in order.
        placements: RefCell<Vec<isize>>,
    }

    impl TopologyProbe for MockDesktop {
        fn snapshot(&self) -> Result<TopologySnapshot, String> {
            Err("no topology scripted".to_owned())
        }
        fn monitor_handle_for_window(&self, hwnd: isize) -> isize {
            self.nearest.get(&hwnd).copied().unwrap_or(0)
        }
        fn is_iconic(&self, hwnd: isize) -> bool {
            self.iconic.contains(&hwnd)
        }
        fn restore_window(&self, _hwnd: isize) -> bool {
            true
        }
        fn focus_window(&self, _hwnd: isize) -> bool {
            true
        }
    }

    impl TilingProbe for MockDesktop {
        fn scan_windows(&self, registry: &mut WindowRegistry) -> Result<(), String> {
            for (hwnd, rect, state) in &self.scan {
                registry.register_window_from_info(&fake_info(*hwnd, *rect));
                if let Some(w) = registry.get_window_mut(HWND(*hwnd as *mut _)) {
                    w.state = state.clone();
                }
            }
            Ok(())
        }
        fn is_window(&self, hwnd: isize) -> bool {
            !self.dead.contains(&hwnd)
        }
        fn window_rect(&self, hwnd: isize) -> Option<Rect> {
            self.rects.get(&hwnd).copied()
        }
        fn foreground_window(&self) -> Option<isize> {
            self.foreground
        }
        fn fix_minimized_placement(
            &self,
            hwnd: isize,
            _bounds: InvisibleBounds,
            _monitors: &[RescueMonitor],
            _owner: Option<usize>,
        ) -> Result<Option<(Rect, Rect)>, String> {
            self.placements.borrow_mut().push(hwnd);
            Ok(Some((r(-5000, 0, 10, 10), r(0, 0, 10, 10))))
        }
    }

    /// The fixture's desktop as Windows would report it: M is iconic,
    /// everything else is live and on-screen.
    fn desktop() -> MockDesktop {
        let mut d = MockDesktop::default();
        d.iconic.insert(M);
        d
    }

    fn strip(flow: &FlowWM, mi: usize, ws: u32) -> VirtualLayout {
        flow.monitors[mi]
            .workspace(WorkspaceId(ws))
            .expect("workspace")
            .scrolling
            .virtual_layout()
            .clone()
    }

    fn column_order(flow: &FlowWM, mi: usize, ws: u32) -> Vec<WindowId> {
        strip(flow, mi, ws)
            .columns
            .iter()
            .flat_map(|c| c.rows.iter().map(|r| r.window_id))
            .collect()
    }

    fn submitted_for(targets: &PauseTargets, hwnd: isize) -> Option<Rect> {
        targets
            .submitted
            .iter()
            .find(|(h, _)| *h == hwnd)
            .map(|(_, r)| *r)
    }

    // ── the pure rule ─────────────────────────────────────────────────

    #[test]
    fn pause_target_restores_the_anchor_or_clamps_the_layout_rect() {
        let screens = [PRIMARY, LEFT];
        let work_areas = [r(0, 0, 1920, 1040), r(-1920, 0, 1920, 1040)];
        // An anchor on the LEFT display is restored there, whoever owns
        // the window now.
        let (rect, restored) = pause_target(
            Some(r(-1500, 100, 400, 300)),
            r(4, 4, 960, 1032),
            0,
            &screens,
            &work_areas,
        );
        assert!(restored);
        assert_eq!(rect, r(-1500, 100, 400, 300));
        // An anchor off every display (an unplugged one) is clamped into
        // the owner's work area, same size.
        let (rect, restored) = pause_target(
            Some(r(5000, 100, 400, 300)),
            r(4, 4, 960, 1032),
            1,
            &screens,
            &work_areas,
        );
        assert!(restored);
        assert_eq!((rect.width, rect.height), (400, 300));
        assert!(LEFT.overlaps(rect) && rect.x >= -1920 && rect.right() <= 0);
        // No anchor: a visible tile stays put, a parked column comes to
        // the edge of the owner's work area.
        let (rect, restored) = pause_target(None, r(4, 4, 960, 1032), 0, &screens, &work_areas);
        assert!(!restored);
        assert_eq!(rect, r(4, 4, 960, 1032));
        let (rect, _) = pause_target(None, r(2400, 4, 960, 1032), 0, &screens, &work_areas);
        assert_eq!(rect, r(960, 4, 960, 1032));
    }

    // ── pause ─────────────────────────────────────────────────────────

    #[test]
    fn pause_moves_every_tile_on_screen_and_restores_pre_tiling_rects() {
        let mut flow = fixture();
        let targets = flow.pause_targets();
        assert!(targets.targets.iter().all(|t| t.instant));
        assert_eq!(targets.tiles, 3, "A, C and the parked B");
        assert_eq!(targets.restored, 1, "only A has a pre-tiling rect");
        // A goes back to its pre-tiling rect (zero invisible bounds here).
        assert_eq!(submitted_for(&targets, A), Some(A_PRE));
        // B lived on parked workspace 2: now inside the primary work area.
        let b = submitted_for(&targets, B).expect("B is moved");
        assert!(
            b.x >= 0 && b.right() <= 1920 && b.y >= 0 && b.bottom() <= 1080,
            "{b:?}"
        );
        // C was visible already: it stays exactly where the layout had it.
        let c_layout = flow.monitors[0]
            .active_scrolling()
            .actual_layout()
            .find(WindowId(C))
            .expect("C tiled")
            .rect;
        assert_eq!(submitted_for(&targets, C), Some(c_layout));
        // The on-screen float is left alone; the parked one is clamped in.
        assert_eq!(submitted_for(&targets, F), None);
        assert_eq!(targets.floats_clamped, 1);
        let g = submitted_for(&targets, G).expect("G is moved");
        assert!(
            PRIMARY.overlaps(g) && g.y >= 0 && g.bottom() <= 1080,
            "{g:?}"
        );

        let probe = desktop();
        let summary = flow.pause_tiling_with(&probe);
        assert_eq!(
            summary,
            PauseSummary {
                already_paused: false,
                tiles: 3,
                restored: 1,
                floats_clamped: 1,
                minimized_fixed: 1,
                borders: 0,
            }
        );
        assert!(flow.tiling_paused);
        assert!(!flow.tiling_enabled());
        // The minimized window's restore placement was fixed.
        assert_eq!(*probe.placements.borrow(), vec![M]);
        // The commanded rects are recorded, the bookkeeping is untouched.
        assert_eq!(flow.last_submitted_rect(A), Some(A_PRE));
        assert_eq!(column_order(&flow, 0, 1), vec![WindowId(A), WindowId(C)]);
        assert_eq!(column_order(&flow, 0, 2), vec![WindowId(B)]);
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(1));
        assert!(flow.registry.is_tiling_active(A));

        // Pausing again does nothing.
        let again = flow.pause_tiling_with(&probe);
        assert!(again.already_paused);
        assert_eq!(again.tiles, 0);
        assert_eq!(probe.placements.borrow().len(), 1);
    }

    #[test]
    fn commands_are_rejected_while_paused_and_the_toggle_reports_state() {
        let mut flow = fixture();
        // Everything works before the pause (a layout error, not a refusal).
        assert!(!matches!(
            flow.dispatch(&SocketMessage::SwitchWorkspace { workspace_id: 1 }),
            SocketResponse::Error { message } if message == TILING_OFF_MESSAGE
        ));

        // `tiling off` through the dispatcher (the production pause only
        // touches Win32 for the minimized placement, which fails harmlessly
        // on a fake handle).
        let resp = flow.dispatch(&SocketMessage::SetTiling { enabled: false });
        assert_eq!(
            resp,
            SocketResponse::Data {
                payload: serde_json::json!({ "tiling_enabled": false })
            }
        );
        assert!(flow.tiling_paused);

        let refused = |flow: &mut FlowWM, msg: SocketMessage| {
            let resp = flow.dispatch(&msg);
            assert_eq!(
                resp,
                SocketResponse::Error {
                    message: TILING_OFF_MESSAGE.to_owned()
                },
                "{msg:?} must be refused while tiling is off"
            );
        };
        refused(&mut flow, SocketMessage::FocusLeft);
        refused(
            &mut flow,
            SocketMessage::SwapColumn {
                direction: crate::common::Direction::Right,
            },
        );
        refused(
            &mut flow,
            SocketMessage::SwitchWorkspace { workspace_id: 2 },
        );
        refused(
            &mut flow,
            SocketMessage::MoveWindowToMonitor {
                direction: crate::common::Direction::Left,
            },
        );
        refused(&mut flow, SocketMessage::Center);
        refused(
            &mut flow,
            SocketMessage::SetWindow {
                mode: crate::ipc::message::WindowMode::Cycle,
            },
        );
        refused(&mut flow, SocketMessage::ReloadConfig);
        refused(&mut flow, SocketMessage::LoadoutLoad { path: None });
        refused(&mut flow, SocketMessage::CloseWindow);
        // The bookkeeping is still exactly as it was.
        assert_eq!(column_order(&flow, 0, 1), vec![WindowId(A), WindowId(C)]);
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(1));

        // Liveness and queries still answer, and say tiling is off.
        assert!(matches!(
            flow.dispatch(&SocketMessage::Ping),
            SocketResponse::Pong {
                tiling_enabled: false,
                monitors: 2,
                ..
            }
        ));
        match flow.dispatch(&SocketMessage::QueryWindowsAll) {
            SocketResponse::Data { payload } => {
                assert_eq!(payload["tiling_enabled"], serde_json::json!(false));
            }
            other => panic!("query must still answer, got {other:?}"),
        }
        // `tiling off` again is a no-op that still reports the state.
        assert_eq!(
            flow.dispatch(&SocketMessage::SetTiling { enabled: false }),
            SocketResponse::Data {
                payload: serde_json::json!({ "tiling_enabled": false })
            }
        );
    }

    // ── while paused: hooks only touch the registry ───────────────────

    #[test]
    fn hooks_do_not_move_windows_while_paused() {
        let mut flow = fixture();
        flow.pause_tiling_with(&desktop());
        let before_1 = strip(&flow, 0, 1);
        let before_2 = strip(&flow, 0, 2);

        // Minimize a parked tile: registry only, the strip keeps its seat.
        flow.on_window_minimized(B);
        assert!(matches!(
            flow.registry
                .get_window(HWND(B as *mut _))
                .map(|w| w.state.clone()),
            Some(WindowState::Tiling(TilingState::Minimized))
        ));
        assert_eq!(strip(&flow, 0, 2), before_2);
        // ... and restoring it is registry only too.
        flow.on_window_restored(B);
        assert!(flow.registry.is_tiling_active(B));
        assert_eq!(strip(&flow, 0, 2), before_2);

        // Focus on a window of the parked workspace: tracked, no switch.
        flow.on_focus_changed(B);
        assert_eq!(flow.registry.focused(), Some(WindowId(B)));
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(1));
        assert_eq!(flow.active_monitor, 0);

        // Hide: registry only.
        flow.on_window_hidden(A);
        assert_eq!(strip(&flow, 0, 1), before_1);

        // A drag never starts.
        flow.on_drag_start(C);
        assert!(flow.drag_state.is_none());

        // An unknown window is not seated (and, with no desktop, not even
        // registered).
        assert!(!flow.on_window_created(0x999));
        assert!(!flow.registry.is_tracked(0x999));
        assert_eq!(strip(&flow, 0, 1), before_1);

        // Nothing was re-submitted: the pause rects stand.
        assert_eq!(flow.last_submitted_rect(A), Some(A_PRE));

        // Destroy: forgotten everywhere, nothing animated.
        flow.on_window_destroyed(C);
        assert!(!flow.registry.is_tracked(C));
        assert_eq!(column_order(&flow, 0, 1), vec![WindowId(A)]);
        assert!(flow.tiling_paused);
    }

    // ── resume ────────────────────────────────────────────────────────

    #[test]
    fn resume_reseats_remembered_slots_and_appends_a_newly_discovered_window() {
        let mut flow = fixture();
        flow.pause_tiling_with(&desktop());

        // During the pause: N appeared (on the primary), C was closed
        // without its DESTROY reaching us, the user is looking at B.
        let mut probe = desktop();
        probe.scan.push((N, r(10, 10, 500, 400), tile()));
        probe.nearest.insert(N, 1);
        probe.dead.insert(C);
        probe.foreground = Some(B);

        let summary = flow.resume_tiling_with(&probe);
        assert!(!summary.already_tiling);
        assert!(!summary.topology_reconciled);
        assert_eq!(summary.monitors, 2);
        assert_eq!(summary.kept, 2, "A and B kept their seats");
        assert_eq!(summary.appended, 1, "N");
        assert_eq!(summary.dropped, 1, "C");
        assert!(summary.targets > 0);
        assert!(!flow.tiling_paused);
        assert!(flow.tiling_enabled());

        // A kept column 0 of workspace 1; C is gone; B kept workspace 2.
        assert_eq!(column_order(&flow, 0, 1), vec![WindowId(A)]);
        assert_eq!(column_order(&flow, 0, 2), vec![WindowId(B), WindowId(N)]);
        assert!(!flow.registry.is_tracked(C));
        // N went to the active workspace of the monitor it is on — which
        // is workspace 2 now, because the foreground window B lives there.
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(2));
        assert_eq!(flow.registry.focused(), Some(WindowId(B)));
        assert_eq!(flow.registry.remembered_monitor(N), Some(0));
        assert!(flow.registry.is_tiling_active(N));
        // N's first seating recorded its pre-tiling anchor.
        assert_eq!(
            flow.registry
                .get_window(HWND(N as *mut _))
                .and_then(|w| w.pre_tiling_rect),
            Some(r(10, 10, 500, 400))
        );
        // Floats are still seated; the registry slots are synced.
        assert!(
            flow.monitors[0]
                .active_workspace()
                .floating
                .contains(WindowId(G))
        );
        assert!(
            flow.monitors[0]
                .workspace(WorkspaceId(1))
                .expect("ws 1")
                .floating
                .contains(WindowId(F))
        );
        // Resuming again does nothing.
        assert!(flow.resume_tiling_with(&probe).already_tiling);
    }

    #[test]
    fn resume_refreshes_a_visible_float_the_user_moved_and_drops_a_minimized_tile() {
        let mut flow = fixture();
        flow.pause_tiling_with(&desktop());
        // The user dragged F while tiling was off, and minimized A.
        let mut probe = desktop();
        probe.rects.insert(F, r(700, 300, 400, 300));
        probe.iconic.insert(A);

        let summary = flow.resume_tiling_with(&probe);
        assert_eq!(summary.floats, 1);
        assert_eq!(summary.kept, 2, "C and B");
        let f = flow.monitors[0]
            .workspace(WorkspaceId(1))
            .expect("ws 1")
            .floating
            .windows()
            .iter()
            .find(|e| e.window_id == WindowId(F))
            .map(|e| e.rect);
        assert_eq!(f, Some(r(700, 300, 400, 300)));
        assert!(matches!(
            flow.registry.get_window(HWND(F as *mut _)).map(|w| w.state.clone()),
            Some(WindowState::Floating(FloatingState::Active { rect })) if rect == r(700, 300, 400, 300)
        ));
        // A is minimized now: out of its strip until its restore hook.
        assert_eq!(column_order(&flow, 0, 1), vec![WindowId(C)]);
        assert!(matches!(
            flow.registry
                .get_window(HWND(A as *mut _))
                .map(|w| w.state.clone()),
            Some(WindowState::Tiling(TilingState::Minimized))
        ));
        // ... and that hook appends it exactly once (the normal path again).
        flow.on_window_restored(A);
        assert_eq!(column_order(&flow, 0, 1), vec![WindowId(C), WindowId(A)]);
    }

    #[test]
    fn toggling_twice_returns_to_the_same_layout() {
        let mut flow = fixture();
        let before: Vec<VirtualLayout> = (0..2)
            .flat_map(|mi| [strip(&flow, mi, 1), strip(&flow, mi, 2)])
            .collect();
        let floats_before: Vec<Vec<(WindowId, Rect)>> = flow.monitors[0]
            .workspaces()
            .iter()
            .map(|ws| {
                ws.floating
                    .windows()
                    .iter()
                    .map(|e| (e.window_id, e.rect))
                    .collect()
            })
            .collect();

        let probe = desktop();
        let paused = flow.pause_tiling_with(&probe);
        assert!(!paused.already_paused);
        let resumed = flow.resume_tiling_with(&probe);
        assert!(!resumed.already_tiling);
        assert_eq!(resumed.appended, 0);
        assert_eq!(resumed.dropped, 0);

        let after: Vec<VirtualLayout> = (0..2)
            .flat_map(|mi| [strip(&flow, mi, 1), strip(&flow, mi, 2)])
            .collect();
        assert_eq!(after, before, "the strips survive an off/on cycle");
        let floats_after: Vec<Vec<(WindowId, Rect)>> = flow.monitors[0]
            .workspaces()
            .iter()
            .map(|ws| {
                ws.floating
                    .windows()
                    .iter()
                    .map(|e| (e.window_id, e.rect))
                    .collect()
            })
            .collect();
        assert_eq!(floats_after, floats_before);
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(1));
        assert_eq!(flow.active_monitor, 0);
        assert!(flow.registry.is_tiling_active(A));
        assert!(flow.tiling_enabled());
        // The seating recorded the layout rects again (A is back in its
        // column, not at its pre-tiling rect).
        let a_layout = flow.monitors[0]
            .active_scrolling()
            .actual_layout()
            .find(WindowId(A))
            .expect("A tiled")
            .rect;
        assert_eq!(flow.last_submitted_rect(A), Some(a_layout));
        assert_ne!(a_layout, A_PRE);

        // A second cycle lands in the same place too.
        flow.pause_tiling_with(&probe);
        flow.resume_tiling_with(&probe);
        let again: Vec<VirtualLayout> = (0..2)
            .flat_map(|mi| [strip(&flow, mi, 1), strip(&flow, mi, 2)])
            .collect();
        assert_eq!(again, before);
    }

    #[test]
    fn display_change_while_paused_is_deferred_to_the_resume() {
        let mut flow = fixture();
        flow.pause_tiling_with(&desktop());
        crate::registry::hooks::request_topology_refresh();
        flow.poll_topology_change();
        assert!(flow.topology_deferred);
        assert!(flow.topology_deadline.is_none());
        // Still paused, nothing re-seated: the strips are untouched.
        assert_eq!(column_order(&flow, 0, 1), vec![WindowId(A), WindowId(C)]);

        // The mock has no topology to offer: the reconcile fails, is logged,
        // and the resume carries on with the current monitor list.
        let summary = flow.resume_tiling_with(&desktop());
        assert!(!summary.topology_reconciled);
        assert!(!flow.topology_deferred);
        assert_eq!(flow.monitors.len(), 2);
        assert_eq!(column_order(&flow, 0, 1), vec![WindowId(A), WindowId(C)]);
    }
}
