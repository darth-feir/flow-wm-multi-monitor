//! Display-topology changes: monitor hotplug and work-area changes.
//!
//! The daemon enumerates monitors once at startup. Every parked column and
//! every parked workspace is positioned relative to that enumeration
//! (beyond the virtual screen, one monitor height away, …), so attaching or
//! detaching a display, changing a resolution, or moving the taskbar
//! invalidates all of it: windows keep being animated to a dead rect, or a
//! "parked" slot ends up inside a new neighbour. This module keeps the
//! monitor list in step with the desktop in three steps:
//!
//! 1. **Detect.** The session-sink window and the border overlays receive
//!    `WM_DISPLAYCHANGE` and `WM_SETTINGCHANGE(SPI_SETWORKAREA)` and call
//!    [`request_topology_refresh`](crate::registry::hooks::request_topology_refresh),
//!    which sets a flag and wakes the main loop. Windows sends several such
//!    messages per change, so the loop only re-arms a settle timer
//!    ([`TOPOLOGY_SETTLE`]) on each report and reconciles once the messages
//!    have stopped ([`FlowWM::poll_topology_change`]).
//! 2. **Reconcile** ([`FlowWM::reconcile_monitors`]). Re-enumerate, match the
//!    old monitors to the new ones ([`match_monitors`]: by `HMONITOR`, then
//!    by identical screen rect, then primary-to-primary), rebuild every
//!    survivor's [`MonitorInfo`] and re-project all of its workspaces in
//!    place, create fresh monitors with ten empty workspaces for added
//!    displays.
//! 3. **Migrate** ([`plan_migration`]). Every window of a vanished monitor —
//!    all ten workspaces — moves to the active workspace of its fallback
//!    monitor: the one Windows now reports for it (`MonitorFromWindow`),
//!    else the primary. Tiles are appended as columns; floats are clamped
//!    into the fallback's work area. Owner memory in the registry is
//!    remapped for every tracked window ([`remap_owner`]), including the
//!    minimized and hidden ones that live in no space. Windows that Windows
//!    itself minimized when their display went away are restored so they
//!    come back through the normal restore path, onto their new owner.
//!
//! Afterwards every monitor's workspaces are re-seated as **instant**
//! animator targets: the old parked slots are meaningless under the new
//! geometry and a tween from one of them would sweep across the screen.
//!
//! The Win32 questions the reconcile asks are behind [`TopologyProbe`] so
//! the whole pass is unit-tested with a mock desktop.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::animation::WindowTarget;
use crate::common::{Rect, WindowId};
use crate::layout::types::MonitorInfo;
use crate::registry::hooks;
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32::{self as registry_win32, MonitorGeometry};
use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId, WorkspaceParking};

use super::animation::PrevRects;
use super::config_derive;
use super::loadout::build_seating_batches;
use super::types::{FlowWM, LayoutConfig};

/// How long the display topology must stay quiet before the daemon
/// reconciles. Windows delivers a burst of `WM_DISPLAYCHANGE` /
/// `WM_SETTINGCHANGE` messages for one plug event (one per display, plus
/// work-area updates); handling them one by one would re-seat every window
/// several times.
pub(super) const TOPOLOGY_SETTLE: Duration = Duration::from_millis(300);

/// Workspaces created on a monitor that appeared after startup — the same
/// count `FlowWM::new` gives every monitor.
const WORKSPACE_COUNT: u32 = 10;

/// The settle deadline after one main-loop wake: a fresh report re-arms it
/// to `now + TOPOLOGY_SETTLE` (so the reconcile runs after the *last*
/// message of a burst), otherwise the previous deadline stands.
#[must_use]
pub(super) fn settle_deadline(
    previous: Option<Instant>,
    reported: bool,
    now: Instant,
) -> Option<Instant> {
    if reported {
        Some(now + TOPOLOGY_SETTLE)
    } else {
        previous
    }
}

/// Whether a settle deadline has arrived.
#[must_use]
pub(super) fn settle_due(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|d| now >= d)
}

/// Everything the reconcile reads from the desktop, as one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TopologySnapshot {
    /// Every attached monitor, primary first (the order the daemon's
    /// monitor list will have).
    pub geometries: Vec<MonitorGeometry>,
    /// Bounding box of all monitors.
    pub virtual_screen: Rect,
}

/// The Win32 questions [`FlowWM::reconcile_monitors_with`] asks, behind a
/// trait so tests can answer them without a desktop.
pub(super) trait TopologyProbe {
    /// Enumerate the monitors and the virtual screen.
    fn snapshot(&self) -> Result<TopologySnapshot, String>;
    /// `MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)` as an integer, or
    /// `0` when unknown.
    fn monitor_handle_for_window(&self, hwnd: isize) -> isize;
    /// `IsIconic(hwnd)`.
    fn is_iconic(&self, hwnd: isize) -> bool;
    /// `ShowWindow(hwnd, SW_RESTORE)`; `true` when issued on a live window.
    fn restore_window(&self, hwnd: isize) -> bool;
}

/// The production [`TopologyProbe`]: plain Win32.
pub(super) struct Win32Topology;

impl TopologyProbe for Win32Topology {
    fn snapshot(&self) -> Result<TopologySnapshot, String> {
        let geometries = registry_win32::enumerate_monitors()?;
        Ok(TopologySnapshot {
            geometries,
            virtual_screen: registry_win32::virtual_screen_rect(),
        })
    }

    fn monitor_handle_for_window(&self, hwnd: isize) -> isize {
        registry_win32::monitor_handle_for_window(HWND(hwnd as *mut _))
    }

