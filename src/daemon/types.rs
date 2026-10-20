//! FlowWM — the single top-level orchestrator for the `flowd` daemon.
//!
//! [`FlowWM`] owns all subsystems and routes events between them.
//! See individual module files for detailed documentation.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crate::animation::WindowAnimator;
use crate::autostart::AhkProcess;
use crate::config::history::HistoryStore;
use crate::config::types::FlowConfig;
use crate::ipc::transport::PipeServer;
use crate::registry::WindowRegistry;
use crate::registry::hooks::{HookSignal, HookThreadHandle};
use crate::workspace::{FloatingSpace, Monitor, ScrollingSpace, Workspace};

/// Intermediate struct holding layout engine parameters derived from
/// [`FlowConfig`]. Used during construction to keep the parameter list
/// readable and avoid recomputing values.
///
/// This type is private to the daemon module — only construction logic
/// needs these derived values.
pub(super) struct LayoutConfig {
    /// Default column width in pixels.
    ///
    /// When `FlowConfig::column_width` is `Some`, this is that value directly.
    /// When `None`, this is computed from `columns_per_screen`, the monitor width,
    /// and `window_gap`:
    /// `base_content_width = (monitor_width - (N+1) * window_gap) / N`
    pub(super) column_width: u32,

    /// Minimum column width in pixels.
    pub(super) min_column_width_px: u32,

    /// Minimum per-row window height in pixels. Caps how many windows a
    /// column can hold. Sourced directly from
    /// [`FlowConfig::min_window_height_px`].
    pub(super) min_window_height_px: u32,

    /// Minimum row height in pixels — the dedicated vertical drag-resize floor
    /// (ticket #10). Sourced directly from
    /// [`FlowConfig::min_row_height_px`].
    pub(super) min_row_height_px: u32,

    /// Padding converted from config types to layout types.
    pub(super) padding: crate::layout::types::Padding,
}

/// The single orchestrator for the FlowWM daemon.
///
/// Owns all subsystems and routes events between them. Lives entirely on
/// the IPC thread — no interior mutability (`Arc<Mutex<>>`) is needed.
///
/// # Architecture
///
/// ```text
/// ┌──────────────────────────────────────────────────────────────────────┐
/// │                      FlowWM                             │
/// │                                                                      │
/// │  Owns:                                                               │
/// │  ┌────────────────┐  ┌──────────────┐  ┌──────────────────────────┐  │
/// │  │ WindowRegistry │  │ Vec<Monitor> │  │ WindowAnimator           │  │
/// │  │ (window state) │  │ (workspaces: │  │ (src/animation/)         │  │
/// │  └────────────────┘  │  Scrolling + │  └──────────────────────────┘  │
/// │                      │  Floating)   │                                 │
/// │  ┌────────────────┐  └──────────────┘  ┌──────────────────────────┐  │
/// │  │ PipeServer     │  ┌──────────────┐  │ HistoryStore             │  │
/// │  │ (IPC transport)│  │ AppConfig    │  │ (learned rules)          │  │
/// │  └────────────────┘  └──────────────┘  └──────────────────────────┘  │
/// │                                                                      │
/// │  Routes:                                                             │
/// │  • Hook events  → registry mutation → layout engine → animator       │
/// │  • IPC commands → layout engine / registry query → animator          │
/// └──────────────────────────────────────────────────────────────────────┘
/// ```
///
/// # Threading Model
///
/// ```text
/// Hook Thread (background):          IPC Thread (main):
///   SetWinEventHook ×3                owns FlowWM (all fields)
///   GetMessageW loop                  ├─ process_hook_events()
///       ↓ callback                    ├─ dispatch IPC command
///   sender.send(HookEvent)            ├─ process_hook_events()
///                                     └─ ... (repeat)
/// ```
///
/// The hook thread never touches any flow field. It only sends [`HookEvent`]
/// through the `mpsc` channel. The IPC thread reads the channel and calls
/// methods on `registry`, the active workspace's scrolling space, and the
/// `animator` directly — **no mutex, no locking, no deadlocks**.
///
/// Since all subsystem methods take `&mut self`, the borrow checker enforces
/// exclusive access at compile time. This is strictly safer than `Mutex`
/// (which only enforces at runtime and can deadlock).
pub struct FlowWM {
    /// Window registry — authoritative source of truth for all tracked windows.
    pub(super) registry: WindowRegistry,

