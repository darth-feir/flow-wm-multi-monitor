//! Vertical parking offset for workspace stacking.
//!
//! Workspaces are stacked **vertically** the same way columns are stacked
//! horizontally inside a [`ScrollingSpace`](super::ScrollingSpace): the active
//! workspace sits at `y = 0`, previous workspaces (id < active) park above at
//! `-y_unit`, and next workspaces (id > active) park below at `+y_unit`, where
//! `y_unit = monitor_height + window_gap`. Here `monitor_height` is the full
//! **physical** screen height (taskbar included), never the work area — see
//! the `monitor_height` parameter docs for why.
//!
//! # Vertical neighbours: the far parking slot
//!
//! `±y_unit` keeps a parked workspace off its *own* monitor; when another
//! monitor sits directly above or below, [`WorkspaceParking`] parks the
//! workspace **beyond the Win32 virtual screen** on that side instead,
//! mirroring the horizontal column rule, and the animation bridge moves it
//! instantly. The rule table is in
//! `docs/src/dev-guide/layout/projection.md`, *Neighbours on four sides*.
//!
//! # The ±1-Unit Model
//!
//! The offset is **always exactly one unit**, regardless of how far apart the
//! workspace ids are. Switching from workspace 2 to 3 moves windows by the
//! same distance as switching from 2 to 8. This is a deliberate design choice:
//!
//! - **Visual consistency** — every workspace switch looks identical, so the
//!   user can anticipate the animation duration and direction without surprises.
//! - **Simplicity** — the offset math is a single sign check, not a scaling
//!   calculation.
//! - **Animation cost** — a fixed distance means a fixed-duration animation,
//!   independent of which workspaces are involved.
//!
//! # The Workspace Stacking Invariant
//!
//! At any moment, for the active workspace `A`:
//!
//! | Workspace id | Parked at | Reason                                     |
//! |--------------|-----------|--------------------------------------------|
//! | `< A`        | `-y_unit` | Above the active workspace (parked).       |
//! | `= A`        | `0`       | The active workspace — on screen.          |
//! | `> A`        | `+y_unit` | Below the active workspace (parked).       |
//!
//! When switching workspaces, every non-empty workspace is submitted to the
//! animator in a single batch. The **source** and **destination** animate
//! into their new slots. Bystanders whose parking *side* changed (e.g. ws 3-7
//! when switching 2 → 8 — they were below active 2, but become above active
//! 8) are **teleported** first (direct `SetWindowPos`, before the animator
//! runs) so the animator sees them already at target and moves them as a
//! no-op; same-side bystanders are already at rest and also no-op. Any window
//! still in flight from a previous, interrupted switch is retargeted by the
//! animator reading its interpolated position — this is what prevents rapid
//! switches from stranding bystanders mid-screen.
//!
//! # Where this lives
//!
//! This module is pure math — no Win32, no IPC. Its offsets are applied in
//! the daemon animation bridge ([`crate::daemon::animation`]) when building
//! `WindowTarget` batches, **not** in the layout projection layer. The
//! projection layer (`layout::projection`) stays horizontal-only and
//! workspace-local; the daemon decides where each workspace sits vertically.

use crate::common::Rect;
use crate::layout::projection::PARK_MARGIN;
use crate::layout::types::MonitorInfo;
use crate::workspace::WorkspaceId;

