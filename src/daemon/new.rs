//! FlowWM constructor — performs all startup work.
//!
//! This module contains the [`FlowWM::new`] method which:
//!
//! 1. Creates [`WindowRegistry`] from user and default rules.
//! 2. Scans existing windows (populates registry before hooks start).
//! 3. Queries monitor work area via Win32.
//! 4. Derives layout parameters from the [`FlowConfig`].
//! 5. Creates [`ScrollingSpace`] with those parameters.
//! 6. Batch-initializes layout from existing tiling windows (sorted by x
//!    coordinate for deterministic column assignment; viewport centered
//!    on the focused column).
//! 7. Creates [`WindowAnimator`] with Win32 backend and the user's
//!    configured animation duration (instant if animation is disabled).
//! 8. Animates the initial layout (windows animate to tiling positions).
//! 9. Starts the WinEvent hook thread.
//! 10. Creates the IPC named pipe server.
//!
//! # Config Model
//!
//! The `app_config` parameter is already a fully-resolved [`FlowConfig`]:
//! serde defaults (see [`config::defaults`]) fill in any fields absent from
//! the user's `flow.toml`. No further merging is needed here.
use std::time::Duration;

use crate::animation::WindowAnimator;
use crate::animation::backend::win32::Win32Backend;
use crate::common::{Rect, WindowId};
use crate::config::dirs::history_rules_path_in;
use crate::config::history::HistoryStore;
use crate::config::types::{FlowConfig, WindowRulesConfig};
use crate::ipc::transport::PipeServer;
use crate::layout::types::MonitorInfo;
use crate::registry::types::{FloatingState, WindowState};
use crate::registry::{WindowRegistry, hooks, win32 as registry_win32};
use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};
use windows::Win32::Foundation::HWND;

use super::animation::animate_layout_raw;
use super::config_derive;
use super::types::FlowWM;