    /// Per-app learned window modes, persisted to `history-flow-rules.toml`.
    ///
    /// Records the user's explicit `set-window float|tile` decisions so the
    /// next window of the same app is classified automatically. See
    /// (`docs/src/dev-guide/classification.md`) for the priority chain.
    pub(super) history: HistoryStore,

    /// The monitor stack — one [`Monitor`] per physical display, each owning
    /// its own vertical stack of [`Workspace`]s.
    ///
    /// The daemon currently initialises exactly one monitor (the primary
    /// display) with ten workspaces (see `new.rs`); multi-monitor support
    /// will grow this vector. Use [`active_monitor`](Self::active_monitor) to
    /// reach the monitor that currently owns keyboard focus.
    pub(super) monitors: Vec<Monitor>,

    /// Index into [`monitors`](Self::monitors) of the monitor that currently
    /// owns keyboard focus. Always a valid index — the daemon keeps at least
    /// one monitor for the lifetime of the process.
    pub(super) active_monitor: usize,

    /// Window animator — background-threaded rect animation for smooth moves.
    pub(super) animator: WindowAnimator,

    /// IPC named pipe server — accepts commands from the `flow` CLI.
    pub(super) server: PipeServer,

    /// Application configuration loaded from `flow.toml`.
    ///
    /// The live source for border colors/thickness (read by the border overlay
    /// and animation code) and the target of hot-reload. The layout engine owns
    /// its own derived width/bounds state internally (see [`crate::workspace`]).
    pub(super) config: FlowConfig,

    /// Path to the configuration directory — used to locate `flow.toml` and
    /// `flow-rules.toml` on hot-reload.
    pub(super) config_dir: PathBuf,

    /// Receiver for hook events from the background WinEvent hook thread.
    pub(super) hook_receiver: Receiver<crate::registry::HookEvent>,

    /// Handle to the background hook thread. Its `Drop` impl posts `WM_QUIT`
    /// to the hook thread's message loop; `Ping` reports its registered-hook
    /// count.
    pub(super) hook_handle: HookThreadHandle,

    /// Win32 Event handle signaled by the hook callback thread.
    ///
    /// The main event loop waits on this via `WaitForMultipleObjects` so hook
    /// events (window create/destroy/focus) are processed immediately, even
    /// when no IPC client is connected.
    ///
    /// RAII: closed automatically on drop via `HookSignal`'s `Drop` impl.
    pub(super) hook_signal: HookSignal,

    /// The invisible top-level window that receives `WM_QUERYENDSESSION` /
    /// `WM_ENDSESSION` / `WM_CLOSE` so logoff and shutdown always trigger
    /// the rescue, even when no border overlay exists. Destroyed on drop.
    #[allow(dead_code)] // RAII: held only so its Drop runs with the daemon's.
    pub(super) session_sink: super::session::SessionSink,

    /// The hotkey script launched by this daemon (`flowd --ahk`), living in
    /// a kill-on-close job object. `None` when not requested, not found, or
    /// already stopped. Never closed explicitly except by
    /// [`fail_back`](FlowWM::fail_back): process exit closes the job
    /// handle and the kernel kills the script.
    pub(super) ahk: Option<AhkProcess>,

    /// Set to `true` when the `Stop` IPC command is received, causing
    /// the main event loop to exit on the next iteration.
    pub(super) shutting_down: bool,

    /// Windows whose `Created` hook event fired before they were fully
    /// initialized (not yet visible, no title, styles not finalized).
    ///
    /// Each entry is `(hwnd, retry_count)`. On every `process_hook_events`
    /// call, pending windows are retried via `handle_created`. A window is
    /// removed from the list when classification succeeds or when
    /// `retry_count` exceeds `MAX_PENDING_RETRIES`.
    ///
    /// # Why this is needed
    ///
    /// `EVENT_OBJECT_CREATE` fires early in the Win32 window lifecycle —
    /// before `ShowWindow`, `SetWindowText`, or style finalization. With
    /// the event-driven loop (ResetEvent + immediate drain), hook events
    /// are processed within microseconds of arrival, so the window's
    /// classification checks (`is_window_visible`, title non-empty, etc.)
    /// fail. A short retry gives the window time to finish initializing.
    ///
    /// # Timeout interaction
    ///
    /// When this list is non-empty, `run()` uses a finite timeout (100 ms)
    /// on `WaitForMultipleObjects` instead of `INFINITE`. This ensures
    /// pending windows are retried even if no new hook events arrive.
    pub(super) pending_creations: Vec<(isize, u8)>,