    fn is_iconic(&self, hwnd: isize) -> bool {
        registry_win32::is_iconic(HWND(hwnd as *mut _))
    }

    fn restore_window(&self, hwnd: isize) -> bool {
        registry_win32::show_window_restored(hwnd)
    }
}

/// What identifies one of the daemon's current monitors for matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct MonitorKey {
    /// The `HMONITOR` the monitor was built with (`0` in tests).
    pub hmonitor: isize,
    /// Its full screen rect.
    pub screen_rect: Rect,
}

/// The correspondence between the daemon's monitors before a change and
/// the enumeration after it. Both directions are kept so callers can walk
/// either list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct MonitorMatch {
    /// Old index → new index; `None` for a monitor that vanished.
    pub old_to_new: Vec<Option<usize>>,
    /// New index → old index; `None` for a monitor that appeared.
    pub new_to_old: Vec<Option<usize>>,
}

impl MonitorMatch {
    /// Old indices of the monitors that vanished.
    #[must_use]
    pub fn removed(&self) -> Vec<usize> {
        (0..self.old_to_new.len())
            .filter(|&i| self.old_to_new[i].is_none())
            .collect()
    }

    /// New indices of the monitors that appeared.
    #[must_use]
    pub fn added(&self) -> Vec<usize> {
        (0..self.new_to_old.len())
            .filter(|&i| self.new_to_old[i].is_none())
            .collect()
    }

    fn link(&mut self, oi: usize, ni: usize) {
        self.old_to_new[oi] = Some(ni);
        self.new_to_old[ni] = Some(oi);
    }
}

/// Match the daemon's monitors (`old`, in daemon order: primary first) to
/// a fresh enumeration (`new`). Three passes, each only over monitors the
/// earlier passes left unmatched:
///
/// 1. same `HMONITOR` (a handle of `0` never matches);
/// 2. identical screen rect — Windows hands out a new handle when a display
///    is re-attached at the same position;
/// 3. the old primary (index 0) to the new primary — a resolution change
///    on a single display keeps the daemon's workspaces on it.
///
/// Every new monitor is matched at most once.
#[must_use]
pub(super) fn match_monitors(old: &[MonitorKey], new: &[MonitorGeometry]) -> MonitorMatch {
    let mut m = MonitorMatch {
        old_to_new: vec![None; old.len()],
        new_to_old: vec![None; new.len()],
    };
    // 1. handle
    for (oi, o) in old.iter().enumerate() {
        if o.hmonitor == 0 {
            continue;
        }
        if let Some(ni) = new
            .iter()
            .enumerate()
            .position(|(ni, n)| m.new_to_old[ni].is_none() && n.hmonitor == o.hmonitor)
        {
            m.link(oi, ni);
        }
    }
    // 2. identical rect
    for (oi, o) in old.iter().enumerate() {
        if m.old_to_new[oi].is_some() {
            continue;
        }
        if let Some(ni) = new
            .iter()
            .enumerate()
            .position(|(ni, n)| m.new_to_old[ni].is_none() && n.screen_rect == o.screen_rect)
        {
            m.link(oi, ni);
        }
    }
    // 3. primary ↔ primary
    if !old.is_empty()
        && m.old_to_new[0].is_none()
        && let Some(ni) = new
            .iter()
            .enumerate()
            .position(|(ni, n)| n.is_primary && m.new_to_old[ni].is_none())
    {
        m.link(0, ni);
    }
    m
}

/// The new index a window remembered on old monitor `old` now belongs to:
/// the monitor `old` became when it survived, else the monitor Windows
/// reports for the window (`nearest_new`, already translated to a new
/// index), else the primary.
#[must_use]
pub(super) fn remap_owner(
    old: usize,
    matching: &MonitorMatch,
    nearest_new: Option<usize>,
    primary_new: usize,
) -> usize {
    let new_count = matching.new_to_old.len();
    matching
        .old_to_new
        .get(old)
        .copied()
        .flatten()
        .or_else(|| nearest_new.filter(|&i| i < new_count))
        .unwrap_or(primary_new)
}

/// How a window was seated in the workspace it is being moved out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seat {
    /// A tiled window (a row in some column).
    Tile,
    /// A floating window at this visible rect.
    Float(Rect),
}

/// A window of a vanished monitor, before migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StrandedWindow {
    /// The window.
    pub window: WindowId,
    /// How it was seated.
    pub seat: Seat,
    /// The old index of the monitor it lived on.
    pub old_monitor: usize,
    /// New index of the monitor Windows now reports for it, if any.
    pub nearest_new: Option<usize>,
}

/// Where one stranded window goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Migration {
    /// The window.
    pub window: WindowId,
    /// How to seat it on the destination (a float's rect is already clamped
    /// into the destination work area).
    pub seat: Seat,
    /// New index of the destination monitor.
    pub to: usize,
}

/// Decide the destination of every window of a vanished monitor — see
/// [`remap_owner`] for the rule — and clamp float rects into the
/// destination's work area (`new_work_areas`, indexed by new index).
#[must_use]
pub(super) fn plan_migration(
    matching: &MonitorMatch,
    primary_new: usize,
    new_work_areas: &[Rect],
    stranded: &[StrandedWindow],
) -> Vec<Migration> {
    stranded
        .iter()
        .map(|s| {
            let to = remap_owner(s.old_monitor, matching, s.nearest_new, primary_new);
            let seat = match s.seat {
                Seat::Tile => Seat::Tile,
                Seat::Float(rect) => Seat::Float(match new_work_areas.get(to) {
                    Some(wa) => rect.clamped_into(*wa),
                    None => rect,
                }),
            };
            Migration {
                window: s.window,
                seat,
                to,
            }
        })
        .collect()
}

