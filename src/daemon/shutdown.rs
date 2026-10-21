//! Graceful-shutdown window rescue and fail-back.
//!
//! When the daemon stops, windows stranded off-screen (parked in non-active
//! workspaces at ±1 monitor height, or scrolled into the packing region)
//! would become unreachable. This module brings them back into the active
//! monitor's [`work_area`](crate::workspace::Monitor::work_area) without
//! disturbing windows the user can currently see.
//!
//! [`FlowWM::fail_back`] is the one teardown every exit path goes through —
//! clean `flow stop`, session end, a dead worker thread, a panic — and it
//! hands the desktop back to plain Windows in a fixed order:
//!
//! 1. `animator.cancel_and_wait()` — no further `SetWindowPos` from the
//!    worker, so step 2 cannot be overwritten by a still-running tween.
//! 2. [`rescue_stranded_windows`](FlowWM::rescue_stranded_windows) — if
//!    step 1 was not acknowledged in time, the pass runs twice with another
//!    bounded wait in between, so a late frame cannot undo it. The pass
//!    also maximizes again every window the daemon had un-maximized to
//!    manage (`Window::adopted_from_maximized`), so the desktop is handed
//!    back as the user had it, and takes every window out of the floating
//!    layer (`[floating] always_on_top`, `daemon::top_layer`), putting the
//!    topmost flag it had back.
//! 3. Drop every border overlay (`DestroyWindow`).
//! 4. Stop the hook thread (`WM_QUIT`).
//! 5. Kill the hotkey script and remove its pidfile, releasing the Win key.
//!    (The kill-on-close job object is the backstop for the exits that never
//!    reach this function.)
//! 6. Stop the IPC server's pending accept
//!    ([`PipeServer::shutdown`](crate::ipc::transport::PipeServer::shutdown)):
//!    a synchronous `ConnectNamedPipe` left pending would make closing the
//!    pipe handle block until the next client connected, keeping the process
//!    alive after the teardown.
//!
//! The status file is rewritten by `main` afterwards (`stopped` or
//! `error`/`runtime`), which also owns the exit code.
//!
//! See `docs/src/dev-guide/ipc-and-watchdog.md` for the design rationale
//! (why visible windows are left untouched, and the "pre-this-run" contract
//! on the rescue anchor).

use windows::Win32::Foundation::HWND;

use super::types::FlowWM;
use crate::common::{InvisibleBounds, Rect, WindowId};
use crate::registry::WindowRegistry;
use crate::registry::win32;

/// The per-monitor geometry the rescue pass needs.
///
/// A plain value type so the pass can run before a `FlowWM` (and its
/// `Vec<Monitor>`) exists — i.e. from a failing `FlowWM::new` after the
/// initial layout has already moved windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RescueMonitor {
    /// Full physical display bounds.
    pub screen_rect: Rect,
    /// Taskbar-excluded work area windows are clamped into.
    pub work_area: Rect,
}

