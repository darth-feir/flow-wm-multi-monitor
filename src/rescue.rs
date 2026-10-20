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
//! Minimized windows need a different move: their `GetWindowRect` is the
//! iconic (-32000, -32000) placeholder, and what strands them is the
//! *normal position* in their `WINDOWPLACEMENT` — the rect `SW_RESTORE`
//! will use. A window minimized while its column was parked keeps the
//! off-screen rect there and reappears off-screen after the daemon is
//! gone. For those, `placement_target` clamps the normal position into
//! the home monitor's work area and the driver writes it back with
//! `SetWindowPlacement`, keeping the window minimized.
//!
//! The geometry is pure (`rescue_target`, `placement_target`) so it is
//! unit-tested over rects; [`rescue_offscreen_windows`] is the thin Win32
//! driver. `flow start` runs it before spawning a daemon so a crashed
//! previous run is cleaned up on the next start; `flow rescue` runs it on
//! demand. The daemon's own shutdown rescue
//! (`daemon::shutdown::rescue_stranded_windows_with`)
//! reuses `rescue_minimized_placement` for its minimized windows.

use windows::Win32::Foundation::HWND;

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
    /// unowned top-level windows, minimized ones included).
    pub scanned: usize,
    /// Windows that were off-screen and got moved.
    pub moved: Vec<RescuedWindow>,
    /// Minimized windows whose restore position was off-screen and got
    /// re-anchored (`from`/`to` are the normal-position rects, in workspace
    /// coordinates). They stay minimized.
    pub minimized: Vec<RescuedWindow>,
}