/// What one reconcile did, for the log line and the tests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct HotplugSummary {
    /// Monitors after the reconcile.
    pub monitors: usize,
    /// Monitors that appeared.
    pub added: usize,
    /// Monitors that vanished.
    pub removed: usize,
    /// Windows moved off vanished monitors.
    pub migrated: usize,
    /// Windows Windows had minimized on a vanished display that were
    /// restored.
    pub restored: usize,
}

impl FlowWM {
    /// Main-loop step for display-topology changes, run on every wake.
    ///
    /// Takes the dirty flag set by the window procedures, re-arms the settle
    /// deadline on each report, and runs [`reconcile_monitors`](Self::reconcile_monitors)
    /// once the deadline arrives. A reconcile during a tile drag is
    /// postponed by one settle period (the drag owns the layout), and one
    /// requested after a shutdown is skipped — the fail-back rescues with
    /// whatever geometry it has, and re-seating windows on the way out would
    /// only fight it.
    pub(super) fn poll_topology_change(&mut self) {
        let now = Instant::now();
        let reported = hooks::take_topology_dirty();
        self.topology_deadline = settle_deadline(self.topology_deadline, reported, now);
        if !settle_due(self.topology_deadline, now) {
            return;
        }
        if self.drag_state.is_some() {
            log::debug!("hotplug: tile drag in progress, postponing the reconcile");
            self.topology_deadline = Some(now + TOPOLOGY_SETTLE);
            return;
        }
        self.topology_deadline = None;
        if hooks::shutdown_requested() {
            log::info!("hotplug: shutdown requested, skipping the reconcile");
            return;
        }
        if let Err(e) = self.reconcile_monitors() {
            log::warn!("hotplug: reconcile failed, keeping the current monitor list: {e}");
        }
    }

    /// Bring the monitor list in step with the desktop (see the
    /// [module docs](self)), then refresh every border overlay for the new
    /// seating. Uses the live Win32 desktop.
    ///
    /// # Errors
    ///
    /// Enumeration failure (or an empty enumeration, which Windows reports
    /// transiently while a display is switching) — the current monitor list
    /// is left untouched.
    pub(super) fn reconcile_monitors(&mut self) -> Result<HotplugSummary, String> {
        let summary = self.reconcile_monitors_with(&Win32Topology)?;
        self.refresh_all_border_styles();
        Ok(summary)
    }