/// Rescue windows stranded off-screen without a `FlowWM`.
///
/// This is the rescue pass itself (see
/// [`FlowWM::rescue_stranded_windows`] for the rules). It is a free function
/// so that a startup failure *after* the initial layout has moved windows can
/// still bring them back — at that point no `FlowWM` exists yet.
///
/// * `registry` — supplies the controlled windows and their rescue anchors.
/// * `virtual_screen` — union of all monitors; a window whose visible content
///   overlaps it is left alone.
/// * `monitors` — screen/work rects, in the same order as the daemon's
///   monitor list; must be non-empty.
/// * `active_monitor` — fallback owner when no monitor contains the anchor.
/// * `owner_of` — monitor index owning a window by workspace membership, or
///   `None` to fall back to the anchor's monitor. Before `FlowWM` exists no
///   workspace bookkeeping is available, so pass `|_| None`.
///
/// Minimized windows ([`minimized_windows`](WindowRegistry::minimized_windows))
/// get a second pass: their `WINDOWPLACEMENT` normal position is clamped
/// on-screen (see
/// [`rescue_minimized_placement`](crate::rescue::rescue_minimized_placement))
/// without un-minimizing them. A third pass maximizes again every window
/// the daemon un-maximized to manage
/// ([`adopted_maximized_windows`](WindowRegistry::adopted_maximized_windows)),
/// after it has been brought on-screen, and puts the foreground back
/// (`SW_MAXIMIZE` activates).
///
/// Returns the number of windows moved, re-anchored or re-maximized.
pub fn rescue_stranded_windows_with(
    registry: &WindowRegistry,
    virtual_screen: Rect,
    monitors: &[RescueMonitor],
    active_monitor: usize,
    owner_of: impl Fn(WindowId) -> Option<usize>,
) -> usize {
    if monitors.is_empty() {
        log::warn!("shutdown rescue: no monitors known, cannot rescue");
        return 0;
    }
    let active_monitor = active_monitor.min(monitors.len() - 1);
    let mut rescued = 0_usize;
    for (hwnd_val, anchor, invisible_bounds) in registry.restorable_windows() {
        let hwnd = HWND(hwnd_val as *mut _);
        let Ok(current) = win32::get_window_rect(hwnd) else {
            log::warn!("shutdown rescue: GetWindowRect failed for hwnd {hwnd_val}, skipping");
            continue;
        };
        if content_overlaps_work_area(current, invisible_bounds, virtual_screen) {
            continue;
        }
        // Bring it back onto the monitor that owns it (by workspace
        // membership), else the one its pre-manage rect was on, else the
        // active monitor.
        let owner = owner_of(WindowId(hwnd_val))
            .filter(|i| *i < monitors.len())
            .unwrap_or_else(|| monitor_index_for_rect(monitors, anchor, active_monitor));
        let work_area = monitors[owner].work_area;
        let target = anchor.clamped_into(work_area);
        if win32::set_window_rect(hwnd_val, target.x, target.y, target.width, target.height) {
            rescued += 1;
        }
    }
    let mut re_anchored = 0_usize;
    for (hwnd_val, invisible_bounds) in registry.minimized_windows() {
        let hwnd = HWND(hwnd_val as *mut _);
        let owner = owner_of(WindowId(hwnd_val)).filter(|i| *i < monitors.len());
        match crate::rescue::rescue_minimized_placement(hwnd, invisible_bounds, monitors, owner) {
            Ok(Some((from, to))) => {
                log::info!(
                    "shutdown rescue: re-anchored minimized hwnd {hwnd_val} from ({},{}) to ({},{})",
                    from.x,
                    from.y,
                    to.x,
                    to.y
                );
                re_anchored += 1;
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!(
                    "shutdown rescue: placement of minimized hwnd {hwnd_val} not fixed: {e}"
                );
            }
        }
    }
    let adopted = registry.adopted_maximized_windows();
    let mut re_maximized = 0_usize;
    if !adopted.is_empty() {
        let foreground = win32::get_foreground_window();
        for &hwnd_val in &adopted {
            if win32::maximize_window(hwnd_val) {
                re_maximized += 1;
            }
        }
        if let Some(fg) = foreground
            && !adopted.contains(&fg)
            && win32::is_window(fg)
        {
            let _ = win32::set_foreground_window(fg);
        }
    }
    log::info!(
        "shutdown rescue: brought {rescued} stranded window(s) back on-screen, re-anchored {re_anchored} minimized window(s), maximized {re_maximized} adopted window(s) again"
    );
    rescued + re_anchored + re_maximized
}