    /// Deadline at which float-location tracking resumes after flow suppresses
    /// it for a float animation.
    ///
    /// `None` when no suppression is active — `FLOAT_TRACKING_ACTIVE` stays on
    /// and `EVENT_OBJECT_LOCATIONCHANGE` from floats is captured live. Set by
    /// [`arm_float_tracking_suppression`](super::FlowWM::arm_float_tracking_suppression)
    /// and cleared by the resume poll in the main loop.
    pub(super) float_resume_deadline: Option<std::time::Instant>,

    /// State for the in-progress tile-window drag, or `None` when idle.
    ///
    /// A 4-state machine `Idle | Classifying | Translate | Resize`: `None`
    /// (idle) is set by [`on_drag_start`](super::drag::FlowWM::on_drag_start)
    /// (MoveSizeStart on a tiled window) to [`DragMode::Classifying`], promoted
    /// on the first `LOCATIONCHANGE` to `Translate` or `Resize`, and cleared
    /// by [`on_drag_end`](super::drag::FlowWM::on_drag_end) (MoveSizeEnd).
    ///
    /// When `Some`, layout-mutating IPC is rejected with
    /// [`SocketResponse::Busy`](crate::ipc::message::SocketResponse::Busy) —
    /// this covers resize automatically because it lives in the same field.
    pub(super) drag_state: Option<super::drag::DragMode>,

    /// Timestamp of the most recent foreground-reconciliation pass.
    ///
    /// The main loop folds `last_foreground_sync + foreground_sync_interval_ms`
    /// into its `MsgWaitForMultipleObjects` timeout so it never sleeps past the
    /// next reconciliation deadline. On each wake,
    /// [`reconcile_foreground`](super::FlowWM::reconcile_foreground) stamps this
    /// field to `now` and re-checks `GetForegroundWindow()` against the tracked
    /// focus — closing the gap when `EVENT_SYSTEM_FOREGROUND` is dropped under
    /// rapid window churn. (`docs/src/dev-guide/event-pipelines.md`)
    pub(super) last_foreground_sync: std::time::Instant,

    /// When the pending display-topology change settles and
    /// [`reconcile_monitors`](super::FlowWM::reconcile_monitors) runs.
    ///
    /// `None` while no change is pending. Armed (and re-armed) to
    /// `now + TOPOLOGY_SETTLE` by the main loop every time a window
    /// procedure reported `WM_DISPLAYCHANGE` / `SPI_SETWORKAREA` — Windows
    /// sends several per change, so the reconcile runs once, after the last
    /// one. Folded into the loop's wait timeout like the other deadlines.
    /// See `daemon::hotplug`.
    pub(super) topology_deadline: Option<std::time::Instant>,
}

impl FlowWM {
    /// Launch `<config_dir>/flow.ahk` inside a kill-on-close job object
    /// (see [`crate::autostart::launch_ahk_in_job`]). Called by `main` once
    /// the `ready` stage has been reported, so the first hotkey always
    /// reaches a listening daemon. A failure is logged and leaves the daemon
    /// running without hotkeys — the window manager itself is unaffected.
    ///
    /// Returns the script's pid on success.
    pub fn launch_ahk(&mut self) -> Option<u32> {
        if let Some(existing) = &self.ahk {
            return Some(existing.pid());
        }
        match crate::autostart::launch_ahk_in_job(&self.config_dir) {
            Ok(proc) => {
                let pid = proc.pid();
                if !proc.is_job_tied() {
                    log::warn!(
                        "flow.ahk (pid {pid}) is not tied to the daemon's job object; it will not be killed automatically if the daemon dies"
                    );
                }
                self.ahk = Some(proc);
                Some(pid)
            }
            Err(e) => {
                log::error!("could not launch flow.ahk: {e}");
                None
            }
        }
    }

    /// Pid of the hotkey script launched by this daemon, if it is still
    /// running. Reported in the `Pong` reply.
    #[must_use]
    pub fn ahk_pid(&self) -> Option<u32> {
        self.ahk
            .as_ref()
            .filter(|p| p.is_running())
            .map(AhkProcess::pid)
    }