/// Compute the **free-side** vertical parking offset for a workspace relative
/// to the active one.
///
/// Returns `±(monitor_height + window_gap)` based on whether `target_id` sits
/// below (`+`) or above (`-`) the `active_id`. Returns `0` when the two ids
/// are equal (i.e. the active workspace itself).
///
/// This is the one-monitor-height slot that keeps a parked workspace off its
/// own monitor. It is correct only when no other monitor sits on that side;
/// the daemon goes through [`WorkspaceParking::offset`], which falls back to
/// this function on a free side and parks beyond the virtual screen on a
/// side with a neighbour.
///
/// # Arguments
///
/// * `target_id` — The workspace whose parking offset is being computed.
/// * `active_id` — The currently active workspace.
/// * `monitor_height` — The full **physical** monitor height, in pixels (NOT
///   the taskbar-excluded work area). Parked workspaces must travel the full
///   physical height to stay completely off-screen; using the work area would
///   leave a slice the height of the taskbar/topbar visible.
/// * `window_gap` — The configured gap, in pixels. The same value is used
///   both intra-workspace (between adjacent windows) and inter-workspace
///   (between stacked workspaces), so the stacking looks visually consistent
///   with the in-workspace tiling.
///
/// # Examples
///
/// ```
/// # use flow_wm::workspace::WorkspaceId;
/// # use flow_wm::workspace::workspace_y_offset;
/// // Workspace 3 sits above active workspace 5.
/// let off = workspace_y_offset(WorkspaceId(3), WorkspaceId(5), 1080, 4);
/// assert_eq!(off, -(1080 + 4));
///
/// // Workspace 7 sits below active workspace 5.
/// let off = workspace_y_offset(WorkspaceId(7), WorkspaceId(5), 1080, 4);
/// assert_eq!(off, 1080 + 4);
///
/// // The active workspace itself sits at y = 0.
/// let off = workspace_y_offset(WorkspaceId(5), WorkspaceId(5), 1080, 4);
/// assert_eq!(off, 0);
/// ```
#[must_use]
pub fn workspace_y_offset(
    target_id: WorkspaceId,
    active_id: WorkspaceId,
    monitor_height: i32,
    window_gap: i32,
) -> i32 {
    // Use i64 for the subtraction so that the common case (target much
    // smaller than active) does not underflow a u32 before signum is taken.
    // The widening is free and self-documents the intent.
    let id_diff = i64::from(target_id.0) - i64::from(active_id.0);
    // signum returns -1, 0, or +1 — exactly the direction scalar we want,
    // without scaling by the (arbitrary) numeric distance between ids.
    let direction = id_diff.signum() as i32;
    direction * (monitor_height + window_gap)
}

/// The geometry the vertical workspace-parking rule needs for one monitor.
///
/// Built once per switch or seating pass from the monitor's
/// [`MonitorInfo`] (virtual screen and vertical neighbours), its full
/// physical screen rect and the configured gap, then asked for each
/// workspace's offset via [`offset`](Self::offset): the active workspace is
/// at `0`; a side without a neighbour uses the one-monitor-height slot
/// (`±(screen_rect.height + window_gap)`), a side with one parks the strip
/// `PARK_MARGIN` beyond the virtual screen. The rule table and why the
/// blocked-side slot is never closer than the free-side one are in
/// `docs/src/dev-guide/layout/projection.md`, *Neighbours on four sides*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceParking {
    /// Full physical screen rect of the monitor (taskbar included).
    pub screen_rect: Rect,
    /// Bounding box of every monitor (the Win32 virtual screen).
    pub virtual_screen: Rect,
    /// Another monitor sits above this one (see [`MonitorInfo::neighbour_up`]).
    pub neighbour_up: bool,
    /// Another monitor sits below this one (see [`MonitorInfo::neighbour_down`]).
    pub neighbour_down: bool,
    /// The configured window gap, used as the free-side stacking gap.
    pub window_gap: i32,
}

impl WorkspaceParking {
    /// Parking geometry for a monitor described by `monitor` (virtual screen
    /// and neighbours), whose full physical bounds are `screen_rect`.
    ///
    /// `screen_rect` is taken separately because [`MonitorInfo`] carries the
    /// taskbar-excluded work area, and parked workspaces must travel the full
    /// physical height (see [`workspace_y_offset`]).
    #[must_use]
    pub fn new(monitor: &MonitorInfo, screen_rect: Rect, window_gap: i32) -> Self {
        Self {
            screen_rect,
            virtual_screen: monitor.virtual_screen,
            neighbour_up: monitor.neighbour_up,
            neighbour_down: monitor.neighbour_down,
            window_gap,
        }
    }

    /// Parking geometry for a lone monitor with no neighbours: both sides
    /// are free, so this reproduces [`workspace_y_offset`] exactly.
    #[must_use]
    pub fn single(screen_rect: Rect, window_gap: i32) -> Self {
        Self {
            screen_rect,
            virtual_screen: screen_rect,
            neighbour_up: false,
            neighbour_down: false,
            window_gap,
        }
    }