/// Bounding rectangle of a set of monitors' screen rects.
#[must_use]
pub(crate) fn virtual_screen_of(monitors: &[RescueMonitor]) -> Option<Rect> {
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
pub(crate) fn home_monitor_index(visible: Rect, monitors: &[RescueMonitor]) -> Option<usize> {
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
pub(crate) fn rescue_target(visible: Rect, monitors: &[RescueMonitor]) -> Option<Rect> {
    rescue_target_on(visible, monitors, None)
}

/// `rescue_target` with an optional known owner: when `owner` is a valid
/// index it wins over the geometric [`home_monitor_index`] guess (the
/// daemon knows which monitor a window's workspace belongs to).
fn rescue_target_on(
    visible: Rect,
    monitors: &[RescueMonitor],
    owner: Option<usize>,
) -> Option<Rect> {
    let virtual_screen = virtual_screen_of(monitors)?;
    if visible.overlaps(virtual_screen) {
        return None;
    }
    let owner = owner
        .filter(|i| *i < monitors.len())
        .or_else(|| home_monitor_index(visible, monitors))?;
    Some(visible.clamped_into(monitors[owner].work_area))
}

/// The `(dx, dy)` that turns *workspace* coordinates (what `WINDOWPLACEMENT`
/// uses) into screen coordinates: the primary monitor's work-area origin
/// relative to its screen origin. Zero with a bottom or right taskbar; the
/// taskbar's thickness with a top or left one.
#[must_use]
pub(crate) fn workspace_offset(primary: &RescueMonitor) -> (i32, i32) {
    (
        primary.work_area.x - primary.screen_rect.x,
        primary.work_area.y - primary.screen_rect.y,
    )
}

/// Where a minimized window's normal (restore) position should go, or
/// `None` if it already lies on the virtual screen.
///
/// `normal` is the placement's `rcNormalPosition` in workspace coordinates
/// (see [`workspace_offset`]; `monitors[0]` is taken as the primary) and
/// includes the window's invisible borders, which `bounds` strips before
/// the overlap test. The result is again a normal-position rect in
/// workspace coordinates, clamped into the work area of the monitor that
/// owns the window: `owner` when the caller knows it (the daemon's
/// workspace bookkeeping), else the geometric [`home_monitor_index`].
#[must_use]
pub(crate) fn placement_target(
    normal: Rect,
    bounds: InvisibleBounds,
    monitors: &[RescueMonitor],
    owner: Option<usize>,
) -> Option<Rect> {
    let (dx, dy) = workspace_offset(monitors.first()?);
    let window_on_screen = Rect {
        x: normal.x + dx,
        y: normal.y + dy,
        ..normal
    };
    let visible = bounds.window_to_visible(window_on_screen);
    let target = rescue_target_on(visible, monitors, owner)?;
    let window = bounds.visible_to_window(target);
    Some(Rect {
        x: window.x - dx,
        y: window.y - dy,
        ..window
    })
}

/// Re-anchor one minimized window whose restore position is off-screen.
///
/// Reads its `WINDOWPLACEMENT`, asks `placement_target` whether the
/// normal position is stranded, and if so writes the clamped rect back with
/// `SetWindowPlacement` — `showCmd` untouched, so the window **stays
/// minimized** and simply restores on-screen later. `bounds` are the
/// window's invisible borders (the daemon has them recorded; the daemon-less
/// path passes zero, which only costs a few pixels of frame bleed) and
/// `owner` the monitor index the caller knows the window belongs to.
///
/// Returns `Ok(Some((from, to)))` when the placement was rewritten,
/// `Ok(None)` when it was already on-screen.
///
/// # Errors
///
/// Returns the Win32 error as a string if the placement cannot be read or
/// written.
pub(crate) fn rescue_minimized_placement(
    hwnd: HWND,
    bounds: InvisibleBounds,
    monitors: &[RescueMonitor],
    owner: Option<usize>,
) -> Result<Option<(Rect, Rect)>, String> {
    let placement = win32::get_window_placement(hwnd)?;
    let from = placement.normal_position;
    let Some(to) = placement_target(from, bounds, monitors, owner) else {
        return Ok(None);
    };
    let fixed = win32::WindowPlacement {
        normal_position: to,
        ..placement
    };
    win32::set_window_placement(hwnd, &fixed)?;
    Ok(Some((from, to)))
}

/// Enumerate the monitors as [`RescueMonitor`]s (primary first).
///
/// # Errors
///
/// Returns the enumeration error as a string.
pub(crate) fn current_monitors() -> Result<Vec<RescueMonitor>, String> {
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
/// visible, titled, Alt-Tab-visible, not owned, not a child. For each
/// non-minimized one, the visible-content rect is computed from
/// `GetWindowRect` and the window's invisible borders; `rescue_target`
/// decides whether and where to move it, and `SetWindowPos` (no
/// activation, z-order kept) does the move. Minimized candidates go
/// through `rescue_minimized_placement` instead: their restore position
/// is clamped on-screen and they stay minimized.
///
/// # Errors
///
/// Returns a string if monitors or windows cannot be enumerated.
pub fn rescue_offscreen_windows() -> Result<RescueReport, String> {
    let monitors = current_monitors()?;
    let hwnds = enum_toplevel_windows()?;
    let mut report = RescueReport::default();
    for hwnd in hwnds {
        if !win32::is_window_visible(hwnd) {
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
        if win32::is_iconic(hwnd) {
            // The iconic rect says nothing; the placement's normal position
            // is what would strand it on restore. Invisible borders cannot
            // be measured on a minimized window, so zero is assumed.
            match rescue_minimized_placement(hwnd, InvisibleBounds::zero(), &monitors, None) {
                Ok(Some((from, to))) => {
                    log::info!(
                        "rescue: re-anchored minimized {title:?} from ({},{}) to ({},{})",
                        from.x,
                        from.y,
                        to.x,
                        to.y
                    );
                    report.minimized.push(RescuedWindow { title, from, to });
                }
                Ok(None) => {}
                Err(e) => log::warn!("rescue: placement of minimized {title:?} not fixed: {e}"),
            }
            continue;
        }
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

    /// Laptop at (0,0) 1920x1080 under a 2560x1440 external display.
    fn stacked_pair() -> Vec<RescueMonitor> {
        vec![mon(0, 0, 1920, 1080), mon(0, -1440, 2560, 1440)]
    }

    #[test]
    fn stacked_pair_workspace_parked_beyond_the_virtual_screen_is_rescued() {
        let m = stacked_pair();
        assert_eq!(virtual_screen_of(&m), Some(r(0, -1440, 2560, 2520)));
        // A laptop window (y 16..1016) in a workspace parked above the pair:
        // the far slot puts its bottom at -1440 - 16 (PARK_MARGIN). It is
        // off the virtual screen, so it is rescued. The laptop's x range is
        // inside the external's, so both monitors tie on band overlap and
        // the nearest one wins — the external, which lies between the slot
        // and the laptop. Without workspace bookkeeping (the daemon's own
        // shutdown path has it, this CLI path does not) that is the best
        // guess; what matters is that the window lands on a monitor.
        let above = r(300, 16 - 1440 - 16 - 1080, 800, 1000);
        assert!(above.bottom() <= -1440);
        assert_eq!(home_monitor_index(above, &m), Some(1));
        let target = rescue_target(above, &m).expect("far-parked window is rescued");
        assert_eq!(target, r(300, -1440, 800, 1000));
        assert!(target.overlaps(m[1].work_area));
        // An external window in a workspace parked below the pair.
        let below = r(2000, 1080 + 16 + 100, 500, 400);
        assert_eq!(home_monitor_index(below, &m), Some(1));
        // Clamped into the external's work area (bottom 1440-40 = -40).
        assert_eq!(rescue_target(below, &m), Some(r(2000, -40 - 400, 500, 400)));
    }

    #[test]
    fn stacked_pair_window_one_height_up_is_on_the_external_and_left_alone() {
        // Why the far slot exists: the old one-height slot for a laptop
        // workspace overlaps the external display, so the rescue (rightly)
        // sees it as visible and does nothing.
        let m = stacked_pair();
        let old_slot = r(300, 16 - 1084, 800, 1000);
        assert_eq!(rescue_target(old_slot, &m), None);
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
        assert_eq!(
            placement_target(r(-5000, 0, 100, 100), InvisibleBounds::zero(), &[], None),
            None
        );
    }

    #[test]
    fn known_owner_beats_the_geometric_guess() {
        let m = two_monitors();
        // Parked left of monitor 0, but the daemon says it belongs to 1.
        let parked = r(-960 - 16, 0, 960, 600);
        assert_eq!(
            rescue_target_on(parked, &m, Some(1)),
            Some(r(1920, 0, 960, 600))
        );
        // An out-of-range owner falls back to the guess.
        assert_eq!(
            rescue_target_on(parked, &m, Some(7)),
            Some(r(0, 0, 960, 600))
        );
    }

    // ── Minimized-window placement clamp ──────────────────────────────

    const BOUNDS: InvisibleBounds = InvisibleBounds {
        left: 7,
        top: 0,
        right: 7,
        bottom: 7,
    };

    #[test]
    fn minimized_placement_parked_right_is_clamped_into_the_home_work_area() {
        let m = two_monitors();
        // Normal position of a window whose column was parked right of
        // monitor 1 (visible x = 4480 + 16), frame included.
        let normal = BOUNDS.visible_to_window(r(4480 + 16, 100, 960, 600));
        let fixed = placement_target(normal, BOUNDS, &m, None).expect("stranded");
        // Visible content now ends flush at the monitor's right edge.
        assert_eq!(
            BOUNDS.window_to_visible(fixed),
            r(4480 - 960, 100, 960, 600)
        );
        // Writing the same rect again is a no-op: it is on-screen now.
        assert_eq!(placement_target(fixed, BOUNDS, &m, None), None);
    }

    #[test]
    fn minimized_placement_on_screen_is_left_alone() {
        let m = two_monitors();
        assert_eq!(
            placement_target(r(100, 100, 800, 600), BOUNDS, &m, None),
            None
        );
        // Frame bleed of a window flush at the edge counts as off-screen
        // once the borders are stripped (touching is not overlapping).
        let flush = BOUNDS.visible_to_window(r(-960, 0, 960, 600));
        assert!(placement_target(flush, BOUNDS, &m, None).is_some());
        // ... but with zero bounds (the daemon-less path) the same frame
        // bleeds 7 px into the screen and is treated as visible, which is
        // why parked windows carry PARK_MARGIN beyond the edge.
        assert_eq!(
            placement_target(flush, InvisibleBounds::zero(), &m, None),
            None
        );
        let parked = r(-960 - 16, 0, 960, 600);
        assert_eq!(
            placement_target(parked, InvisibleBounds::zero(), &m, None),
            Some(r(0, 0, 960, 600))
        );
    }

    #[test]
    fn minimized_placement_honours_the_workspace_offset_of_a_top_taskbar() {
        // 40 px taskbar at the TOP of the primary: workspace coordinates
        // are screen coordinates shifted up by 40.
        let primary = RescueMonitor {
            screen_rect: r(0, 0, 1920, 1080),
            work_area: r(0, 40, 1920, 1040),
        };
        assert_eq!(workspace_offset(&primary), (0, 40));
        let m = vec![primary];
        // Normal position (workspace coords) of a window parked a full
        // monitor height above: screen y = -1040 - 16 - 600 + 40.
        let normal = r(100, -1040 - 16 - 600, 800, 600);
        let fixed = placement_target(normal, InvisibleBounds::zero(), &m, None).expect("stranded");
        // Clamped to the work area's top edge (screen y = 40) → workspace y = 0.
        assert_eq!(fixed, r(100, 0, 800, 600));
        // Round trip: the fixed rect maps to screen (100, 40), inside the work area.
        assert!(r(fixed.x, fixed.y + 40, 800, 600).overlaps(primary.work_area));
    }

    #[test]
    fn minimized_placement_uses_the_known_owner() {
        let m = two_monitors();
        let normal = r(-960 - 16, 0, 960, 600);
        assert_eq!(
            placement_target(normal, InvisibleBounds::zero(), &m, Some(1)),
            Some(r(1920, 0, 960, 600))
        );
    }
}