    /// The index of the monitor that currently owns keyboard focus.
    #[must_use]
    #[allow(dead_code)] // API surface for the upcoming multi-monitor / workspace commands.
    pub(super) fn active_monitor_index(&self) -> usize {
        self.active_monitor
    }

    /// Borrow the active monitor (the one that owns keyboard focus).
    ///
    /// # Panics
    ///
    /// Panics if `monitors` is empty. The daemon always keeps at least one
    /// monitor, so this never fires in practice.
    #[must_use]
    pub(super) fn active_monitor(&self) -> &Monitor {
        &self.monitors[self.active_monitor]
    }

    /// Mutably borrow the active monitor.
    ///
    /// # Panics
    ///
    /// Panics if `monitors` is empty — see [`active_monitor`](Self::active_monitor).
    #[must_use]
    pub(super) fn active_monitor_mut(&mut self) -> &mut Monitor {
        &mut self.monitors[self.active_monitor]
    }

    /// Make `idx` the active monitor. Out-of-range indices are ignored.
    pub(super) fn set_active_monitor(&mut self, idx: usize) {
        if idx < self.monitors.len() && idx != self.active_monitor {
            log::debug!("active monitor {} -> {}", self.active_monitor, idx);
            self.active_monitor = idx;
        }
    }

    /// Index of the monitor whose workspaces (tiling or floating) contain
    /// `window`, if any.
    #[must_use]
    pub(super) fn monitor_index_containing(
        &self,
        window: crate::common::WindowId,
    ) -> Option<usize> {
        self.monitors
            .iter()
            .position(|m| m.find_workspace_containing(window).is_some())
    }

    /// Index of the monitor whose workspaces contain `window`, else the
    /// monitor the registry remembers as its owner, else the active monitor.
    ///
    /// The fallback chain for every "which monitor does this window belong
    /// to" question that must produce an answer: a window that was just
    /// removed from its space (minimized, hidden) is no longer *contained*
    /// anywhere, but its owning-monitor memory still says where it lived.
    #[must_use]
    pub(super) fn monitor_index_for_window(&self, window: crate::common::WindowId) -> usize {
        self.monitor_index_containing(window)
            .or_else(|| self.remembered_monitor(window))
            .unwrap_or(self.active_monitor)
    }

    /// The registry's owning-monitor memory for `window`, validated against
    /// the current monitor count (a stale index from a display that has
    /// since been unplugged is treated as unknown).
    #[must_use]
    pub(super) fn remembered_monitor(&self, window: crate::common::WindowId) -> Option<usize> {
        self.registry
            .remembered_monitor(window.0)
            .filter(|&i| i < self.monitors.len())
    }

    /// Record `idx` as the owning monitor of `window` in the registry. Called
    /// wherever a window is seated in a monitor's scrolling or floating
    /// space so the memory never drifts from the spaces.
    pub(super) fn remember_monitor(&mut self, window: crate::common::WindowId, idx: usize) {
        self.registry.set_monitor(window.0, idx);
    }

    /// Index of the monitor whose screen rect overlaps `rect`, preferring the
    /// one containing the rect's centre. Falls back to the active monitor.
    #[must_use]
    pub(super) fn monitor_index_for_rect(&self, rect: crate::common::Rect) -> usize {
        self.monitor_index_for_rect_opt(rect)
            .unwrap_or(self.active_monitor)
    }

    /// Index of the monitor whose screen rect contains `rect`'s centre, else
    /// the first one `rect` overlaps; `None` when `rect` touches no monitor
    /// (a window parked beyond the virtual screen).
    #[must_use]
    pub(super) fn monitor_index_for_rect_opt(&self, rect: crate::common::Rect) -> Option<usize> {
        let rects: Vec<crate::common::Rect> =
            self.monitors.iter().map(|m| m.screen_rect()).collect();
        monitor_index_for_rect_in(&rects, rect)
    }

    /// Index of the monitor whose `HMONITOR` is the one Windows reports for
    /// `hwnd` (`MonitorFromWindow`), or `None` when no monitor carries that
    /// handle (tests, or a display that appeared after startup).
    #[must_use]
    pub(super) fn monitor_index_for_hmonitor_of(&self, hwnd: isize) -> Option<usize> {
        let h = crate::registry::win32::monitor_handle_for_window(
            windows::Win32::Foundation::HWND(hwnd as *mut _),
        );
        if h == 0 {
            return None;
        }
        self.monitors.iter().position(|m| m.hmonitor() == h)
    }

