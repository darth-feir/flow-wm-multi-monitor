//! Display-topology changes: monitor hotplug and work-area changes.
//!
//! Every parked column and workspace is positioned relative to the monitor
//! list enumerated at startup, so attaching or detaching a display, changing
//! a resolution or moving the taskbar invalidates all of it. This module
//! keeps the list in step with the desktop: **detect** (the window
//! procedures report `WM_DISPLAYCHANGE` / `SPI_SETWORKAREA`, debounced by
//! [`TOPOLOGY_SETTLE`] in [`FlowWM::poll_topology_change`]), **reconcile**
//! ([`FlowWM::reconcile_monitors`], matching old to new monitors with
//! [`match_monitors`]), **migrate** the windows of vanished monitors
//! ([`plan_migration`], [`remap_owner`]) and **restore the casualties**
//! Windows minimized ([`is_hotplug_casualty`]) — then re-seat everything as
//! instant animator targets. The full pass, the retry policy
//! ([`MAX_RECONCILE_RETRIES`]) and why restores use `SW_SHOWNOACTIVATE` are
//! in `docs/src/dev-guide/ipc-and-watchdog.md`, *Monitor hotplug*.
//!
//! The Win32 questions the reconcile asks are behind [`TopologyProbe`] so
//! the whole pass is unit-tested with a mock desktop.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::common::{Rect, WindowId};
use crate::config::width_preset::ScreenFacts;
use crate::layout::types::MonitorInfo;
use crate::registry::hooks;
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32::{self as registry_win32, ForegroundPush, MonitorGeometry};
use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId, WorkspaceParking};

use super::animation::{PrevRects, WorkspaceTargets};
use super::loadout::build_seating_batches;
use super::types::{FlowWM, LayoutConfig};
use super::width_preset;

/// How long the display topology must stay quiet before the daemon
/// reconciles. Windows delivers a burst of `WM_DISPLAYCHANGE` /
/// `WM_SETTINGCHANGE` messages for one plug event (one per display, plus
/// work-area updates); handling them one by one would re-seat every window
/// several times.
pub(super) const TOPOLOGY_SETTLE: Duration = Duration::from_millis(300);

/// How many times a failed reconcile (enumeration error, empty enumeration
/// mid-switch) is re-armed for another settle period before the daemon
/// gives up until the next display report.
pub(super) const MAX_RECONCILE_RETRIES: u8 = 5;

/// How long *before* the first topology report a minimize is still
/// attributed to the display change. Windows minimizes the windows of a
/// vanishing display as part of the same desktop rearrangement that
/// broadcasts `WM_DISPLAYCHANGE`, and the `MINIMIZESTART` hooks can reach
/// the daemon a little ahead of that broadcast.
pub(super) const HOTPLUG_MINIMIZE_GRACE: Duration = Duration::from_millis(1000);

/// Whether a minimized window is a hotplug casualty: it went minimized
/// (`minimized_at`, see [`Window::minimized_at`](crate::registry::types::Window::minimized_at))
/// at or after the first report of the pending topology change
/// (`first_report`, less [`HOTPLUG_MINIMIZE_GRACE`]). A window minimized
/// earlier was minimized by the user and stays that way; one that never
/// went through the minimize hook is not a casualty either.
#[must_use]
pub(super) fn is_hotplug_casualty(
    minimized_at: Option<Instant>,
    first_report: Option<Instant>,
) -> bool {
    match (minimized_at, first_report) {
        (Some(minimized), Some(first)) => minimized + HOTPLUG_MINIMIZE_GRACE >= first,
        _ => false,
    }
}

/// The retry counter after one failed reconcile: `Some(next)` when another
/// settle period should be armed, `None` when the budget
/// ([`MAX_RECONCILE_RETRIES`]) is spent and the daemon gives up.
#[must_use]
pub(super) fn next_reconcile_retry(retries: u8) -> Option<u8> {
    (retries < MAX_RECONCILE_RETRIES).then_some(retries + 1)
}

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
    /// `ShowWindow(hwnd, SW_SHOWNOACTIVATE)` — restore from minimized
    /// without activating; `true` when issued on a live window.
    fn restore_window(&self, hwnd: isize) -> bool;
    /// `SetForegroundWindow(hwnd)`, with its failure classified
    /// ([`ForegroundPush`]): an invalid handle is a dead window.
    fn focus_window(&self, hwnd: isize) -> ForegroundPush;
    /// `IsWindowVisible(hwnd) && !cloaked` — a hidden or cloaked window is
    /// skipped as a focus candidate, not dropped.
    fn is_visible(&self, hwnd: isize) -> bool;
    /// `IsWindow(hwnd)` — whether the handle still names a window. Every
    /// "focus the remembered window" path checks this before pushing the
    /// foreground, so a window that died without its `DESTROY` reaching the
    /// layout is purged instead of handed to `SetForegroundWindow`.
    fn is_window(&self, hwnd: isize) -> bool;
}