/// Index of the monitor whose screen rect contains `rect`'s centre, else the
/// first whose screen rect overlaps it, else `fallback`. Mirrors
/// `FlowWM::monitor_index_for_rect` over plain geometry.
fn monitor_index_for_rect(monitors: &[RescueMonitor], rect: Rect, fallback: usize) -> usize {
    let cx = rect.x + rect.width / 2;
    let cy = rect.y + rect.height / 2;
    if let Some(i) = monitors.iter().position(|m| {
        let s = m.screen_rect;
        cx >= s.x && cx < s.right() && cy >= s.y && cy < s.bottom()
    }) {
        return i;
    }
    monitors
        .iter()
        .position(|m| m.screen_rect.overlaps(rect))
        .unwrap_or(fallback)
}

impl FlowWM {
    /// Rescue windows stranded off-screen back into the active monitor's
    /// `work_area`. Called once during graceful shutdown, after the event
    /// loop exits and before the daemon process tears down.
    ///
    /// For each window flow controls ([`restorable_windows`][crate::registry::WindowRegistry::restorable_windows]):
    ///
    /// 1. Ask Win32 for the window's current rect (`GetWindowRect`) — ground
    ///    truth from the OS, not workspace bookkeeping.
    /// 2. If its visible content (window rect minus invisible borders) overlaps
    ///    the active monitor's `work_area`, leave it alone: the user can see it,
    ///    and touching it would disturb their in-progress arrangement.
    /// 3. Otherwise, move it to its `pre_manage_rect` anchor, clamped into the
    ///    `work_area` if the anchor is itself off-screen.
    ///
    /// `Ignored` and `Hidden` windows never reach this loop — the registry
    /// accessor filters them out, since flow does not actively place them.
    /// `Minimized` windows get their restore placement re-anchored instead,
    /// and windows adopted from maximized are maximized again (see
    /// `rescue_stranded_windows_with`).
    ///
    /// Failures (window vanished mid-loop, `SetWindowPos` rejected) are logged
    /// at `warn` and skipped: one bad window must not abort the rescue pass.
    ///
    /// If the animator does not acknowledge the cancel within its timeout, a
    /// frame may still land after the first pass; the pass is then run a
    /// second time after another bounded wait.
    pub fn rescue_stranded_windows(&mut self) -> usize {
        // Stop the worker first: a tween still in flight would otherwise
        // re-apply its interpolated rects over the rescued positions.
        let acknowledged = self.animator.cancel_and_wait();
        let rescued = self.rescue_stranded_windows_pass();
        if acknowledged {
            return rescued;
        }
        // The worker was silent (wedged in a backend call, or dead). Give it
        // one more bounded chance to drain, then undo whatever a late frame
        // may have re-applied.
        let late_ack = self.animator.cancel_and_wait();
        log::warn!(
            "shutdown rescue: animator did not acknowledge the cancel before the first pass ({}); running the rescue again",
            if late_ack {
                "acknowledged now"
            } else {
                "still silent"
            }
        );
        rescued + self.rescue_stranded_windows_pass()
    }

    /// One rescue pass over the current registry and monitor geometry.
    fn rescue_stranded_windows_pass(&mut self) -> usize {
        // The floating layer: every window the daemon holds in the topmost
        // band goes back to the flag it had (`daemon::top_layer`), whatever
        // `[floating] always_on_top` says.
        let _ = self.release_top_layer();
        // Fullscreen windows: the topmost flag is the daemon's and must not
        // outlive it; the window itself goes back inside its work area.
        for (hwnd, rect) in self.demote_all_fullscreen_to_floats() {
            let bounds = self
                .registry
                .get_window(HWND(hwnd as *mut _))
                .map(|w| w.invisible_bounds)
                .unwrap_or_default();
            let target = bounds.visible_to_window(rect);
            let _ = win32::set_window_rect(hwnd, target.x, target.y, target.width, target.height);
        }
        // "Stranded" means off every monitor, not merely off the active one:
        // a window parked beyond the virtual screen must come back, a window
        // that lives on another monitor must be left alone.
        let virtual_screen = self.active_scrolling().monitor().virtual_screen;
        let monitors: Vec<RescueMonitor> = self
            .monitors
            .iter()
            .map(|m| RescueMonitor {
                screen_rect: m.screen_rect(),
                work_area: m.work_area(),
            })
            .collect();
        rescue_stranded_windows_with(
            &self.registry,
            virtual_screen,
            &monitors,
            self.active_monitor,
            |wid| self.monitor_index_containing(wid),
        )
    }