    /// The monitor a window should be (re-)seated on: the remembered owner,
    /// then the monitor Windows reports for it, then the one its rect lies
    /// on, then the active monitor. See [`resolve_home_monitor`] for the
    /// rule and why the rect comes last.
    #[must_use]
    pub(super) fn home_monitor_for_hwnd(&self, hwnd: isize) -> usize {
        let remembered = self.remembered_monitor(crate::common::WindowId(hwnd));
        let by_handle = self.monitor_index_for_hmonitor_of(hwnd);
        let by_rect = crate::registry::win32::get_window_rect(windows::Win32::Foundation::HWND(
            hwnd as *mut _,
        ))
        .ok()
        .and_then(|r| self.monitor_index_for_rect_opt(r));
        resolve_home_monitor(
            remembered,
            by_handle,
            by_rect,
            self.active_monitor,
            self.monitors.len(),
        )
    }

    /// Index of the nearest monitor lying beyond `from`'s edge in `dir`, or
    /// `None` when no monitor lies in that direction. See the free function
    /// [`monitor_in_direction`] for the rule.
    #[must_use]
    pub(super) fn monitor_in_direction(
        &self,
        from: usize,
        dir: crate::common::Direction,
    ) -> Option<usize> {
        let rects: Vec<crate::common::Rect> =
            self.monitors.iter().map(|m| m.screen_rect()).collect();
        monitor_in_direction(&rects, from, dir)
    }

    /// Index of the monitor that currently displays `hwnd` (by its on-screen
    /// rect), falling back to the active monitor when the rect is unknown.
    #[must_use]
    pub(super) fn monitor_index_for_hwnd(&self, hwnd: isize) -> usize {
        match crate::registry::win32::get_window_rect(windows::Win32::Foundation::HWND(
            hwnd as *mut _,
        )) {
            Ok(rect) => self.monitor_index_for_rect(rect),
            Err(_) => self.active_monitor,
        }
    }

    /// Borrow the active workspace (the one currently on screen) of the
    /// active monitor.
    ///
    /// This is the workspace every IPC command and hook event operates on
    /// by default. Workspace-switching commands (`switch-workspace`,
    /// `move-to-workspace`) reassign the active index before animating.
    #[must_use]
    #[allow(dead_code)] // API surface: float paths now resolve the owning monitor explicitly.
    pub(super) fn active_workspace(&self) -> &Workspace {
        self.active_monitor().active_workspace()
    }

    /// Mutably borrow the active workspace of the active monitor.
    #[must_use]
    #[allow(dead_code)] // API surface: float paths now resolve the owning monitor explicitly.
    pub(super) fn active_workspace_mut(&mut self) -> &mut Workspace {
        self.active_monitor_mut().active_workspace_mut()
    }

    /// Borrow the scrolling space of the active workspace.
    ///
    /// This is the primary accessor the daemon's hook and dispatch handlers
    /// use to reach the (formerly `LayoutEngine`) tiling state. Every
    /// `self.layout.X(...)` call site was migrated to
    /// `self.active_scrolling[_mut]().X(...)` during the workspace refactor.
    #[must_use]
    pub(super) fn active_scrolling(&self) -> &ScrollingSpace {
        self.active_monitor().active_scrolling()
    }

    /// Mutably borrow the scrolling space of the active workspace.
    #[must_use]
    pub(super) fn active_scrolling_mut(&mut self) -> &mut ScrollingSpace {
        self.active_monitor_mut().active_scrolling_mut()
    }

    /// Borrow the floating space of the active workspace.
    #[must_use]
    #[allow(dead_code)] // API surface for upcoming floating-window management.
    pub(super) fn active_floating(&self) -> &FloatingSpace {
        self.active_monitor().active_floating()
    }

    /// Mutably borrow the floating space of the active workspace.
    #[must_use]
    #[allow(dead_code)] // API surface for upcoming floating-window management.
    pub(super) fn active_floating_mut(&mut self) -> &mut FloatingSpace {
        self.active_monitor_mut().active_floating_mut()
    }
}

