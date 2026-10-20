//! Daemon-less rescue of off-screen windows (`flow rescue`).
//!
//! When a previous daemon died without running its shutdown rescue (crash,
//! `taskkill /f`, power loss), the windows it had parked beyond the virtual
//! screen stay there: visible to Win32, unreachable by mouse. This module
//! brings them back **without** a daemon: it enumerates the visible
//! top-level windows the daemon would have managed, and every one whose
//! visible content does not overlap the virtual screen is clamped into the
//! work area of the monitor it most plausibly belongs to.
//!
//! The geometry is pure ([`rescue_target`]) so it is unit-tested over
//! rects; [`rescue_offscreen_windows`] is the thin Win32 driver. `flow
//! start` runs it before spawning a daemon so a crashed previous run is
//! cleaned up on the next start; `flow rescue` runs it on demand.

use crate::common::{InvisibleBounds, Rect};
use crate::daemon::shutdown::RescueMonitor;
use crate::registry::core::{enum_toplevel_windows, has_owner, has_parent};
use crate::registry::win32;

/// One window moved by the rescue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescuedWindow {
    /// Window title at the time of the rescue.
    pub title: String,
    /// Visible rect before the move.
    pub from: Rect,
    /// Visible rect after the move.
    pub to: Rect,
}

/// Outcome of [`rescue_offscreen_windows`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RescueReport {
    /// Candidate windows examined (visible, titled, Alt-Tab-visible,
    /// unowned top-level windows).
    pub scanned: usize,
    /// Windows that were off-screen and got moved.
    pub moved: Vec<RescuedWindow>,
}

/// Bounding rectangle of a set of monitors' screen rects.
#[must_use]
pub fn virtual_screen_of(monitors: &[RescueMonitor]) -> Option<Rect> {
    let first = monitors.first()?.screen_rect;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.right(), first.bottom());
    for m in &monitors[1..] {
        let s = m.screen_rect;
        x0 = x0.min(s.x);
        y0 = y0.min(s.y);
        x1 = x1.max(s.right());
        y1 = y1.max(s.bottom());
    }
    Some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// Length of the overlap of `[a0, a1)` and `[b0, b1)`, clamped at zero.
fn band_overlap(a0: i32, a1: i32, b0: i32, b1: i32) -> i32 {
    (a1.min(b1) - a0.max(b0)).max(0)
}

/// Index of the monitor an off-screen window belongs to.
///
/// Parked windows keep one coordinate of their home monitor: a window
/// parked left/right of its monitor still lies in that monitor's horizontal
/// band (its y-range), one parked above/below still lies in its vertical
/// band (x-range). The monitor sharing the most band extent wins; when no
/// band overlaps (diagonal), the one whose screen rect is nearest to the
/// window's centre does.
#[must_use]
pub fn home_monitor_index(visible: Rect, monitors: &[RescueMonitor]) -> Option<usize> {
    if monitors.is_empty() {
        return None;
    }
    let cx = visible.x + visible.width / 2;
    let cy = visible.y + visible.height / 2;
    let mut best: Option<(usize, i32, i64)> = None; // (index, band score, distance²)
    for (i, m) in monitors.iter().enumerate() {
        let s = m.screen_rect;
        let score = band_overlap(visible.y, visible.bottom(), s.y, s.bottom())
            + band_overlap(visible.x, visible.right(), s.x, s.right());
        let dx = i64::from((cx - s.x.max(cx.min(s.right() - 1))).abs());
        let dy = i64::from((cy - s.y.max(cy.min(s.bottom() - 1))).abs());
        let dist = dx * dx + dy * dy;
        let better = match best {
            None => true,
            Some((_, best_score, best_dist)) => {
                score > best_score || (score == best_score && dist < best_dist)
            }
        };
        if better {
            best = Some((i, score, dist));
        }
    }
    best.map(|(i, _, _)| i)
}

/// Where an off-screen window should go, or `None` if it is on-screen.
///
/// `visible` is the window's visible-content rect (invisible borders
/// stripped). A window whose visible content overlaps the virtual screen
/// (the union of all monitors) is left alone. Otherwise it is clamped into
/// the work area of its [`home_monitor_index`] monitor, preserving its size
/// where possible.
#[must_use]
pub fn rescue_target(visible: Rect, monitors: &[RescueMonitor]) -> Option<Rect> {
    let virtual_screen = virtual_screen_of(monitors)?;
    if visible.overlaps(virtual_screen) {
        return None;
    }
    let owner = home_monitor_index(visible, monitors)?;
    Some(visible.clamped_into(monitors[owner].work_area))
}