    /// Hand the desktop back to plain Windows: the single teardown used by
    /// every exit path (see the module docs for the order and why).
    ///
    /// `reason` is logged (at `info` for a clean stop, `error` otherwise)
    /// and is what the status file will carry. Every step is best-effort
    /// and independent: a failure in one is logged and the next still runs.
    /// Calling it twice is harmless (the second call finds nothing to do).
    pub fn fail_back(&mut self, reason: &str, clean: bool) {
        if clean {
            log::info!("shutdown: {reason}");
        } else {
            log::error!("fail-back: {reason}");
        }
        self.shutting_down = true;

        // 1 + 2. Stop the animator and bring stranded windows back.
        let rescued = self.rescue_stranded_windows();

        // 3. Borders: overlays are our windows; destroy them so nothing of
        //    the daemon stays on screen.
        let borders = self.registry.detach_all_borders();

        // 4. Hook thread: post WM_QUIT; it unhooks and exits on its own.
        self.hook_handle.stop();

        // 5. Hotkey script: explicit kill (orderly) + pidfile removal. The
        //    job object would also kill it when the process exits; doing it
        //    here means the Win key is free before we return.
        let ahk = match self.ahk.take() {
            Some(proc) => {
                let pid = proc.pid();
                if proc.stop() {
                    log::info!("fail-back: stopped flow.ahk (pid {pid})");
                    Some(pid)
                } else {
                    log::info!("fail-back: flow.ahk (pid {pid}) was already gone");
                    None
                }
            }
            None => {
                // Not launched by this daemon (or already stopped): still
                // honour a pidfile left by an older `flow start --ahk`.
                match crate::autostart::stop_ahk_script_in(&self.config_dir) {
                    Ok(true) => {
                        log::info!("fail-back: stopped a flow.ahk recorded in the pidfile");
                    }
                    Ok(false) => {}
                    Err(e) => log::warn!("fail-back: stopping flow.ahk: {e}"),
                }
                None
            }
        };

        // 6. IPC: cancel the pending accept and join its thread so dropping
        //    the server (and exiting) cannot block on the pipe handle.
        let accept_stopped = self.server.shutdown();

        log::info!(
            "fail-back complete: {rescued} window(s) rescued, {borders} border(s) dropped ({} tracked), hook thread stopped, ahk {}, ipc accept {}",
            self.registry.windows().count(),
            match ahk {
                Some(pid) => format!("pid {pid} killed"),
                None => "not running".to_owned(),
            },
            if accept_stopped { "stopped" } else { "LEAKED" }
        );
        crate::registry::hooks::mark_shutdown_complete();
    }
}

fn content_overlaps_work_area(window_rect: Rect, bounds: InvisibleBounds, work_area: Rect) -> bool {
    // GetWindowRect includes invisible borders, so a window parked flush at the
    // work-area edge bleeds a few pixels into it. Stripping the borders first
    // recovers the visible rect; parked windows then only touch (not overlap)
    // the edge and are correctly treated as stranded.
    bounds.window_to_visible(window_rect).overlaps(work_area)
}

#[cfg(test)]
mod tests {
    use crate::common::{InvisibleBounds, Rect};

    use super::content_overlaps_work_area;