/// Pure rule behind [`FlowWM::home_monitor_for_hwnd`]: which monitor a
/// window that is coming back into a layout belongs to.
///
/// Order of trust:
/// 1. `remembered` — the owning-monitor memory recorded when the window was
///    last seated. Survives minimize/hide, during which the window may have
///    been parked beyond the virtual screen.
/// 2. `by_handle` — `MonitorFromWindow` matched to a known `HMONITOR`.
///    Authoritative for a window the daemon has never seated, but for a
///    parked window it is merely the *nearest* monitor to a far-off rect.
/// 3. `by_rect` — the monitor whose bounds contain the window's rect centre.
/// 4. `active` — the monitor that owns keyboard focus.
///
/// Each candidate is only used when it is a valid index below
/// `monitor_count`.
#[must_use]
pub(super) fn resolve_home_monitor(
    remembered: Option<usize>,
    by_handle: Option<usize>,
    by_rect: Option<usize>,
    active: usize,
    monitor_count: usize,
) -> usize {
    let valid = |i: usize| i < monitor_count;
    remembered
        .filter(|&i| valid(i))
        .or_else(|| by_handle.filter(|&i| valid(i)))
        .or_else(|| by_rect.filter(|&i| valid(i)))
        .unwrap_or(active)
}

/// Pure geometry behind [`FlowWM::monitor_index_for_rect_opt`]: the monitor
/// (by index into `screens`) containing `rect`'s centre, else the first one
/// `rect` overlaps, else `None`.
#[must_use]
pub(super) fn monitor_index_for_rect_in(
    screens: &[crate::common::Rect],
    rect: crate::common::Rect,
) -> Option<usize> {
    let cx = rect.x + rect.width / 2;
    let cy = rect.y + rect.height / 2;
    screens
        .iter()
        .position(|s| cx >= s.x && cx < s.right() && cy >= s.y && cy < s.bottom())
        .or_else(|| screens.iter().position(|s| s.overlaps(rect)))
}

/// Pure geometry behind [`FlowWM::monitor_in_direction`].
///
/// Candidates must start past `from`'s edge in `dir` (allowing a few pixels
/// of overlap or gap). Among them the one sharing the most of `from`'s
/// perpendicular extent wins, with distance from the edge as the tie-breaker.
#[must_use]
pub(super) fn monitor_in_direction(
    rects: &[crate::common::Rect],
    from: usize,
    dir: crate::common::Direction,
) -> Option<usize> {
    use crate::common::Direction;
    const SLACK: i32 = 8;
    let f = *rects.get(from)?;
    // (index, negated band overlap, distance) - smaller tuple wins
    let mut best: Option<(usize, i32, i32)> = None;
    for (i, r) in rects.iter().enumerate() {
        if i == from {
            continue;
        }
        let (beyond, distance, overlap) = match dir {
            Direction::Left => (
                r.right() <= f.x + SLACK,
                f.x - r.right(),
                band_overlap(f.y, f.bottom(), r.y, r.bottom()),
            ),
            Direction::Right => (
                r.x >= f.right() - SLACK,
                r.x - f.right(),
                band_overlap(f.y, f.bottom(), r.y, r.bottom()),
            ),
            Direction::Up => (
                r.bottom() <= f.y + SLACK,
                f.y - r.bottom(),
                band_overlap(f.x, f.right(), r.x, r.right()),
            ),
            Direction::Down => (
                r.y >= f.bottom() - SLACK,
                r.y - f.bottom(),
                band_overlap(f.x, f.right(), r.x, r.right()),
            ),
        };
        if !beyond {
            continue;
        }
        let key = (i, -overlap, distance.max(0));
        if best.is_none_or(|b| (key.1, key.2) < (b.1, b.2)) {
            best = Some(key);
        }
    }
    best.map(|b| b.0)
}

/// Length of the overlap between the intervals `[a0, a1)` and `[b0, b1)`.
fn band_overlap(a0: i32, a1: i32, b0: i32, b1: i32) -> i32 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

#[cfg(test)]
mod monitor_direction_tests {
    use super::monitor_in_direction;
    use crate::common::{Direction, Rect};

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn side_by_side_pair() {
        // primary at 0, secondary to its left (the user's real layout)
        let rects = [r(0, 0, 1920, 1080), r(-1920, 0, 1920, 1080)];
        assert_eq!(monitor_in_direction(&rects, 0, Direction::Left), Some(1));
        assert_eq!(monitor_in_direction(&rects, 0, Direction::Right), None);
        assert_eq!(monitor_in_direction(&rects, 1, Direction::Right), Some(0));
        assert_eq!(monitor_in_direction(&rects, 1, Direction::Left), None);
        assert_eq!(monitor_in_direction(&rects, 0, Direction::Up), None);
        assert_eq!(monitor_in_direction(&rects, 0, Direction::Down), None);
    }