    /// [`reconcile_monitors`](Self::reconcile_monitors) against `probe`,
    /// without the border refresh (which creates overlay windows). This is
    /// the whole algorithm; see the [module docs](self) for the outline.
    pub(super) fn reconcile_monitors_with(
        &mut self,
        probe: &dyn TopologyProbe,
    ) -> Result<HotplugSummary, String> {
        let snap = probe.snapshot()?;
        if snap.geometries.is_empty() {
            return Err("no monitors enumerated".to_owned());
        }

        // Nothing may tween while the geometry changes under it: a batch
        // still in flight would keep steering windows to old parked slots.
        self.animator.cancel_and_wait();

        let old_keys: Vec<MonitorKey> = self
            .monitors
            .iter()
            .map(|m| MonitorKey {
                hmonitor: m.hmonitor(),
                screen_rect: m.screen_rect(),
            })
            .collect();
        let matching = match_monitors(&old_keys, &snap.geometries);
        let primary_new = snap
            .geometries
            .iter()
            .position(|g| g.is_primary)
            .unwrap_or(0);
        let new_index_for_handle = |h: isize| -> Option<usize> {
            if h == 0 {
                return None;
            }
            snap.geometries.iter().position(|g| g.hmonitor == h)
        };

        // Geometry and layout parameters for every monitor of the new desktop.
        let screen_rects: Vec<Rect> = snap.geometries.iter().map(|g| g.screen_rect).collect();
        let infos: Vec<MonitorInfo> = snap
            .geometries
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let others: Vec<Rect> = screen_rects
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, r)| *r)
                    .collect();
                MonitorInfo::in_desktop(g.work_area, g.screen_rect, snap.virtual_screen, &others)
            })
            .collect();
        let configs: Vec<LayoutConfig> = infos
            .iter()
            .map(|info| config_derive::derive_layout_config(&self.config, info))
            .collect();
        let columns_per_screen = self.config.columns_per_screen;

        // Survivors move into the new list (re-projected in place); added
        // monitors start empty; whatever is left in `old` has vanished.
        let mut old: Vec<Option<Monitor>> = std::mem::take(&mut self.monitors)
            .into_iter()
            .map(Some)
            .collect();
        let mut new_monitors: Vec<Monitor> = Vec::with_capacity(snap.geometries.len());
        for (ni, g) in snap.geometries.iter().enumerate() {
            let info = infos[ni];
            let lc = &configs[ni];
            let survivor = matching.new_to_old[ni].and_then(|oi| old[oi].take());
            match survivor {
                Some(mut m) => {
                    m.set_geometry(g.screen_rect, g.work_area);
                    m.set_hmonitor(g.hmonitor);
                    for ws in m.workspaces_mut() {
                        // Parameters first (an auto-derived column width follows
                        // the monitor), then the geometry: the row refit inside
                        // set_monitor_info uses the final padding.
                        let _ = ws.scrolling.reconfigure(
                            lc.column_width,
                            lc.min_column_width_px,
                            lc.min_window_height_px,
                            lc.min_row_height_px,
                            lc.padding,
                            columns_per_screen,
                        );
                        let _ = ws.scrolling.set_monitor_info(info);
                    }
                    new_monitors.push(m);
                }
                None => new_monitors.push(fresh_monitor(g, info, lc, columns_per_screen)),
            }
            log::info!(
                "hotplug: monitor {ni}: screen ({},{},{},{}) work ({},{},{},{}) primary={} neighbours L={} R={} U={} D={}{}",
                g.screen_rect.x,
                g.screen_rect.y,
                g.screen_rect.width,
                g.screen_rect.height,
                g.work_area.x,
                g.work_area.y,
                g.work_area.width,
                g.work_area.height,
                g.is_primary,
                info.neighbour_left,
                info.neighbour_right,
                info.neighbour_up,
                info.neighbour_down,
                match matching.new_to_old[ni] {
                    Some(oi) if oi != ni => format!(" (was monitor {oi})"),
                    Some(_) => String::new(),
                    None => " (new)".to_owned(),
                }
            );
        }

        // Every window of every workspace of every vanished monitor.
        let mut stranded: Vec<StrandedWindow> = Vec::new();
        for (oi, slot) in old.iter_mut().enumerate() {
            let Some(mut m) = slot.take() else {
                continue;
            };
            for ws in m.workspaces_mut() {
                let tiles: Vec<WindowId> = ws
                    .scrolling
                    .virtual_layout()
                    .columns
                    .iter()
                    .flat_map(|c| c.rows.iter().map(|r| r.window_id))
                    .collect();
                let floats: Vec<(WindowId, Rect)> = ws
                    .floating
                    .windows()
                    .iter()
                    .map(|e| (e.window_id, e.rect))
                    .collect();
                for wid in tiles {
                    stranded.push(StrandedWindow {
                        window: wid,
                        seat: Seat::Tile,
                        old_monitor: oi,
                        nearest_new: new_index_for_handle(probe.monitor_handle_for_window(wid.0)),
                    });
                }
                for (wid, rect) in floats {
                    stranded.push(StrandedWindow {
                        window: wid,
                        seat: Seat::Float(rect),
                        old_monitor: oi,
                        nearest_new: new_index_for_handle(probe.monitor_handle_for_window(wid.0)),
                    });
                }
            }
        }
        let new_work_areas: Vec<Rect> = snap.geometries.iter().map(|g| g.work_area).collect();
        let migrations = plan_migration(&matching, primary_new, &new_work_areas, &stranded);

        self.monitors = new_monitors;
        for mv in &migrations {
            match mv.seat {
                Seat::Tile => {
                    let _ = self.monitors[mv.to]
                        .active_scrolling_mut()
                        .add_window(mv.window);
                }
                Seat::Float(rect) => {
                    self.monitors[mv.to]
                        .active_workspace_mut()
                        .floating
                        .add(mv.window, rect);
                    if let Some(w) = self.registry.get_window_mut(HWND(mv.window.0 as *mut _))
                        && matches!(w.state, WindowState::Floating(FloatingState::Active { .. }))
                    {
                        w.state = WindowState::Floating(FloatingState::Active { rect });
                    }
                }
            }
            self.remember_monitor(mv.window, mv.to);
            log::debug!(
                "hotplug: window {} ({:?}) migrated to monitor {}",
                mv.window.0,
                mv.seat,
                mv.to
            );
        }

        // Owner memory: every tracked window that still remembers an old
        // index — including the minimized / hidden ones that live in no
        // space — is remapped with the same rule the migration used.
        let migrated: HashSet<isize> = migrations.iter().map(|m| m.window.0).collect();
        let remembered: Vec<(isize, usize)> = self
            .registry
            .windows()
            .filter_map(|w| w.monitor.map(|m| (w.hwnd.0 as isize, m)))
            .collect();
        for &(hwnd, old_idx) in &remembered {
            if migrated.contains(&hwnd) {
                continue;
            }
            let nearest = new_index_for_handle(probe.monitor_handle_for_window(hwnd));
            self.registry
                .set_monitor(hwnd, remap_owner(old_idx, &matching, nearest, primary_new));
        }

        // Windows that Windows minimized because their display went away:
        // anything we owned on a vanished monitor that is iconic now. They
        // are restored and come back through the normal restore hook, onto
        // the owner just recorded. (A window the user had minimized there
        // earlier comes back too — it has nowhere else to go.)
        let removed_set: HashSet<usize> = matching.removed().into_iter().collect();
        let candidates: HashSet<isize> = remembered
            .iter()
            .filter(|(_, old_idx)| removed_set.contains(old_idx))
            .map(|(hwnd, _)| *hwnd)
            .chain(migrated.iter().copied())
            .collect();
        let mut restored = 0_usize;
        for hwnd in candidates {
            let minimized = self
                .registry
                .get_window(HWND(hwnd as *mut _))
                .is_some_and(|w| {
                    matches!(
                        w.state,
                        WindowState::Tiling(TilingState::Minimized)
                            | WindowState::Floating(FloatingState::Minimized)
                    )
                });
            if (minimized || migrated.contains(&hwnd))
                && probe.is_iconic(hwnd)
                && probe.restore_window(hwnd)
            {
                restored += 1;
                log::debug!("hotplug: restored window {hwnd} minimized with its display");
            }
        }

        // The active monitor follows the focused window, else stays on the
        // monitor it was (if that survived), else the primary.
        let old_active = self.active_monitor;
        self.active_monitor = self
            .registry
            .focused()
            .and_then(|wid| self.monitor_index_containing(wid))
            .or_else(|| matching.old_to_new.get(old_active).copied().flatten())
            .unwrap_or(primary_new)
            .min(self.monitors.len() - 1);

        self.seat_all_monitors_instantly();
        self.rebuild_float_hwnds();

        let summary = HotplugSummary {
            monitors: self.monitors.len(),
            added: matching.added().len(),
            removed: matching.removed().len(),
            migrated: migrations.len(),
            restored,
        };
        log::info!(
            "hotplug: {} monitor(s) (+{} -{}), {} window(s) migrated{}",
            summary.monitors,
            summary.added,
            summary.removed,
            summary.migrated,
            if summary.restored > 0 {
                format!(", {} restored", summary.restored)
            } else {
                String::new()
            }
        );
        Ok(summary)
    }

    /// Instant animator targets seating every workspace of every monitor
    /// under the current geometry: the active workspace at its projected
    /// rects, the others at their [`WorkspaceParking`] slots. Instant
    /// because the windows' previous positions belong to a geometry that no
    /// longer exists.
    #[must_use]
    pub(super) fn instant_seating_targets(&self) -> Vec<WindowTarget> {
        let no_prev = HashMap::new();
        let mut targets = Vec::new();
        for (mi, m) in self.monitors.iter().enumerate() {
            let window_gap = m.active_scrolling().padding().window_gap;
            let parking =
                WorkspaceParking::new(m.active_scrolling().monitor(), m.screen_rect(), window_gap);
            let batches = build_seating_batches(m.workspaces(), m.active_workspace_id(), &parking);
            let (mut t, _) = self.workspace_targets(mi, &batches, PrevRects::Snapshot(&no_prev));
            for target in &mut t {
                target.instant = true;
            }
            targets.extend(t);
        }
        targets
    }

    /// Submit [`instant_seating_targets`](Self::instant_seating_targets) and
    /// sync the registry's tiling slots and (workspace-local) tiled rects
    /// from every workspace.
    fn seat_all_monitors_instantly(&mut self) {
        let targets = self.instant_seating_targets();
        if !targets.is_empty() {
            log::debug!(
                "hotplug: re-seating {} target(s) instantly across {} monitor(s)",
                targets.len(),
                self.monitors.len()
            );
            if let Err(e) = self.animator.animate(targets) {
                log::warn!("hotplug: re-seat animation error: {e}");
            }
        }
        for m in &self.monitors {
            for ws in m.workspaces() {
                self.registry
                    .update_tiling_slots_from_layout(ws.scrolling.virtual_layout());
                self.registry
                    .update_tiled_rects(ws.scrolling.actual_layout());
            }
        }
    }
}