    /// The vertical offset at which workspace `target_id` parks while
    /// `active_id` is the active workspace. See the type-level docs for the
    /// rule.
    ///
    /// # Examples
    ///
    /// ```
    /// # use flow_wm::common::Rect;
    /// # use flow_wm::layout::projection::PARK_MARGIN;
    /// # use flow_wm::layout::types::MonitorInfo;
    /// # use flow_wm::workspace::{WorkspaceId, WorkspaceParking};
    /// // A laptop panel at (0,0) with a 1440p display stacked above it.
    /// let laptop = Rect { x: 0, y: 0, width: 1920, height: 1080 };
    /// let external = Rect { x: 0, y: -1440, width: 2560, height: 1440 };
    /// let vs = Rect { x: 0, y: -1440, width: 2560, height: 2520 };
    /// let info = MonitorInfo::in_desktop(laptop, laptop, vs, &[external]);
    /// let parking = WorkspaceParking::new(&info, laptop, 4);
    ///
    /// // Above is blocked: the strip's bottom sits PARK_MARGIN above the
    /// // virtual screen, i.e. above the external display.
    /// let above = parking.offset(WorkspaceId(1), WorkspaceId(2));
    /// assert_eq!(above, -1440 - PARK_MARGIN - 1080);
    ///
    /// // Below is free: the usual one-height slide.
    /// let below = parking.offset(WorkspaceId(3), WorkspaceId(2));
    /// assert_eq!(below, 1080 + 4);
    /// ```
    #[must_use]
    pub fn offset(&self, target_id: WorkspaceId, active_id: WorkspaceId) -> i32 {
        let free = workspace_y_offset(
            target_id,
            active_id,
            self.screen_rect.height,
            self.window_gap,
        );
        if free < 0 && self.neighbour_up {
            self.virtual_screen.y - PARK_MARGIN - self.screen_rect.bottom()
        } else if free > 0 && self.neighbour_down {
            self.virtual_screen.bottom() + PARK_MARGIN - self.screen_rect.y
        } else {
            free
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typical test monitor: 1920×1080 work area, 4px gap.
    const MONITOR_H: i32 = 1080;
    const GAP: i32 = 4;
    /// The parking unit for a 1080-tall monitor with a 4px gap.
    const Y_UNIT: i32 = MONITOR_H + GAP; // 1084

    // ---------------------------------------------------------------------
    // Sign / direction
    // ---------------------------------------------------------------------

    #[test]
    fn offset_for_previous_workspace_is_negative() {
        // Workspace 3 relative to active 5 → above (negative offset).
        let off = workspace_y_offset(WorkspaceId(3), WorkspaceId(5), MONITOR_H, GAP);
        assert_eq!(off, -Y_UNIT);
    }

    #[test]
    fn offset_for_next_workspace_is_positive() {
        // Workspace 7 relative to active 5 → below (positive offset).
        let off = workspace_y_offset(WorkspaceId(7), WorkspaceId(5), MONITOR_H, GAP);
        assert_eq!(off, Y_UNIT);
    }

    #[test]
    fn offset_for_active_workspace_is_zero() {
        // The active workspace itself sits at y = 0 by definition.
        let off = workspace_y_offset(WorkspaceId(5), WorkspaceId(5), MONITOR_H, GAP);
        assert_eq!(off, 0);
    }

    // ---------------------------------------------------------------------
    // ±1-unit invariant (the core design contract)
    // ---------------------------------------------------------------------

    #[test]
    fn offset_does_not_scale_with_id_distance() {
        // The ±1 unit model: the offset for workspace 2 is the SAME regardless
        // of whether the new active is 3, 4, or 8. Switching 2→3, 2→4, and 2→8
        // must all leave ws 2 at -Y_UNIT (it is previous to all of them).
        let off_to_3 = workspace_y_offset(WorkspaceId(2), WorkspaceId(3), MONITOR_H, GAP);
        let off_to_4 = workspace_y_offset(WorkspaceId(2), WorkspaceId(4), MONITOR_H, GAP);
        let off_to_8 = workspace_y_offset(WorkspaceId(2), WorkspaceId(8), MONITOR_H, GAP);
        assert_eq!(off_to_3, -Y_UNIT);
        assert_eq!(off_to_4, -Y_UNIT);
        assert_eq!(off_to_8, -Y_UNIT);
    }

    #[test]
    fn offset_independent_of_direction_of_switch() {
        // Switching 5→6 vs 6→5 must produce symmetric offsets for the same
        // workspace. Ws 5 relative to active 6 → -Y_UNIT. Ws 6 relative to
        // active 5 → +Y_UNIT. Equal magnitude, opposite sign.
        let ws5_below_6 = workspace_y_offset(WorkspaceId(5), WorkspaceId(6), MONITOR_H, GAP);
        let ws6_above_5 = workspace_y_offset(WorkspaceId(6), WorkspaceId(5), MONITOR_H, GAP);
        assert_eq!(ws5_below_6, -Y_UNIT);
        assert_eq!(ws6_above_5, Y_UNIT);
        assert_eq!(ws5_below_6.abs(), ws6_above_5.abs());
    }

    // ---------------------------------------------------------------------
    // Edge cases
    // ---------------------------------------------------------------------

    #[test]
    fn offset_smallest_id_above_active_two() {
        // Workspace 1 (the smallest id in the typical 1..=10 range) is just
        // above active workspace 2.
        let off = workspace_y_offset(WorkspaceId(1), WorkspaceId(2), MONITOR_H, GAP);
        assert_eq!(off, -Y_UNIT);
    }

    #[test]
    fn offset_handles_zero_gap() {
        // A zero gap degenerates y_unit to exactly monitor_height.
        let off = workspace_y_offset(WorkspaceId(2), WorkspaceId(1), 1080, 0);
        assert_eq!(off, 1080);
    }

    #[test]
    fn offset_same_id_returns_zero_regardless_of_inputs() {
        // The active-workspace offset is always zero, independent of monitor
        // size or gap. This guards against future refactors accidentally
        // returning a non-zero "self offset".
        assert_eq!(workspace_y_offset(WorkspaceId(1), WorkspaceId(1), 0, 0), 0);
        assert_eq!(
            workspace_y_offset(WorkspaceId(1), WorkspaceId(1), 9999, 999),
            0
        );
    }

    #[test]
    fn offset_does_not_underflow_when_target_much_smaller() {
        // Regression: the subtraction `target.0 - active.0` must not underflow
        // when target is much smaller than active. Use the extreme pair (1, 10).
        let off = workspace_y_offset(WorkspaceId(1), WorkspaceId(10), MONITOR_H, GAP);
        assert_eq!(off, -Y_UNIT);
    }

    #[test]
    fn offset_does_not_overflow_when_target_much_larger() {
        // Symmetric to the underflow test: target much larger than active.
        let off = workspace_y_offset(WorkspaceId(10), WorkspaceId(1), MONITOR_H, GAP);
        assert_eq!(off, Y_UNIT);
    }

    // ---------------------------------------------------------------------
    // WorkspaceParking: free side vs blocked side
    // ---------------------------------------------------------------------

    /// Laptop panel at the origin; the external display (if any) is placed
    /// by the caller.
    const LAPTOP: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: MONITOR_H,
    };

    fn parking_with(others: &[Rect]) -> WorkspaceParking {
        let mut vs = LAPTOP;
        for o in others {
            let x0 = vs.x.min(o.x);
            let y0 = vs.y.min(o.y);
            let x1 = vs.right().max(o.right());
            let y1 = vs.bottom().max(o.bottom());
            vs = Rect {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            };
        }
        let info = MonitorInfo::in_desktop(LAPTOP, LAPTOP, vs, others);
        WorkspaceParking::new(&info, LAPTOP, GAP)
    }

    #[test]
    fn single_monitor_parking_matches_the_free_side_function() {
        let p = WorkspaceParking::single(LAPTOP, GAP);
        assert_eq!(p.offset(WorkspaceId(1), WorkspaceId(2)), -Y_UNIT);
        assert_eq!(p.offset(WorkspaceId(2), WorkspaceId(2)), 0);
        assert_eq!(p.offset(WorkspaceId(3), WorkspaceId(2)), Y_UNIT);
        assert_eq!(p, parking_with(&[]));
    }

    #[test]
    fn side_by_side_neighbour_leaves_vertical_parking_alone() {
        // A monitor to the right blocks nothing vertically: the strip still
        // slides one height up or down.
        let right = Rect {
            x: 1920,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let p = parking_with(&[right]);
        assert!(!p.neighbour_up && !p.neighbour_down);
        assert_eq!(p.offset(WorkspaceId(1), WorkspaceId(2)), -Y_UNIT);
        assert_eq!(p.offset(WorkspaceId(3), WorkspaceId(2)), Y_UNIT);
    }

    #[test]
    fn neighbour_above_parks_beyond_the_virtual_screen_but_below_still_slides() {
        // External 2560x1440 stacked above the laptop: virtual screen spans
        // y = -1440 .. 1080.
        let external = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        let p = parking_with(&[external]);
        assert!(p.neighbour_up && !p.neighbour_down);
        // Above: the strip's bottom edge lands at -1440 - PARK_MARGIN, so a
        // window spanning the laptop's full height (0..1080) ends at
        // (-1440 - 16 - 1080) .. (-1440 - 16): entirely above the external.
        let above = p.offset(WorkspaceId(1), WorkspaceId(2));
        assert_eq!(above, -1440 - PARK_MARGIN - MONITOR_H);
        assert!(LAPTOP.bottom() + above <= external.y);
        // The old one-height slot would have sat fully on the external.
        assert!(LAPTOP.y - Y_UNIT >= external.y);
        // Below is free: unchanged.
        assert_eq!(p.offset(WorkspaceId(3), WorkspaceId(2)), Y_UNIT);
        // Active stays put.
        assert_eq!(p.offset(WorkspaceId(2), WorkspaceId(2)), 0);
    }

    #[test]
    fn neighbour_below_parks_beyond_the_virtual_screen_but_above_still_slides() {
        let external = Rect {
            x: -320,
            y: MONITOR_H,
            width: 2560,
            height: 1440,
        };
        let p = parking_with(&[external]);
        assert!(!p.neighbour_up && p.neighbour_down);
        let below = p.offset(WorkspaceId(3), WorkspaceId(2));
        // Virtual screen bottom is 1080 + 1440 = 2520; the strip's top goes
        // PARK_MARGIN past it.
        assert_eq!(below, 2520 + PARK_MARGIN);
        assert!(LAPTOP.y + below >= external.bottom());
        assert_eq!(p.offset(WorkspaceId(1), WorkspaceId(2)), -Y_UNIT);
    }

    #[test]
    fn neighbours_on_both_sides_park_both_directions_far() {
        let up = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        let down = Rect {
            x: 0,
            y: MONITOR_H,
            width: 1920,
            height: 1080,
        };
        let p = parking_with(&[up, down]);
        assert!(p.neighbour_up && p.neighbour_down);
        assert_eq!(
            p.offset(WorkspaceId(1), WorkspaceId(5)),
            -1440 - PARK_MARGIN - MONITOR_H
        );
        assert_eq!(
            p.offset(WorkspaceId(9), WorkspaceId(5)),
            MONITOR_H + 1080 + PARK_MARGIN
        );
        // The ±1-unit model survives: every workspace on a side shares its slot.
        assert_eq!(
            p.offset(WorkspaceId(1), WorkspaceId(5)),
            p.offset(WorkspaceId(4), WorkspaceId(5))
        );
    }

    #[test]
    fn blocked_slot_is_never_closer_than_the_free_slot() {
        let up = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        let p = parking_with(&[up]);
        let blocked = p.offset(WorkspaceId(1), WorkspaceId(2));
        assert!(blocked.abs() > Y_UNIT);
    }

    #[test]
    fn diagonal_monitor_is_not_a_vertical_neighbour() {
        // A display above-right that shares no x-range: nothing parked one
        // height up can land on it, so the free-side slot is kept.
        let diag = Rect {
            x: 1920,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        let p = parking_with(&[diag]);
        assert!(!p.neighbour_up && !p.neighbour_down);
        assert_eq!(p.offset(WorkspaceId(1), WorkspaceId(2)), -Y_UNIT);
    }

    #[test]
    fn blocked_offset_uses_the_physical_screen_rect_not_the_work_area() {
        // Work area is 40px shorter than the screen (bottom taskbar). The
        // blocked-side offset must shift the strip by the physical bottom,
        // otherwise the taskbar strip of the workspace would peek onto the
        // display above.
        let work_area = Rect {
            height: MONITOR_H - 40,
            ..LAPTOP
        };
        let external = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 1440,
        };
        let vs = Rect {
            x: 0,
            y: -1440,
            width: 2560,
            height: 2520,
        };
        let info = MonitorInfo::in_desktop(work_area, LAPTOP, vs, &[external]);
        let p = WorkspaceParking::new(&info, LAPTOP, GAP);
        assert_eq!(
            p.offset(WorkspaceId(1), WorkspaceId(2)),
            external.y - PARK_MARGIN - LAPTOP.bottom()
        );
    }

    #[test]
    fn offset_at_u32_extremes_uses_i64_widening_correctly() {
        // Edge: the function body widens both ids to `i64` before subtracting
        // (see the `i64::from(...)` casts) precisely so that a large `target`
        // minus a small `active` (or vice versa) cannot underflow a `u32` and
        // flip the sign of `signum`. The under/overflow tests above only use
        // the small pair (1, 10); this test exercises the same protection at
        // the actual `u32` type boundary, where a naive `u32` subtraction
        // would wrap to a huge positive and produce the wrong direction.
        //
        // Arrange: the extreme id pairs. Act/Assert: sign is still correct.
        // target >> active → parks below → +Y_UNIT.
        assert_eq!(
            workspace_y_offset(WorkspaceId(u32::MAX), WorkspaceId(1), MONITOR_H, GAP),
            Y_UNIT,
        );
        // target << active → parks above → -Y_UNIT.
        assert_eq!(
            workspace_y_offset(WorkspaceId(1), WorkspaceId(u32::MAX), MONITOR_H, GAP),
            -Y_UNIT,
        );
        // Equal extremes → active workspace → 0, independent of magnitude.
        assert_eq!(
            workspace_y_offset(WorkspaceId(u32::MAX), WorkspaceId(u32::MAX), MONITOR_H, GAP),
            0,
        );
    }
}