impl FlowWM {
    /// Construct and initialize the daemon.
    ///
    /// Performs all startup work in sequence. See the [module documentation](self)
    /// for the complete initialization flow.
    ///
    /// # Arguments
    ///
    /// * `app_config` - Application settings (serde defaults + user overrides).
    /// * `user_rules` - User-defined window rules from `flow-rules.toml`.
    /// * `default_rules` - Bundled default rules.
    /// * `config_dir` - Path to the configuration directory.
    /// * `desktop_name` - Optional test desktop name (debug builds only).
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` if any startup step fails:
    /// - Window scan failure (non-fatal — logged, returns `Ok`).
    /// - Monitor work area query failure.
    /// - Hook thread start failure.
    /// - Named pipe creation failure (likely another daemon running).
    pub fn new(
        app_config: FlowConfig,
        user_rules: WindowRulesConfig,
        default_rules: WindowRulesConfig,
        config_dir: std::path::PathBuf,
        desktop_name: Option<String>,
    ) -> Result<Self, String> {
        log::debug!(
            "effective config: columns_per_screen={}, column_width={:?}, min_column_width_px={}, padding.window_gap={}, padding.up={}, padding.down={}, animation.duration_ms={}",
            app_config.columns_per_screen,
            app_config.column_width,
            app_config.min_column_width_px,
            app_config.padding.window_gap,
            app_config.padding.up,
            app_config.padding.down,
            app_config.animation.duration_ms,
        );

        // 1. Create registry from rules.
        let mut registry = WindowRegistry::new(&user_rules, &default_rules);

        // Load learned rules from `history-flow-rules.toml` and push them into
        // the registry's classification pipeline BEFORE scanning existing
        // windows, so previously-recorded decisions apply to windows that
        // were already open when flowd started.
        let history = HistoryStore::load(&history_rules_path_in(&config_dir));
        if !history.is_empty() {
            log::info!("loaded {} learned rules from history", history.len());
            registry.set_learned_rules(history.rules().to_vec());
        }

        // 2. Scan existing windows before hooks start.
        registry.scan_existing_windows()?;

        // 3. Get monitor geometry via Win32 — both the full physical screen
        //    rect (for parking workspaces off-screen) and the taskbar-excluded
        //    work area (for in-workspace window placement). See
        //    registry::win32::MonitorGeometry for why both are needed.
        // One scrolling space per monitor, each sized to its own work area.
        // Windows are assigned to the monitor they are currently on.
        let geometries = registry_win32::enumerate_monitors()?;
        let virtual_screen = registry_win32::virtual_screen_rect();
        let screen_rects: Vec<Rect> = geometries.iter().map(|g| g.screen_rect).collect();
        log::info!(
            "init: {} monitor(s), virtual screen ({},{},{},{})",
            geometries.len(),
            virtual_screen.x,
            virtual_screen.y,
            virtual_screen.width,
            virtual_screen.height,
        );

        struct PerMonitor {
            info: MonitorInfo,
            layout_config: super::types::LayoutConfig,
            scrolling: ScrollingSpace,
        }
        let mut per_monitor: Vec<PerMonitor> = Vec::with_capacity(geometries.len());
        for (i, g) in geometries.iter().enumerate() {
            let others: Vec<Rect> = screen_rects
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, r)| *r)
                .collect();
            let info = MonitorInfo::in_desktop(g.work_area, g.screen_rect, virtual_screen, &others);
            log::info!(
                "init: monitor {i}: screen ({},{},{},{}) work ({},{},{},{}) primary={} neighbours L={} R={}",
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
            );
            let layout_config = config_derive::derive_layout_config(&app_config, &info);
            let scrolling = ScrollingSpace::new(
                info,
                layout_config.column_width,
                layout_config.min_column_width_px,
                layout_config.min_window_height_px,
                layout_config.min_row_height_px,
                layout_config.padding,
                app_config.columns_per_screen,
            );
            per_monitor.push(PerMonitor {
                info,
                layout_config,
                scrolling,
            });
        }

        // Partition the pre-existing tiling windows by the monitor they sit on.
        let all_tiling = registry.tiling_window_ids_with_widths_sorted_by_x();
        let mut per_monitor_windows: Vec<(Vec<WindowId>, Vec<u32>)> =
            vec![(Vec::new(), Vec::new()); geometries.len()];
        for (wid, width) in all_tiling {
            let h = registry_win32::monitor_handle_for_window(HWND(wid.0 as *mut _));
            let idx = geometries.iter().position(|g| g.hmonitor == h).unwrap_or(0);
            per_monitor_windows[idx].0.push(wid);
            per_monitor_windows[idx].1.push(width);
        }

        let fg_hwnd = registry_win32::get_foreground_window();
        if let Some(hwnd) = fg_hwnd {
            registry.set_focused(hwnd);
        }
        let mut active_monitor = 0;
        let mut initial_diffs = Vec::with_capacity(geometries.len());
        for (idx, pm) in per_monitor.iter_mut().enumerate() {
            let (tiling_ids, widths) = std::mem::take(&mut per_monitor_windows[idx]);
            log::debug!(
                "init: monitor {idx}: {} tiling windows (sorted by x), widths: {:?}",
                tiling_ids.len(),
                widths
            );
            if tiling_ids.is_empty() {
                continue;
            }
            let focus_col = fg_hwnd.and_then(|hwnd| {
                let wid = WindowId(hwnd);
                tiling_ids.iter().position(|&id| id == wid)
            });
            if focus_col.is_some() {
                active_monitor = idx;
            }
            let diff = pm
                .scrolling
                .initialize_windows(tiling_ids, focus_col, Some(&widths));
            for entry in &diff.actual_layout.entries {
                log::trace!(
                    "init target: {:?} rect ({},{},{},{})",
                    entry.window_id,
                    entry.rect.x,
                    entry.rect.y,
                    entry.rect.width,
                    entry.rect.height,
                );
            }
            initial_diffs.push(diff);
        }
        if initial_diffs.is_empty() {
            log::warn!("init: no tiling windows found - skipping initial layout");
        }

        let backend = Win32Backend::new();
        let init_config = config_derive::derive_animator_config(&app_config, Duration::ZERO);
        log::debug!(
            "init: animator config duration={:?}ms, animation.enabled={}",
            init_config.duration,
            app_config.animation.enabled,
        );
        let mut animator = WindowAnimator::new(backend, init_config);
        for diff in &initial_diffs {
            log::debug!(
                "init: submitting {} targets to animator",
                diff.actual_layout.entries.len()
            );
            animate_layout_raw(
                &mut animator,
                diff,
                &registry,
                app_config.borders.thickness as i32,
                app_config.borders.overlap as i32,
            );
            registry.update_tiling_slots_from_layout(&diff.virtual_layout);
            registry.update_tiled_rects(&diff.actual_layout);
        }

        let (hook_receiver, _hook_handle, hook_signal) = hooks::start_hook_thread(desktop_name)?;
        let server = PipeServer::create()
            .map_err(|e| format!("failed to create pipe (is another daemon running?): {e}"))?;
        log::info!("flowd: daemon initialized successfully");

        const WORKSPACE_COUNT: u32 = 10;
        let mut monitors: Vec<Monitor> = Vec::with_capacity(geometries.len());
        for (g, pm) in geometries.iter().zip(per_monitor) {
            let PerMonitor {
                info,
                layout_config,
                scrolling,
            } = pm;
            let mut workspaces: Vec<Workspace> = Vec::with_capacity(WORKSPACE_COUNT as usize);
            workspaces.push(Workspace::new(WorkspaceId(1), scrolling));
            for id in 2..=WORKSPACE_COUNT {
                let empty_scrolling = ScrollingSpace::new(
                    info,
                    layout_config.column_width,
                    layout_config.min_column_width_px,
                    layout_config.min_window_height_px,
                    layout_config.min_row_height_px,
                    layout_config.padding,
                    app_config.columns_per_screen,
                );
                workspaces.push(Workspace::new(WorkspaceId(id), empty_scrolling));
            }
            monitors.push(
                Monitor::new(g.screen_rect, g.work_area, workspaces, 0).with_hmonitor(g.hmonitor),
            );
        }

        let mut manager = Self {
            registry,
            history,
            monitors,
            active_monitor,
            animator,
            server,
            config: app_config,
            config_dir,
            hook_receiver,
            _hook_handle,
            hook_signal,
            shutting_down: false,
            pending_creations: Vec::new(),
            float_resume_deadline: None,
            drag_state: None,
            last_foreground_sync: std::time::Instant::now(),
        };
        // Adopt pre-existing float-classified windows (found during the init
        // scan) into workspace 1's FloatingSpace, at their current on-screen
        // position. Without this they'd be tracked in the registry but absent
        // from FloatingSpace (so workspace switching would strand them) and
        // borderless (refresh_all_border_styles below would seat a border at
        // their zero placeholder rect). No animation: a separate animate call
        // would interrupt the in-flight tiling init animation
        // (InterruptPolicy::RetargetFromCurrent). The windows are already where
        // they should be, so adoption-in-place is both safe and non-disruptive.
        manager.populate_existing_floats();
        // Initial border overlay attach for windows found during the scan.
        // Idempotent and O(N) where N = tracked windows. Runs after
        // populate_existing_floats so float overlays seat at the adopted rect.
        manager.refresh_all_border_styles();
        Ok(manager)
    }

    /// Adopt every pre-existing float-classified window into the active
    /// workspace's [`FloatingSpace`](crate::workspace::FloatingSpace).
    ///
    /// Mirrors the float setup that [`on_window_created`](super::FlowWM::on_window_created)
    /// performs for a freshly-classified float, but for windows that were
    /// already open when the daemon launched. Each float is registered at its
    /// **current on-screen position** (via [`current_visible_rect`], falling
    /// back to [`centered_float_rect`](Self::centered_float_rect)) rather than
    /// being yanked to centre — the daemon adopting an existing window should
    /// not move it. No animation is submitted: the tiling init animation is
    /// already in flight, and a second batch would replace it
    /// (`InterruptPolicy::RetargetFromCurrent`).
    fn populate_existing_floats(&mut self) {
        // Snapshot hwnds up front: register_float takes &mut self, so we can't
        // hold a borrow of the registry's window iterator across it.
        let float_hwnds: Vec<isize> = self
            .registry
            .windows()
            .filter(|w| matches!(w.state, WindowState::Floating(FloatingState::Active { .. })))
            .map(|w| w.hwnd.0 as isize)
            .collect();

        for hwnd in float_hwnds {
            let wid = WindowId(hwnd);
            let rect = self
                .current_visible_rect(hwnd)
                .unwrap_or_else(|| self.centered_float_rect(wid));
            self.register_float(wid, rect);
        }
    }

    /// Read a window's current on-screen position as a visible rect, or `None`
    /// if it can't be queried (window destroyed mid-scan, GetWindowRect
    /// failure). The visible rect is what [`FloatingSpace`](crate::workspace::FloatingSpace)
    /// stores — `GetWindowRect`'s window rect is translated back through the
    /// window's measured [`InvisibleBounds`](crate::common::InvisibleBounds).
    fn current_visible_rect(&self, hwnd: isize) -> Option<Rect> {
        let hwnd_handle = HWND(hwnd as *mut _);
        let window_rect = registry_win32::get_window_rect(hwnd_handle).ok()?;
        let window = self.registry.get_window(hwnd_handle)?;
        Some(window.invisible_bounds.window_to_visible(window_rect))
    }
}

#[cfg(test)]
impl FlowWM {
    /// Construct a `FlowWM` for unit tests, bypassing all Win32 startup work.
    ///
    /// Fabricates non-essential fields (`PipeServer`, hook handles, animator)
    /// with test-safe dummies — see [`PipeServer::test_dummy`],
    /// [`HookThreadHandle::test_dummy`](crate::registry::hooks::HookThreadHandle::test_dummy),
    /// [`HookSignal::test_dummy`](crate::registry::hooks::HookSignal::test_dummy),
    /// and [`MockBackend`](crate::animation::backend::mock::MockBackend). The
    /// caller supplies a fully-populated [`WindowRegistry`] and the
    /// [`Vec<Monitor>`] stack; everything else defaults.
    ///
    /// Intended for testing dispatch handlers (`dispatch_focus`,
    /// `dispatch_swap_column`, etc.) without spawning a real daemon. The
    /// fabricated fields are never touched on no-op code paths; positive paths
    /// exercise `monitors` / `animator` / `registry` as normal.
    pub(super) fn new_for_test(registry: WindowRegistry, monitors: Vec<Monitor>) -> Self {
        use crate::animation::backend::mock::MockBackend;

        let (_hook_sender, hook_receiver) = std::sync::mpsc::channel();
        Self {
            registry,
            history: HistoryStore::default(),
            monitors,
            active_monitor: 0,
            animator: WindowAnimator::new(
                MockBackend::new(),
                crate::animation::AnimatorConfig::default(),
            ),
            server: PipeServer::test_dummy(),
            config: FlowConfig::default(),
            config_dir: std::path::PathBuf::new(),
            hook_receiver,
            _hook_handle: hooks::HookThreadHandle::test_dummy(),
            hook_signal: hooks::HookSignal::test_dummy(),
            shutting_down: false,
            pending_creations: Vec::new(),
            float_resume_deadline: None,
            drag_state: None,
            last_foreground_sync: std::time::Instant::now(),
        }
    }
}