    #[test]
    fn three_in_a_row_picks_the_adjacent_one() {
        let rects = [
            r(0, 0, 1920, 1080),
            r(1920, 0, 1920, 1080),
            r(3840, 0, 1920, 1080),
        ];
        assert_eq!(monitor_in_direction(&rects, 0, Direction::Right), Some(1));
        assert_eq!(monitor_in_direction(&rects, 2, Direction::Left), Some(1));
        assert_eq!(monitor_in_direction(&rects, 1, Direction::Left), Some(0));
    }

    #[test]
    fn stacked_pair_uses_up_and_down() {
        let rects = [r(0, 0, 1920, 1080), r(0, -1080, 1920, 1080)];
        assert_eq!(monitor_in_direction(&rects, 0, Direction::Up), Some(1));
        assert_eq!(monitor_in_direction(&rects, 1, Direction::Down), Some(0));
        assert_eq!(monitor_in_direction(&rects, 0, Direction::Left), None);
    }

    #[test]
    fn prefers_the_monitor_sharing_the_most_band() {
        // Two monitors to the right: one mostly above us, one level with us.
        let rects = [
            r(0, 0, 1920, 1080),
            r(1920, -900, 1920, 1080),
            r(1920, 100, 1920, 1080),
        ];
        assert_eq!(monitor_in_direction(&rects, 0, Direction::Right), Some(2));
    }

    #[test]
    fn out_of_range_from_is_none() {
        let rects = [r(0, 0, 1920, 1080)];
        assert_eq!(monitor_in_direction(&rects, 5, Direction::Left), None);
    }
}

#[cfg(test)]
mod home_monitor_tests {
    use super::{monitor_index_for_rect_in, resolve_home_monitor};
    use crate::common::Rect;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn remembered_monitor_wins_over_handle_and_rect() {
        // A window minimized while parked beyond the virtual screen: the
        // rect says "nowhere", MonitorFromWindow says "nearest" (0), but the
        // memory says it lived on monitor 1.
        assert_eq!(resolve_home_monitor(Some(1), Some(0), None, 0, 2), 1);
        assert_eq!(resolve_home_monitor(Some(1), Some(0), Some(0), 0, 2), 1);
    }

    #[test]
    fn handle_beats_rect_when_nothing_is_remembered() {
        assert_eq!(resolve_home_monitor(None, Some(1), Some(0), 0, 2), 1);
    }

    #[test]
    fn rect_is_used_when_handle_is_unknown() {
        assert_eq!(resolve_home_monitor(None, None, Some(1), 0, 2), 1);
    }

    #[test]
    fn falls_back_to_the_active_monitor() {
        assert_eq!(resolve_home_monitor(None, None, None, 1, 2), 1);
    }

    #[test]
    fn stale_indices_are_skipped() {
        // Memory from a three-monitor session on a now two-monitor desktop.
        assert_eq!(resolve_home_monitor(Some(2), Some(1), None, 0, 2), 1);
        assert_eq!(resolve_home_monitor(Some(2), Some(5), Some(7), 0, 2), 0);
    }

    #[test]
    fn rect_centre_picks_the_monitor() {
        let screens = [r(0, 0, 1920, 1080), r(-1920, 0, 1920, 1080)];
        assert_eq!(
            monitor_index_for_rect_in(&screens, r(-1000, 100, 400, 300)),
            Some(1)
        );
        assert_eq!(
            monitor_index_for_rect_in(&screens, r(100, 100, 400, 300)),
            Some(0)
        );
    }

    #[test]
    fn rect_overlap_is_the_second_choice_and_parked_rects_are_none() {
        let screens = [r(0, 0, 1920, 1080), r(-1920, 0, 1920, 1080)];
        // Centre off both monitors (below), but the top edge overlaps 0.
        assert_eq!(
            monitor_index_for_rect_in(&screens, r(100, 1000, 400, 400)),
            Some(0)
        );
        // Parked far below the virtual screen: no monitor at all.
        assert_eq!(
            monitor_index_for_rect_in(&screens, r(100, 5000, 400, 300)),
            None
        );
    }
}