/// A monitor that appeared after startup: ten empty workspaces, like the
/// ones `FlowWM::new` builds.
fn fresh_monitor(
    g: &MonitorGeometry,
    info: MonitorInfo,
    lc: &LayoutConfig,
    columns_per_screen: u32,
) -> Monitor {
    let workspaces: Vec<Workspace> = (1..=WORKSPACE_COUNT)
        .map(|id| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(
                    info,
                    lc.column_width,
                    lc.min_column_width_px,
                    lc.min_window_height_px,
                    lc.min_row_height_px,
                    lc.padding,
                    columns_per_screen,
                ),
            )
        })
        .collect();
    Monitor::new(g.screen_rect, g.work_area, workspaces, 0).with_hmonitor(g.hmonitor)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::HWND;

    use super::*;
    use crate::config::types::WindowRulesConfig;
    use crate::layout::types::Padding;
    use crate::registry::WindowRegistry;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn geo(hmonitor: isize, screen: Rect, is_primary: bool) -> MonitorGeometry {
        MonitorGeometry {
            screen_rect: screen,
            work_area: screen,
            hmonitor,
            is_primary,
        }
    }

    fn key(hmonitor: isize, screen: Rect) -> MonitorKey {
        MonitorKey {
            hmonitor,
            screen_rect: screen,
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
    const RIGHT: Rect = Rect {
        x: 1920,
        y: 0,
        width: 1920,
        height: 1080,
    };

    // ── settle timer ──────────────────────────────────────────────────

    #[test]
    fn settle_deadline_rearms_on_every_report_and_keeps_the_previous_one_otherwise() {
        let t0 = Instant::now();
        assert_eq!(settle_deadline(None, false, t0), None);
        let first = settle_deadline(None, true, t0);
        assert_eq!(first, Some(t0 + TOPOLOGY_SETTLE));
        // A second message 100 ms later pushes the deadline out again.
        let t1 = t0 + Duration::from_millis(100);
        let second = settle_deadline(first, true, t1);
        assert_eq!(second, Some(t1 + TOPOLOGY_SETTLE));
        // Quiet wakes leave it alone.
        assert_eq!(
            settle_deadline(second, false, t1 + Duration::from_millis(50)),
            second
        );
    }

    #[test]
    fn settle_due_only_once_the_deadline_has_arrived() {
        let t0 = Instant::now();
        let deadline = Some(t0 + TOPOLOGY_SETTLE);
        assert!(!settle_due(None, t0));
        assert!(!settle_due(deadline, t0));
        assert!(!settle_due(deadline, t0 + Duration::from_millis(299)));
        assert!(settle_due(deadline, t0 + TOPOLOGY_SETTLE));
        assert!(settle_due(deadline, t0 + Duration::from_secs(5)));
    }

    // ── matching ──────────────────────────────────────────────────────

    #[test]
    fn match_by_handle_first_even_when_the_rect_changed() {
        // Monitor 2 moved from the left to the right of the primary but
        // kept its handle: still the same monitor.
        let old = [key(1, PRIMARY), key(2, LEFT)];
        let new = [geo(1, PRIMARY, true), geo(2, RIGHT, false)];
        let m = match_monitors(&old, &new);
        assert_eq!(m.old_to_new, vec![Some(0), Some(1)]);
        assert_eq!(m.new_to_old, vec![Some(0), Some(1)]);
        assert!(m.added().is_empty());
        assert!(m.removed().is_empty());
    }

    #[test]
    fn match_by_identical_rect_when_the_handle_changed() {
        // Re-attached at the same position under a fresh HMONITOR.
        let old = [key(1, PRIMARY), key(2, LEFT)];
        let new = [geo(1, PRIMARY, true), geo(7, LEFT, false)];
        let m = match_monitors(&old, &new);
        assert_eq!(m.old_to_new, vec![Some(0), Some(1)]);
        assert!(m.added().is_empty());
        assert!(m.removed().is_empty());
    }

    #[test]
    fn match_primary_to_primary_as_the_last_resort() {
        // Single display, resolution changed, handle changed: the daemon's
        // only monitor must survive on the new primary.
        let old = [key(1, PRIMARY)];
        let new = [geo(9, r(0, 0, 2560, 1440), true)];
        let m = match_monitors(&old, &new);
        assert_eq!(m.old_to_new, vec![Some(0)]);
        assert_eq!(m.new_to_old, vec![Some(0)]);
    }

    #[test]
    fn match_reports_added_and_removed_sets() {
        // Left display detached, a right one attached.
        let old = [key(1, PRIMARY), key(2, LEFT)];
        let new = [geo(1, PRIMARY, true), geo(3, RIGHT, false)];
        let m = match_monitors(&old, &new);
        assert_eq!(m.old_to_new, vec![Some(0), None]);
        assert_eq!(m.new_to_old, vec![Some(0), None]);
        assert_eq!(m.removed(), vec![1]);
        assert_eq!(m.added(), vec![1]);
    }

    #[test]
    fn match_never_links_a_zero_handle_or_a_new_monitor_twice() {
        // Test monitors carry hmonitor 0: only rects can match them, and
        // two old monitors with the same rect cannot both take one new one.
        let old = [key(0, PRIMARY), key(0, PRIMARY)];
        let new = [geo(1, PRIMARY, true)];
        let m = match_monitors(&old, &new);
        assert_eq!(m.old_to_new, vec![Some(0), None]);
        assert_eq!(m.removed(), vec![1]);
    }

    // ── migration plan ────────────────────────────────────────────────

    fn removed_left() -> MonitorMatch {
        MonitorMatch {
            old_to_new: vec![Some(0), None],
            new_to_old: vec![Some(0)],
        }
    }

    #[test]
    fn remap_owner_survivor_then_nearest_then_primary() {
        let m = MonitorMatch {
            old_to_new: vec![Some(1), None, None],
            new_to_old: vec![None, Some(0)],
        };
        // Survivor: index follows the monitor.
        assert_eq!(remap_owner(0, &m, Some(0), 0), 1);
        // Vanished: MonitorFromWindow's answer wins.
        assert_eq!(remap_owner(1, &m, Some(0), 1), 0);
        // Vanished and no usable answer: primary.
        assert_eq!(remap_owner(1, &m, None, 1), 1);
        assert_eq!(remap_owner(2, &m, Some(9), 1), 1);
        // A stale index beyond the old list: primary.
        assert_eq!(remap_owner(7, &m, None, 1), 1);
    }

    #[test]
    fn plan_migration_routes_each_window_and_clamps_floats() {
        let m = removed_left();
        let work_areas = [r(0, 0, 1920, 1040)];
        let stranded = [
            StrandedWindow {
                window: WindowId(0xA),
                seat: Seat::Tile,
                old_monitor: 1,
                nearest_new: Some(0),
            },
            StrandedWindow {
                window: WindowId(0xB),
                seat: Seat::Tile,
                old_monitor: 1,
                nearest_new: None,
            },
            StrandedWindow {
                window: WindowId(0xF),
                seat: Seat::Float(r(-1500, 900, 400, 300)),
                old_monitor: 1,
                nearest_new: Some(0),
            },
        ];
        let plan = plan_migration(&m, 0, &work_areas, &stranded);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].to, 0);
        assert_eq!(plan[0].seat, Seat::Tile);
        assert_eq!(plan[1].to, 0);
        // The float landed inside the destination work area, same size.
        let Seat::Float(rect) = plan[2].seat else {
            panic!("float stayed a float");
        };
        assert_eq!((rect.width, rect.height), (400, 300));
        assert!(rect.x >= 0 && rect.right() <= 1920);
        assert!(rect.y >= 0 && rect.bottom() <= 1040);
    }

    // ── end to end with a mock desktop ────────────────────────────────

    /// A scripted desktop for [`TopologyProbe`].
    struct MockTopology {
        snap: TopologySnapshot,
        /// hwnd → HMONITOR Windows would report for it.
        nearest: HashMap<isize, isize>,
        iconic: HashSet<isize>,
        restored: RefCell<Vec<isize>>,
    }

    impl MockTopology {
        fn new(geometries: Vec<MonitorGeometry>) -> Self {
            let virtual_screen = geometries
                .iter()
                .map(|g| g.screen_rect)
                .reduce(|a, b| {
                    let x = a.x.min(b.x);
                    let y = a.y.min(b.y);
                    r(
                        x,
                        y,
                        a.right().max(b.right()) - x,
                        a.bottom().max(b.bottom()) - y,
                    )
                })
                .unwrap_or(PRIMARY);
            Self {
                snap: TopologySnapshot {
                    geometries,
                    virtual_screen,
                },
                nearest: HashMap::new(),
                iconic: HashSet::new(),
                restored: RefCell::new(Vec::new()),
            }
        }
    }

    impl TopologyProbe for MockTopology {
        fn snapshot(&self) -> Result<TopologySnapshot, String> {
            Ok(self.snap.clone())
        }
        fn monitor_handle_for_window(&self, hwnd: isize) -> isize {
            self.nearest.get(&hwnd).copied().unwrap_or(0)
        }
        fn is_iconic(&self, hwnd: isize) -> bool {
            self.iconic.contains(&hwnd)
        }
        fn restore_window(&self, hwnd: isize) -> bool {
            self.restored.borrow_mut().push(hwnd);
            true
        }
    }

    /// Register a fake window in `state` (no Win32).
    fn fake_window(registry: &mut WindowRegistry, hwnd: isize, state: WindowState) {
        let info = crate::registry::win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: format!("win{hwnd}"),
            class: "C".into(),
            rect: r(0, 0, 800, 600),
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

    /// A monitor with `hmonitor` at `screen` on a desktop whose other
    /// monitors are `others`, with workspaces 1 and 2 (1 active).
    fn monitor(hmonitor: isize, screen: Rect, others: &[Rect], virtual_screen: Rect) -> Monitor {
        let info = MonitorInfo::in_desktop(screen, screen, virtual_screen, others);
        let ws = |id: u32| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(
                    info,
                    960,
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

    const A: isize = 0xA;
    const B: isize = 0xB;
    const C: isize = 0xC;
    const D: isize = 0xD;
    const F: isize = 0xF;
    const M: isize = 0x3;

    /// Primary (handle 1) with A tiled; LEFT (handle 2) with B and C tiled
    /// and F floating on workspace 1, D tiled on parked workspace 2, and M
    /// minimized (remembered on the left monitor).
    fn side_by_side_flow() -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [A, B, C, D] {
            fake_window(&mut registry, hwnd, tile());
        }
        fake_window(
            &mut registry,
            F,
            WindowState::Floating(FloatingState::Active {
                rect: r(-1500, 100, 400, 300),
            }),
        );
        fake_window(
            &mut registry,
            M,
            WindowState::Tiling(TilingState::Minimized),
        );
        let vs = r(-1920, 0, 3840, 1080);
        let mut m0 = monitor(1, PRIMARY, &[LEFT], vs);
        let mut m1 = monitor(2, LEFT, &[PRIMARY], vs);
        m0.active_scrolling_mut().add_window(WindowId(A));
        m1.active_scrolling_mut().add_window(WindowId(B));
        m1.active_scrolling_mut().add_window(WindowId(C));
        m1.active_workspace_mut()
            .floating
            .add(WindowId(F), r(-1500, 100, 400, 300));
        m1.workspace_mut(WorkspaceId(2))
            .expect("workspace 2")
            .scrolling
            .add_window(WindowId(D));
        registry.set_monitor(A, 0);
        for hwnd in [B, C, D, F, M] {
            registry.set_monitor(hwnd, 1);
        }
        registry.set_focused(B);
        let mut flow = FlowWM::new_for_test(registry, vec![m0, m1]);
        flow.active_monitor = 1;
        flow
    }

    #[test]
    fn removed_monitor_migrates_every_window_and_reprojects_onto_the_survivor() {
        let mut flow = side_by_side_flow();
        let mut probe = MockTopology::new(vec![geo(1, PRIMARY, true)]);
        // Windows now reports the primary for everything that was on the
        // left display; M was minimized with it.
        for hwnd in [A, B, C, D, F, M] {
            probe.nearest.insert(hwnd, 1);
        }
        probe.iconic.insert(M);

        let summary = flow
            .reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(
            summary,
            HotplugSummary {
                monitors: 1,
                added: 0,
                removed: 1,
                migrated: 4,
                restored: 1,
            }
        );

        // One monitor left, its neighbour flag gone, virtual screen shrunk.
        assert_eq!(flow.monitors.len(), 1);
        let info = *flow.monitors[0].active_scrolling().monitor();
        assert!(!info.neighbour_left);
        assert_eq!(info.virtual_screen, PRIMARY);

        // Every tile (including D from the parked workspace) is a column of
        // the survivor's active strip; F is a float there, inside its work
        // area.
        let strip = flow.monitors[0].active_scrolling().virtual_layout();
        for hwnd in [A, B, C, D] {
            assert!(
                strip.find_window(WindowId(hwnd)).is_some(),
                "window {hwnd:#x} missing from the surviving strip"
            );
        }
        let floats = flow.monitors[0].active_workspace().floating.windows();
        assert_eq!(floats.len(), 1);
        assert_eq!(floats[0].window_id, WindowId(F));
        assert!(PRIMARY.overlaps(floats[0].rect));
        assert!(floats[0].rect.x >= 0);

        // Owner memory remapped for everyone, the minimized window included.
        for hwnd in [A, B, C, D, F, M] {
            assert_eq!(flow.registry.remembered_monitor(hwnd), Some(0), "{hwnd:#x}");
        }
        // The minimized window was restored (it comes back via the hook).
        assert_eq!(*probe.restored.borrow(), vec![M]);

        // Nothing projects onto the dead display: every tiled rect is inside
        // the primary or parked beyond its (now single-monitor) virtual
        // screen, never over the old left rect.
        for entry in &flow.monitors[0].active_scrolling().actual_layout().entries {
            assert!(
                !entry.rect.overlaps(LEFT),
                "{:?} still projects onto the removed display at {:?}",
                entry.window_id,
                entry.rect
            );
        }
        // The active monitor follows the focused window (B) onto 0.
        assert_eq!(flow.active_monitor, 0);

        // Re-seating targets are all instant.
        let targets = flow.instant_seating_targets();
        assert!(!targets.is_empty());
        assert!(targets.iter().all(|t| t.instant));
    }

    #[test]
    fn added_monitor_gets_ten_empty_workspaces_and_neighbours_are_updated() {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        fake_window(&mut registry, A, tile());
        let mut m0 = monitor(1, PRIMARY, &[], PRIMARY);
        m0.active_scrolling_mut().add_window(WindowId(A));
        registry.set_monitor(A, 0);
        let mut flow = FlowWM::new_for_test(registry, vec![m0]);

        let probe = MockTopology::new(vec![geo(1, PRIMARY, true), geo(5, RIGHT, false)]);
        let summary = flow
            .reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(
            summary,
            HotplugSummary {
                monitors: 2,
                added: 1,
                removed: 0,
                migrated: 0,
                restored: 0,
            }
        );

        assert_eq!(flow.monitors.len(), 2);
        let added = &flow.monitors[1];
        assert_eq!(added.hmonitor(), 5);
        assert_eq!(added.screen_rect(), RIGHT);
        assert_eq!(added.workspaces().len(), 10);
        assert!(
            added
                .workspaces()
                .iter()
                .all(|ws| ws.scrolling.virtual_layout().window_count() == 0
                    && ws.floating.is_empty())
        );
        // The survivor now knows about its right-hand neighbour and the
        // wider virtual screen; the newcomer knows about its left one.
        let m0_info = flow.monitors[0].active_scrolling().monitor();
        assert!(m0_info.neighbour_right);
        assert!(!m0_info.neighbour_left);
        assert_eq!(m0_info.virtual_screen, r(0, 0, 3840, 1080));
        assert!(added.active_scrolling().monitor().neighbour_left);
        // A stayed where it was.
        assert!(
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .find_window(WindowId(A))
                .is_some()
        );
        assert_eq!(flow.registry.remembered_monitor(A), Some(0));
        assert_eq!(flow.active_monitor, 0);
    }

    #[test]
    fn work_area_only_change_updates_rects_without_migrating() {
        let mut flow = side_by_side_flow();
        // The taskbar on the primary grew: same monitors, same handles,
        // smaller work area on monitor 0.
        let shrunk = r(0, 0, 1920, 1000);
        let mut primary = geo(1, PRIMARY, true);
        primary.work_area = shrunk;
        let probe = MockTopology::new(vec![primary, geo(2, LEFT, false)]);

        let summary = flow
            .reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(
            summary,
            HotplugSummary {
                monitors: 2,
                added: 0,
                removed: 0,
                migrated: 0,
                restored: 0,
            }
        );
        assert_eq!(flow.monitors[0].work_area(), shrunk);
        assert_eq!(flow.monitors[0].screen_rect(), PRIMARY);
        for ws in flow.monitors[0].workspaces() {
            assert_eq!(ws.scrolling.monitor().work_area, shrunk);
        }
        // A's projected rect respects the new work area.
        let a = flow.monitors[0]
            .active_scrolling()
            .actual_layout()
            .find(WindowId(A))
            .expect("A still tiled on monitor 0")
            .rect;
        assert!(
            a.bottom() <= shrunk.bottom(),
            "A projects to {a:?} outside the shrunk work area {shrunk:?}"
        );
        // Nothing moved monitors; owner memory unchanged.
        assert!(
            flow.monitors[1]
                .active_scrolling()
                .virtual_layout()
                .find_window(WindowId(B))
                .is_some()
        );
        assert_eq!(flow.registry.remembered_monitor(B), Some(1));
        assert_eq!(flow.registry.remembered_monitor(A), Some(0));
        assert_eq!(flow.active_monitor, 1);
        assert!(probe.restored.borrow().is_empty());
    }

    #[test]
    fn reattached_display_under_a_new_handle_keeps_its_workspaces() {
        let mut flow = side_by_side_flow();
        let probe = MockTopology::new(vec![geo(1, PRIMARY, true), geo(42, LEFT, false)]);
        let summary = flow
            .reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(summary.removed, 0);
        assert_eq!(summary.added, 0);
        assert_eq!(summary.migrated, 0);
        assert_eq!(flow.monitors[1].hmonitor(), 42);
        assert!(
            flow.monitors[1]
                .workspace(WorkspaceId(2))
                .expect("workspace 2")
                .scrolling
                .virtual_layout()
                .find_window(WindowId(D))
                .is_some()
        );
    }

    #[test]
    fn empty_enumeration_is_an_error_and_leaves_the_monitors_alone() {
        let mut flow = side_by_side_flow();
        let probe = MockTopology::new(Vec::new());
        assert!(flow.reconcile_monitors_with(&probe).is_err());
        assert_eq!(flow.monitors.len(), 2);
        assert_eq!(flow.active_monitor, 1);
    }
}