/// Enumerate the monitors as [`RescueMonitor`]s (primary first).
///
/// # Errors
///
/// Returns the enumeration error as a string.
pub fn current_monitors() -> Result<Vec<RescueMonitor>, String> {
    Ok(win32::enumerate_monitors()?
        .into_iter()
        .map(|g| RescueMonitor {
            screen_rect: g.screen_rect,
            work_area: g.work_area,
        })
        .collect())
}

/// Bring every off-screen top-level window back onto a monitor.
///
/// Candidates are the windows the daemon's initial scan would consider:
/// visible, titled, Alt-Tab-visible, not minimized, not owned, not a child.
/// For each, the visible-content rect is computed from `GetWindowRect` and
/// the window's invisible borders; [`rescue_target`] decides whether and
/// where to move it, and `SetWindowPos` (no activation, z-order kept) does
/// the move.
///
/// # Errors
///
/// Returns a string if monitors or windows cannot be enumerated.
pub fn rescue_offscreen_windows() -> Result<RescueReport, String> {
    let monitors = current_monitors()?;
    let hwnds = enum_toplevel_windows()?;
    let mut report = RescueReport::default();
    for hwnd in hwnds {
        if !win32::is_window_visible(hwnd) || win32::is_iconic(hwnd) {
            continue;
        }
        let title = win32::get_window_text(hwnd).unwrap_or_default();
        if title.is_empty() || !win32::is_alt_tab_visible(hwnd) {
            continue;
        }
        if has_owner(hwnd) || has_parent(hwnd) {
            continue;
        }
        report.scanned += 1;
        let Ok(window_rect) = win32::get_window_rect(hwnd) else {
            continue;
        };
        let bounds: InvisibleBounds = win32::get_invisible_bounds(hwnd);
        let visible = bounds.window_to_visible(window_rect);
        let Some(target) = rescue_target(visible, &monitors) else {
            continue;
        };
        let target_window = bounds.visible_to_window(target);
        let hwnd_val = hwnd.0 as isize;
        if win32::set_window_rect(
            hwnd_val,
            target_window.x,
            target_window.y,
            target_window.width,
            target_window.height,
        ) {
            log::info!(
                "rescue: moved {title:?} from ({},{}) to ({},{})",
                visible.x,
                visible.y,
                target.x,
                target.y
            );
            report.moved.push(RescuedWindow {
                title,
                from: visible,
                to: target,
            });
        } else {
            log::warn!("rescue: SetWindowPos failed for {title:?} (hwnd {hwnd_val})");
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: i32, h: i32) -> RescueMonitor {
        RescueMonitor {
            screen_rect: Rect {
                x,
                y,
                width: w,
                height: h,
            },
            // 40 px taskbar at the bottom.
            work_area: Rect {
                x,
                y,
                width: w,
                height: h - 40,
            },
        }
    }

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// Two side-by-side monitors; the second one is 1440p and taller.
    fn two_monitors() -> Vec<RescueMonitor> {
        vec![mon(0, 0, 1920, 1080), mon(1920, -180, 2560, 1440)]
    }

    #[test]
    fn virtual_screen_is_union_of_screen_rects() {
        assert_eq!(
            virtual_screen_of(&two_monitors()),
            Some(r(0, -180, 4480, 1440))
        );
        assert_eq!(virtual_screen_of(&[]), None);
        assert_eq!(
            virtual_screen_of(&[mon(-1920, 0, 1920, 1080)]),
            Some(r(-1920, 0, 1920, 1080))
        );
    }

    #[test]
    fn on_screen_windows_are_left_alone() {
        let m = two_monitors();
        assert_eq!(rescue_target(r(100, 100, 800, 600), &m), None);
        // Straddling the edge of the virtual screen still counts as visible.
        assert_eq!(rescue_target(r(4000, 0, 960, 600), &m), None);
        assert_eq!(rescue_target(r(-500, 0, 960, 600), &m), None);
        // On the second monitor's extra height (y = -180 .. 0).
        assert_eq!(rescue_target(r(2000, -170, 400, 300), &m), None);
    }

    #[test]
    fn window_parked_left_of_first_monitor_returns_to_it() {
        let m = two_monitors();
        // Flush against the left edge: only touches, so stranded.
        let parked = r(-960, 0, 960, 600);
        assert_eq!(rescue_target(parked, &m), Some(r(0, 0, 960, 600)));
        assert_eq!(home_monitor_index(parked, &m), Some(0));
    }

    #[test]
    fn window_parked_right_of_second_monitor_returns_to_it() {
        let m = two_monitors();
        let parked = r(4480 + 16, 100, 960, 600);
        assert_eq!(home_monitor_index(parked, &m), Some(1));
        // Clamped so its right edge sits at the monitor's right edge.
        assert_eq!(
            rescue_target(parked, &m),
            Some(r(4480 - 960, 100, 960, 600))
        );
    }

    #[test]
    fn window_parked_above_or_below_keeps_its_x_band() {
        let m = two_monitors();
        // Parked one monitor-height above monitor 0.
        let above = r(300, -1080 - 200, 800, 600);
        assert_eq!(home_monitor_index(above, &m), Some(0));
        assert_eq!(rescue_target(above, &m), Some(r(300, 0, 800, 600)));
        // Parked below monitor 1 (its bottom is 1260).
        let below = r(2500, 1260 + 50, 800, 600);
        assert_eq!(home_monitor_index(below, &m), Some(1));
        // Work area bottom is 1260 - 40 = 1220 → y = 1220 - 600.
        assert_eq!(rescue_target(below, &m), Some(r(2500, 620, 800, 600)));
    }

    #[test]
    fn diagonal_window_goes_to_nearest_monitor() {
        let m = two_monitors();
        // Beyond the top-right corner of monitor 1: no band overlap at all.
        let diag = r(4600, -900, 400, 300);
        assert_eq!(home_monitor_index(diag, &m), Some(1));
        assert_eq!(rescue_target(diag, &m), Some(r(4480 - 400, -180, 400, 300)));
        // Beyond the bottom-left corner of monitor 0.
        let diag0 = r(-700, 1300, 400, 300);
        assert_eq!(home_monitor_index(diag0, &m), Some(0));
        assert_eq!(rescue_target(diag0, &m), Some(r(0, 1040 - 300, 400, 300)));
    }

    #[test]
    fn oversized_window_is_shrunk_into_the_work_area() {
        let m = vec![mon(0, 0, 1920, 1080)];
        let huge = r(-5000, 0, 4000, 3000);
        assert_eq!(rescue_target(huge, &m), Some(r(0, 0, 1920, 1040)));
    }

    #[test]
    fn single_monitor_with_negative_origin() {
        // A lone monitor whose origin is negative (secondary-only layout).
        let m = vec![mon(-2560, -1440, 2560, 1440)];
        let parked = r(16, -1000, 800, 600); // right of the monitor
        assert_eq!(rescue_target(parked, &m), Some(r(-800, -1000, 800, 600)));
        assert_eq!(rescue_target(r(-1000, -1000, 800, 600), &m), None);
    }

    #[test]
    fn three_monitors_middle_band_wins_over_distance() {
        // Left, middle, right in a row; a window parked far below the middle
        // one shares the middle one's x-band and must go there even though
        // its centre is closer to... itself: band beats distance.
        let m = vec![
            mon(-1920, 0, 1920, 1080),
            mon(0, 0, 1920, 1080),
            mon(1920, 0, 1920, 1080),
        ];
        let below_middle = r(500, 5000, 800, 600);
        assert_eq!(home_monitor_index(below_middle, &m), Some(1));
        assert_eq!(
            rescue_target(below_middle, &m),
            Some(r(500, 1040 - 600, 800, 600))
        );
        // Parked left of the leftmost monitor.
        assert_eq!(
            rescue_target(r(-3000, 10, 800, 600), &m),
            Some(r(-1920, 10, 800, 600))
        );
    }

    #[test]
    fn no_monitors_means_no_target() {
        assert_eq!(rescue_target(r(-5000, 0, 100, 100), &[]), None);
        assert_eq!(home_monitor_index(r(0, 0, 1, 1), &[]), None);
    }
}