/// The production [`TopologyProbe`]: plain Win32.
pub(super) struct Win32Topology;

impl TopologyProbe for Win32Topology {
    fn is_window(&self, hwnd: isize) -> bool {
        registry_win32::is_window(hwnd)
    }

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

    fn focus_window(&self, hwnd: isize) -> ForegroundPush {
        registry_win32::push_foreground_window(hwnd)
    }

    fn is_visible(&self, hwnd: isize) -> bool {
        registry_win32::is_visible_uncloaked(hwnd)
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
    /// Hotplug casualties (windows Windows minimized with the display
    /// change) that were restored.
    pub restored: usize,
}

/// A minimized window the reconcile restores, in seating order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Casualty {
    /// Old index of the monitor it was remembered on (`usize::MAX` when
    /// none), the first sort key.
    old_monitor: usize,
    /// Its saved column and row, the second key.
    col: usize,
    /// See `col`.
    row: usize,
    /// The window (last key, for a deterministic order).
    hwnd: isize,
    /// Whether it was a tile (else a float, which keeps its float seat).
    tile: bool,
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
        if reported && self.topology_first_report.is_none() {
            self.topology_first_report = Some(now);
        }
        if self.tiling_paused {
            // Tiling is off: the desktop is Windows' to arrange, so nothing
            // is re-seated now. The change is remembered (and the first
            // report's instant kept for the casualty rule) and reconciled
            // when tiling resumes — see `daemon::tiling`.
            if (reported || self.topology_deadline.is_some()) && !self.topology_deferred {
                log::info!(
                    "hotplug: display change while tiling is off; reconcile deferred until tiling resumes"
                );
            }
            self.topology_deferred |= reported || self.topology_deadline.is_some();
            self.topology_deadline = None;
            self.topology_retries = 0;
            return;
        }
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
            self.topology_first_report = None;
            self.topology_retries = 0;
            return;
        }
        match self.reconcile_monitors() {
            Ok(_) => {
                self.topology_first_report = None;
                self.topology_retries = 0;
            }
            Err(e) => match next_reconcile_retry(self.topology_retries) {
                Some(next) => {
                    self.topology_retries = next;
                    self.topology_deadline = Some(now + TOPOLOGY_SETTLE);
                    log::warn!(
                        "hotplug: reconcile failed, retry {next}/{MAX_RECONCILE_RETRIES} in {} ms: {e}",
                        TOPOLOGY_SETTLE.as_millis()
                    );
                }
                None => {
                    self.topology_first_report = None;
                    self.topology_retries = 0;
                    log::error!(
                        "hotplug: reconcile failed {MAX_RECONCILE_RETRIES} times, keeping the current monitor list until the next display change: {e}"
                    );
                }
            },
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
        // Layout parameters per monitor: a survivor keeps the width preset
        // it was on when its (possibly new) list still has it, else lands on
        // that list's default; a new monitor starts on its list's default;
        // without presets the base width follows `columns_per_screen` /
        // `column_width` on each monitor's own geometry. A list that does
        // not fit a screen (a display that appeared with no rule for it) is
        // only warned about here — the floor keeps the columns usable.
        let facts: Vec<ScreenFacts> = snap
            .geometries
            .iter()
            .map(|g| width_preset::screen_facts(g.work_area, &g.device_name()))
            .collect();
        let presets: Vec<usize> = facts
            .iter()
            .enumerate()
            .map(|(ni, f)| {
                let Some(columns) = self.columns_config() else {
                    return 0;
                };
                if let Err(e) = columns.check_screen(
                    f,
                    self.config.padding.window_gap,
                    self.config.min_column_width_px,
                ) {
                    log::warn!("hotplug: {e}; narrow terms are floored on that screen");
                }
                let previous = matching.new_to_old[ni].and_then(|oi| self.monitor_preset(oi));
                width_preset::resolve_for_screen(
                    columns,
                    f,
                    previous,
                    self.config.padding.window_gap,
                    self.config.min_column_width_px,
                )
                .0
            })
            .collect();
        let configs: Vec<LayoutConfig> = infos
            .iter()
            .enumerate()
            .map(|(ni, info)| self.layout_config_for(info, &facts[ni], presets[ni]))
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
                    m.set_device_name(g.device_name());
                    m.set_width_preset(presets[ni]);
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
                        // The preset's pattern on the new work area (pins
                        // the columns by position when it has terms).
                        let _ = ws.scrolling.set_width_pattern(&lc.width_pattern_px);
                        // Floats keep their rect across a geometry change
                        // unless it no longer fits the work area (a display
                        // that shrank, a taskbar that grew): clamp them in
                        // like migrated floats, on every workspace.
                        for (wid, rect) in clamp_floats_into(ws, g.work_area) {
                            self.mirror_float_rect(wid, rect);
                        }
                    }
                    new_monitors.push(m);
                }
                None => new_monitors.push(
                    fresh_monitor(g, info, lc, columns_per_screen).with_width_preset(presets[ni]),
                ),
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
            // A window can only have one seat: skip one that is somehow
            // already in a surviving space (add_window has no membership
            // check of its own).
            if self.monitor_index_containing(mv.window).is_some() {
                continue;
            }
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
                    self.mirror_float_rect(mv.window, rect);
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

        // Hotplug casualties (windows Windows minimized because their nearest
        // display vanished) are restored without activation and seated here,
        // in their old (monitor, column, row) order, so the strip comes back
        // as it was; the later MINIMIZEEND hooks find them seated.
        let first_report = self.topology_first_report;
        let old_owner: HashMap<isize, usize> = remembered.iter().copied().collect();
        let mut casualties: Vec<Casualty> = self
            .registry
            .windows()
            .filter_map(|w| {
                let hwnd = w.hwnd.0 as isize;
                let tile = match w.state {
                    WindowState::Tiling(TilingState::Minimized) => true,
                    WindowState::Floating(FloatingState::Minimized) => false,
                    _ => return None,
                };
                if !is_hotplug_casualty(w.minimized_at, first_report) {
                    return None;
                }
                let (col, row) = w
                    .last_virtual_slot
                    .as_ref()
                    .map_or((usize::MAX, 0), |s| (s.col, s.row));
                Some(Casualty {
                    old_monitor: old_owner.get(&hwnd).copied().unwrap_or(usize::MAX),
                    col,
                    row,
                    hwnd,
                    tile,
                })
            })
            .collect();
        casualties.sort_unstable();
        let mut restored = 0_usize;
        for c in casualties {
            if !probe.is_iconic(c.hwnd) || !probe.restore_window(c.hwnd) {
                continue;
            }
            self.registry.restore_window(c.hwnd);
            restored += 1;
            let wid = WindowId(c.hwnd);
            if !c.tile || self.monitor_index_containing(wid).is_some() {
                // A float kept its float seat while minimized.
                continue;
            }
            // Back into its old column slot when its strip survived (the
            // owner memory still names that monitor), appended in slot
            // order otherwise — behind the migrated tiles, like the
            // migration itself appends.
            let dest = self
                .remembered_monitor(wid)
                .unwrap_or(primary_new)
                .min(self.monitors.len() - 1);
            let strip_survived =
                matching.old_to_new.get(c.old_monitor).copied().flatten() == Some(dest);
            let slot = if strip_survived { c.col } else { usize::MAX };
            let _ = self.monitors[dest]
                .active_scrolling_mut()
                .seat_window_at(slot, wid);
            self.remember_monitor(wid, dest);
            log::debug!(
                "hotplug: restored window {} minimized with the display change, seated on monitor {dest} at column {}",
                c.hwnd,
                if strip_survived {
                    c.col.to_string()
                } else {
                    "end".to_owned()
                }
            );
        }
        // A tile of a vanished monitor that is iconic but was not minimized
        // through our hook (state still Active): restore it too — it is
        // already seated by the migration.
        for &hwnd in &migrated {
            if self.registry.is_tiling_active(hwnd)
                && probe.is_iconic(hwnd)
                && probe.restore_window(hwnd)
            {
                restored += 1;
                log::debug!("hotplug: restored migrated window {hwnd} that was still iconic");
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

        // A fullscreen window's column travelled with it: flag it again in
        // whichever strip it landed (ADR-0021).
        self.resync_fullscreen_columns();
        self.seat_all_monitors_instantly();
        self.rebuild_float_hwnds();
        self.sync_all_top_layer();
        self.seat_all_piles();

        // Restores never activate, but push the foreground once to where the
        // registry says it belongs so a restore cannot have left it elsewhere.
        // The registry's focused window is used when it is still a window
        // and seated; otherwise the active monitor's next live candidate is
        // (dead ones purged on the way), like `focus_monitor`.
        if restored > 0 {
            let active = self.active_monitor;
            let remembered = self.registry.focused().filter(|f| {
                probe.is_window(f.0)
                    && probe.is_visible(f.0)
                    && self.monitor_index_containing(*f).is_some()
            });
            let pushed = match remembered {
                Some(f) => match probe.focus_window(f.0) {
                    ForegroundPush::Focused => true,
                    ForegroundPush::InvalidHandle => {
                        // Windows reports ERROR_INVALID_HANDLE for windows it
                        // merely refuses to activate as well as for dead ones;
                        // purge only when the window is really gone, otherwise
                        // leave it seated and fall back to the next candidate.
                        if probe.is_window(f.0) {
                            log::debug!(
                                "hotplug: SetForegroundWindow rejected live hwnd {} ({}); leaving it seated",
                                f.0,
                                self.describe_window(f.0)
                            );
                        } else {
                            log::debug!(
                                "hotplug: hwnd {} vanished during the focus push ({}); dropping it",
                                f.0,
                                self.describe_window(f.0)
                            );
                            self.purge_dead_window(active, f);
                        }
                        false
                    }
                    ForegroundPush::Failed(code) => {
                        log::warn!(
                            "hotplug: could not push focus back to hwnd {} (Win32 error {code:#x}; {})",
                            f.0,
                            self.describe_window(f.0)
                        );
                        true
                    }
                },
                None => false,
            };
            if !pushed {
                // The active monitor's next live, visible candidate — dead
                // ones purged on the way — else nothing (logged at debug).
                let picked = self.resolve_focus_target_with(
                    active,
                    |h| probe.is_window(h),
                    |h| probe.is_visible(h),
                    |h| probe.focus_window(h),
                );
                if let Some(id) = picked {
                    self.registry.set_focused(id.0);
                }
            }
        }

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
    pub(super) fn instant_seating_targets(&self) -> WorkspaceTargets {
        let no_prev = HashMap::new();
        let mut out = WorkspaceTargets::default();
        for (mi, m) in self.monitors.iter().enumerate() {
            let window_gap = m.active_scrolling().padding().window_gap;
            let parking =
                WorkspaceParking::new(m.active_scrolling().monitor(), m.screen_rect(), window_gap);
            let batches = build_seating_batches(m.workspaces(), m.active_workspace_id(), &parking);
            let WorkspaceTargets {
                mut targets,
                has_float_target,
                submitted,
            } = self.workspace_targets(mi, &batches, PrevRects::Snapshot(&no_prev));
            for target in &mut targets {
                target.instant = true;
            }
            out.targets.extend(targets);
            out.has_float_target |= has_float_target;
            out.submitted.extend(submitted);
        }
        out
    }

    /// Submit [`instant_seating_targets`](Self::instant_seating_targets),
    /// record them as every window's last submitted rect, and sync the
    /// registry's tiling slots and (workspace-local) tiled rects from every
    /// workspace. Shared by the hotplug reconcile and the tiling resume.
    /// Returns the number of targets submitted.
    pub(super) fn seat_all_monitors_instantly(&mut self) -> usize {
        let WorkspaceTargets {
            targets, submitted, ..
        } = self.instant_seating_targets();
        let count = targets.len();
        if !targets.is_empty() {
            log::debug!(
                "re-seating {} target(s) instantly across {} monitor(s)",
                targets.len(),
                self.monitors.len()
            );
            if let Err(e) = self.animator.animate(targets) {
                log::warn!("re-seat animation error: {e}");
            }
        }
        self.record_submitted(&submitted);
        for m in &self.monitors {
            for ws in m.workspaces() {
                self.registry
                    .update_tiling_slots_from_layout(ws.scrolling.virtual_layout());
                self.registry
                    .update_tiled_rects(ws.scrolling.actual_layout());
            }
        }
        count
    }
}

impl FlowWM {
    /// Mirror a float's new rect into the registry's
    /// [`FloatingState::Active`] record (a non-active float is left alone).
    fn mirror_float_rect(&mut self, wid: WindowId, rect: Rect) {
        if let Some(w) = self.registry.get_window_mut(HWND(wid.0 as *mut _))
            && matches!(w.state, WindowState::Floating(FloatingState::Active { .. }))
        {
            w.state = WindowState::Floating(FloatingState::Active { rect });
        }
    }
}

/// Clamp every float of `ws` into `work_area` (same size, moved inside —
/// [`Rect::clamped_into`]) and return the ones that moved, so the caller can
/// mirror them into the registry.
fn clamp_floats_into(ws: &mut Workspace, work_area: Rect) -> Vec<(WindowId, Rect)> {
    let moved: Vec<(WindowId, Rect)> = ws
        .floating
        .windows()
        .iter()
        .filter_map(|e| {
            let clamped = e.rect.clamped_into(work_area);
            (clamped != e.rect).then_some((e.window_id, clamped))
        })
        .collect();
    for &(wid, rect) in &moved {
        ws.floating.add(wid, rect);
    }
    moved
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
                )
                .with_row_limits(lc.visible_rows, lc.max_rows)
                .with_width_pattern(&lc.width_pattern_px)
                .with_layout(lc.layout_mode, lc.wrap_around),
            )
        })
        .collect();
    Monitor::new(g.screen_rect, g.work_area, workspaces, 0)
        .with_hmonitor(g.hmonitor)
        .with_device_name(g.device_name())
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
            device: [0; 32],
        }
        .with_device_name(&format!(r"\\.\DISPLAY{hmonitor}"))
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
        /// Every `focus_window` call, in order.
        focused: RefCell<Vec<isize>>,
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
                focused: RefCell::new(Vec::new()),
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
        fn focus_window(&self, hwnd: isize) -> ForegroundPush {
            self.focused.borrow_mut().push(hwnd);
            ForegroundPush::Focused
        }
        fn is_visible(&self, _hwnd: isize) -> bool {
            true
        }
        fn is_window(&self, _hwnd: isize) -> bool {
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
            is_iconic: false,
        };
        registry.register_window_from_info(&info);
        if let Some(w) = registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = state;
        }
    }

    fn tile() -> WindowState {
        WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
    }

    /// Mark `hwnd` as a tile minimized at `at` from column `col`, row `row`
    /// — what the minimize hook records.
    fn minimized_tile(
        registry: &mut WindowRegistry,
        hwnd: isize,
        col: usize,
        row: usize,
        at: Instant,
    ) {
        if let Some(w) = registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = WindowState::Tiling(TilingState::Minimized);
            w.last_virtual_slot = Some(crate::registry::types::VirtualSlot { col, row });
            w.minimized_at = Some(at);
        }
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
        fake_window(&mut registry, M, tile());
        // M was minimized by Windows with the display change.
        minimized_tile(&mut registry, M, 2, 0, Instant::now());
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
        // The first display report arrived just before the fixture's
        // minimize stamps.
        flow.topology_first_report = Some(Instant::now() - Duration::from_millis(50));
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
        // The minimized window was restored and seated by the reconcile
        // itself, behind the migrated tiles, as an active tile.
        assert_eq!(*probe.restored.borrow(), vec![M]);
        assert!(flow.registry.is_tiling_active(M));
        let strip = flow.monitors[0].active_scrolling().virtual_layout();
        assert_eq!(strip.find_window(WindowId(M)).map(|(c, _)| c), Some(4));
        // Focus was pushed once, to the registry's focused window (B).
        assert_eq!(*probe.focused.borrow(), vec![B]);

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

        // Re-seating targets are all instant, and every one was recorded as
        // the window's last submitted rect.
        let seating = flow.instant_seating_targets();
        assert!(!seating.targets.is_empty());
        assert!(seating.targets.iter().all(|t| t.instant));
        for (hwnd, rect) in &seating.submitted {
            assert_eq!(flow.last_submitted_rect(*hwnd), Some(*rect));
        }
    }

    // ── casualties: order, slots, timing, duplicates ─────────────────

    #[test]
    fn casualty_predicate_follows_the_first_report() {
        let t0 = Instant::now();
        let first = Some(t0);
        // Minimized after the first report: a casualty.
        assert!(is_hotplug_casualty(
            Some(t0 + Duration::from_millis(10)),
            first
        ));
        // Just before it, within the grace: still a casualty.
        assert!(is_hotplug_casualty(
            Some(t0 - HOTPLUG_MINIMIZE_GRACE + Duration::from_millis(1)),
            first
        ));
        // Well before: the user minimized it on purpose.
        assert!(!is_hotplug_casualty(
            Some(t0 - Duration::from_secs(30)),
            first
        ));
        // Never minimized through the hook, or no report pending: no.
        assert!(!is_hotplug_casualty(None, first));
        assert!(!is_hotplug_casualty(Some(t0), None));
    }

    #[test]
    fn reconcile_retries_are_bounded() {
        assert_eq!(next_reconcile_retry(0), Some(1));
        assert_eq!(
            next_reconcile_retry(MAX_RECONCILE_RETRIES - 1),
            Some(MAX_RECONCILE_RETRIES)
        );
        assert_eq!(next_reconcile_retry(MAX_RECONCILE_RETRIES), None);
    }

    /// With presets configured, tiles migrating from a vanished display are
    /// created at the survivor's current preset width — widths follow the
    /// monitor — while the survivor's own columns keep theirs.
    #[test]
    fn migrated_tiles_take_the_survivors_preset_width() {
        let mut flow = side_by_side_flow();
        flow.config.padding.window_gap = 4;
        flow.config.min_column_width_px = 320;
        flow.config.columns =
            Some(toml::from_str("presets = [\"1/3\", \"1/2\", \"1/1\"]").expect("columns"));
        flow.monitors[0].set_width_preset(2); // 1/1 on the primary: 1920 - 8
        let mut probe = MockTopology::new(vec![geo(1, PRIMARY, true)]);
        for hwnd in [A, B, C, D, F, M] {
            probe.nearest.insert(hwnd, 1);
        }
        probe.iconic.insert(M);
        flow.reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");

        let strip = flow.monitors[0].active_scrolling().virtual_layout();
        assert_eq!(
            flow.monitors[0].active_scrolling().config().column_width,
            1912
        );
        for hwnd in [B, C, D, M] {
            let (col, _) = strip.find_window(WindowId(hwnd)).expect("migrated");
            assert_eq!(strip.columns[col].width_px, 1912, "{hwnd:#x}");
        }
        // The survivor's own column was not resized by the reconcile.
        let (col, _) = strip.find_window(WindowId(A)).expect("A");
        assert_eq!(strip.columns[col].width_px, 960);
    }

    /// A display change re-resolves every monitor's preset against its own
    /// list (`[[columns.screen]]` by device name here): a survivor keeps the
    /// preset it was on while its list has it, a display that appears starts
    /// on its list's default, and a pattern pins the migrated columns by
    /// position.
    #[test]
    fn hotplug_carries_presets_over_by_width_and_applies_screen_rules() {
        let mut flow = side_by_side_flow();
        flow.config.padding.window_gap = 4;
        flow.config.min_column_width_px = 320;
        flow.monitors[0].set_device_name(r"\\.\DISPLAY1");
        flow.monitors[1].set_device_name(r"\\.\DISPLAY2");
        flow.config.columns = Some(
            toml::from_str(
                r#"
presets = ["1/3", "1/3+2/3", "1/2", "1/1"]
default = "1/2"
[[screen]]
match = { orientation = "portrait" }
presets = ["1/2", "1/1"]
default = "1/1"
[[screen]]
match = { name = "DISPLAY2" }
presets = ["1/2", "1/1"]
default = "1/1"
"#,
            )
            .expect("columns"),
        );
        flow.monitors[0].set_width_preset(1); // the pattern on the primary
        flow.monitors[1].set_width_preset(0); // "1/2" of DISPLAY2's own list
        assert_eq!(flow.monitor_preset_label(1).as_deref(), Some("1/2"));

        // LEFT vanishes: the primary survives on its pattern and the
        // migrated columns are pinned by position (1/3 of 1920 at gap 4 is
        // 634, 2/3 is 1273).
        let mut probe = MockTopology::new(vec![geo(1, PRIMARY, true)]);
        for hwnd in [A, B, C, D, F, M] {
            probe.nearest.insert(hwnd, 1);
        }
        probe.iconic.insert(M);
        flow.reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(flow.monitors.len(), 1);
        assert_eq!(flow.monitor_preset_label(0).as_deref(), Some("1/3+2/3"));
        let strip = flow.monitors[0].active_scrolling();
        assert!(strip.widths_pinned());
        let widths: Vec<i32> = strip
            .virtual_layout()
            .columns
            .iter()
            .map(|c| c.width_px)
            .collect();
        assert_eq!(widths.len(), 5, "{widths:?}");
        for (i, w) in widths.iter().enumerate() {
            assert_eq!(*w, if i % 2 == 0 { 634 } else { 1273 }, "column {i}");
        }

        // A display named DISPLAY2 appears: its rule's default, "1/1".
        let probe = MockTopology::new(vec![geo(1, PRIMARY, true), geo(2, LEFT, false)]);
        flow.reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(flow.monitors.len(), 2);
        assert_eq!(flow.monitors[1].device_name(), r"\\.\DISPLAY2");
        assert_eq!(flow.monitor_preset_list(1).expect("list").rule, Some(1));
        assert_eq!(flow.monitor_preset_label(1).as_deref(), Some("1/1"));
        assert_eq!(
            flow.monitors[1].active_scrolling().config().column_width,
            1912
        );
        assert_eq!(flow.monitor_preset_label(0).as_deref(), Some("1/3+2/3"));

        // DISPLAY2 goes and a display on the other side, DISPLAY3, comes
        // (a different rect, so not a survivor): it matches no rule and
        // starts on the top-level default.
        let probe = MockTopology::new(vec![geo(1, PRIMARY, true), geo(3, RIGHT, false)]);
        flow.reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(flow.monitor_preset_list(1).expect("list").rule, None);
        assert_eq!(flow.monitor_preset_label(1).as_deref(), Some("1/2"));
        assert_eq!(
            flow.monitors[1].active_scrolling().config().column_width,
            954
        );

        // The primary is rotated to portrait: the same HMONITOR survives,
        // the portrait rule now matches it and has no pattern, so it lands
        // on that rule's default and its columns are free again.
        let portrait = r(0, 0, 1080, 1920);
        let probe = MockTopology::new(vec![geo(1, portrait, true), geo(3, RIGHT, false)]);
        flow.reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(flow.monitor_preset_list(0).expect("list").rule, Some(0));
        assert_eq!(flow.monitor_preset_label(0).as_deref(), Some("1/1"));
        assert!(!flow.monitors[0].active_scrolling().widths_pinned());
        assert_eq!(
            flow.monitors[0].active_scrolling().config().column_width,
            1072
        );
        // Rotated back: the landscape list has no "1/1"? It does — the
        // primary stays on it rather than returning to the pattern.
        let probe = MockTopology::new(vec![geo(1, PRIMARY, true), geo(3, RIGHT, false)]);
        flow.reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(flow.monitor_preset_list(0).expect("list").rule, None);
        assert_eq!(flow.monitor_preset_label(0).as_deref(), Some("1/1"));
    }

    #[test]
    fn restore_order_preserved_for_a_vanished_strip() {
        // The LEFT monitor's whole strip (B, C, D) was minimized by Windows,
        // so the MINIMIZESTART hooks already emptied it. The windows come
        // back on the primary in their old column order, behind A, no
        // matter the registry's iteration order.
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [A, B, C, D] {
            fake_window(&mut registry, hwnd, tile());
        }
        let t = Instant::now();
        minimized_tile(&mut registry, D, 2, 0, t);
        minimized_tile(&mut registry, B, 0, 0, t);
        minimized_tile(&mut registry, C, 1, 0, t);
        let vs = r(-1920, 0, 3840, 1080);
        let mut m0 = monitor(1, PRIMARY, &[LEFT], vs);
        let m1 = monitor(2, LEFT, &[PRIMARY], vs);
        m0.active_scrolling_mut().add_window(WindowId(A));
        registry.set_monitor(A, 0);
        for hwnd in [B, C, D] {
            registry.set_monitor(hwnd, 1);
        }
        registry.set_focused(A);
        let mut flow = FlowWM::new_for_test(registry, vec![m0, m1]);
        flow.topology_first_report = Some(t - Duration::from_millis(20));

        let mut probe = MockTopology::new(vec![geo(1, PRIMARY, true)]);
        for hwnd in [B, C, D] {
            probe.nearest.insert(hwnd, 1);
            probe.iconic.insert(hwnd);
        }
        let summary = flow
            .reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(summary.migrated, 0);
        assert_eq!(summary.restored, 3);
        assert_eq!(*probe.restored.borrow(), vec![B, C, D]);
        let strip = flow.monitors[0].active_scrolling().virtual_layout();
        let order: Vec<WindowId> = strip.columns.iter().map(|c| c.rows[0].window_id).collect();
        assert_eq!(
            order,
            vec![WindowId(A), WindowId(B), WindowId(C), WindowId(D)]
        );
        for hwnd in [B, C, D] {
            assert!(flow.registry.is_tiling_active(hwnd), "{hwnd:#x}");
            assert_eq!(flow.registry.remembered_monitor(hwnd), Some(0));
        }
        // Restoring never moved the foreground: it was pushed back to A.
        assert_eq!(*probe.focused.borrow(), vec![A]);
    }

    #[test]
    fn survivor_parked_column_minimized_in_the_settle_window_returns_to_its_slot() {
        // The primary's column 1 (P) was parked beyond the LEFT display, so
        // Windows minimized it with that display even though the primary
        // survives. It goes back into column 1.
        const P: isize = 0x5;
        const Q: isize = 0x6;
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [A, P, Q] {
            fake_window(&mut registry, hwnd, tile());
        }
        let t = Instant::now();
        minimized_tile(&mut registry, P, 1, 0, t);
        let vs = r(-1920, 0, 3840, 1080);
        let mut m0 = monitor(1, PRIMARY, &[LEFT], vs);
        let m1 = monitor(2, LEFT, &[PRIMARY], vs);
        m0.active_scrolling_mut().add_window(WindowId(A));
        m0.active_scrolling_mut().add_window(WindowId(Q));
        for hwnd in [A, P, Q] {
            registry.set_monitor(hwnd, 0);
        }
        let mut flow = FlowWM::new_for_test(registry, vec![m0, m1]);
        flow.topology_first_report = Some(t - Duration::from_millis(20));

        let mut probe = MockTopology::new(vec![geo(1, PRIMARY, true)]);
        probe.nearest.insert(P, 1);
        probe.iconic.insert(P);
        let summary = flow
            .reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(summary.restored, 1);
        let strip = flow.monitors[0].active_scrolling().virtual_layout();
        let order: Vec<WindowId> = strip.columns.iter().map(|c| c.rows[0].window_id).collect();
        assert_eq!(order, vec![WindowId(A), WindowId(P), WindowId(Q)]);
        assert!(flow.registry.is_tiling_active(P));
    }

    #[test]
    fn window_minimized_by_the_user_before_the_change_stays_minimized() {
        let mut flow = side_by_side_flow();
        // M was minimized half a minute before the first report.
        if let Some(w) = flow.registry.get_window_mut(HWND(M as *mut _)) {
            w.minimized_at = Some(Instant::now() - Duration::from_secs(30));
        }
        let mut probe = MockTopology::new(vec![geo(1, PRIMARY, true)]);
        for hwnd in [A, B, C, D, F, M] {
            probe.nearest.insert(hwnd, 1);
        }
        probe.iconic.insert(M);
        let summary = flow
            .reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        assert_eq!(summary.restored, 0);
        assert!(probe.restored.borrow().is_empty());
        assert!(probe.focused.borrow().is_empty());
        // Still minimized, but its owner memory moved to the survivor.
        assert!(matches!(
            flow.registry
                .get_window(HWND(M as *mut _))
                .map(|w| w.state.clone()),
            Some(WindowState::Tiling(TilingState::Minimized))
        ));
        assert_eq!(flow.registry.remembered_monitor(M), Some(0));
        assert!(
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .find_window(WindowId(M))
                .is_none()
        );
    }

    #[test]
    fn no_duplicate_column_when_the_restore_hook_follows_the_reconcile() {
        let mut flow = side_by_side_flow();
        let mut probe = MockTopology::new(vec![geo(1, PRIMARY, true)]);
        for hwnd in [A, B, C, D, F, M] {
            probe.nearest.insert(hwnd, 1);
        }
        probe.iconic.insert(M);
        flow.reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        let count = |flow: &FlowWM| {
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .columns
                .iter()
                .filter(|c| c.rows.iter().any(|r| r.window_id == WindowId(M)))
                .count()
        };
        assert_eq!(count(&flow), 1);
        // The MINIMIZEEND hook for M arrives after the reconcile seated it
        // (its layout half; the border refresh needs a desktop).
        flow.registry.restore_window(M);
        flow.readd_tile_to_home_monitor(M);
        assert_eq!(
            count(&flow),
            1,
            "the restore hook must not add a second column"
        );
        assert_eq!(
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .window_count(),
            5
        );
    }

    #[test]
    fn survivor_floats_are_clamped_into_a_shrunk_work_area() {
        let mut flow = side_by_side_flow();
        // F sits at (-1500, 100, 400, 300) on the LEFT monitor, which now
        // loses its bottom 800 px to a taskbar: F no longer fits and is
        // moved up, same size.
        let shrunk = r(-1920, 0, 1920, 280);
        let mut left = geo(2, LEFT, false);
        left.work_area = shrunk;
        let probe = MockTopology::new(vec![geo(1, PRIMARY, true), left]);
        flow.reconcile_monitors_with(&probe)
            .expect("reconcile succeeds");
        let floats = flow.monitors[1].active_workspace().floating.windows();
        assert_eq!(floats.len(), 1);
        let f = floats[0].rect;
        // Shrunk to the 280 px work-area height, width untouched.
        assert_eq!((f.width, f.height), (400, 280));
        assert!(f.y >= 0 && f.bottom() <= 280, "{f:?}");
        assert!(matches!(
            flow.registry.get_window(HWND(F as *mut _)).map(|w| w.state.clone()),
            Some(WindowState::Floating(FloatingState::Active { rect })) if rect == f
        ));
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