    // Typical Win11 invisible borders.
    const BOUNDS: InvisibleBounds = InvisibleBounds {
        left: 7,
        top: 0,
        right: 7,
        bottom: 7,
    };
    // 1920x1040 work area (taskbar excluded).
    const WORK_AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1040,
    };

    #[test]
    fn parked_right_window_is_not_visible_despite_border_bleed() {
        // Visible left edge at x=1920; GetWindowRect reports x=1913 due to the
        // 7px left invisible border, so the raw rect overlaps the work area.
        // The visible-content rect only touches the edge → stranded → rescue.
        let window_rect = Rect {
            x: 1913,
            y: 0,
            width: 974, // visible 960 + 7 + 7
            height: 607,
        };
        assert!(!content_overlaps_work_area(window_rect, BOUNDS, WORK_AREA));
    }

    #[test]
    fn parked_left_window_is_not_visible_despite_border_bleed() {
        // Mirror of the right case: visible right edge at x=0, window right edge
        // bleeds to x=7. Visible-content rect only touches → stranded → rescue.
        let window_rect = Rect {
            x: -967,
            y: 0,
            width: 974,
            height: 607,
        };
        assert!(!content_overlaps_work_area(window_rect, BOUNDS, WORK_AREA));
    }

    #[test]
    fn stacked_pair_far_parked_workspace_window_is_not_visible() {
        // Laptop (0,0,1920,1080) under a 2560x1440 external: the virtual
        // screen is (0,-1440,2560,2520). A laptop window in a workspace
        // parked above the pair sits with its visible bottom at -1456.
        let virtual_screen = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 2520,
        };
        let visible = Rect {
            x: 300,
            y: -1456 - 1000,
            width: 800,
            height: 1000,
        };
        let window_rect = BOUNDS.visible_to_window(visible);
        assert!(!content_overlaps_work_area(
            window_rect,
            BOUNDS,
            virtual_screen
        ));
        // Below the pair, flush with the virtual screen's bottom + margin.
        let visible = Rect {
            x: 300,
            y: 1080 + 1440 + 16,
            width: 800,
            height: 1000,
        };
        let window_rect = BOUNDS.visible_to_window(visible);
        assert!(!content_overlaps_work_area(
            window_rect,
            BOUNDS,
            virtual_screen
        ));
        // The old one-height slot is on the external: visible, not rescued.
        let visible = Rect {
            x: 300,
            y: -1084,
            width: 800,
            height: 1000,
        };
        let window_rect = BOUNDS.visible_to_window(visible);
        assert!(content_overlaps_work_area(
            window_rect,
            BOUNDS,
            virtual_screen
        ));
    }

    #[test]
    fn genuinely_on_screen_window_is_visible() {
        // A window squarely inside the work area stays visible → no rescue.
        let visible = Rect {
            x: 100,
            y: 100,
            width: 800,
            height: 600,
        };
        let window_rect = BOUNDS.visible_to_window(visible);
        assert!(content_overlaps_work_area(window_rect, BOUNDS, WORK_AREA));
    }

    #[test]
    fn parked_flush_with_zero_bounds_is_not_visible() {
        // Borderless windows have no bleed; the raw rect already only touches
        // the edge. Either way they must be rescued.
        let window_rect = Rect {
            x: 1920,
            y: 0,
            width: 960,
            height: 600,
        };
        assert!(!content_overlaps_work_area(
            window_rect,
            InvisibleBounds::zero(),
            WORK_AREA,
        ));
    }

    #[test]
    fn inter_workspace_vertically_parked_window_is_not_visible() {
        // Inter-workspace windows are parked a full monitor-height above/below
        // the active work_area. These were already rescued correctly before
        // the border-bleed fix (the bleed is irrelevant at ±1040px); this pins
        // that the fix did not regress that path. It is also the only test
        // here that forces the y-axis conjunct of `overlaps` false — every
        // other test keeps the visible-y range inside the work_area, so the
        // decision always came down to the x-axis.
        let visible_above = Rect {
            x: 0,
            y: -1040,
            width: 960,
            height: 600,
        };
        let window_rect = BOUNDS.visible_to_window(visible_above);

        assert!(!content_overlaps_work_area(window_rect, BOUNDS, WORK_AREA));
    }
}
