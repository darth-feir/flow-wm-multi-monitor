//! Riders follow their anchor (ADR-0038).
//!
//! A rider (an untracked window owned by a flow window that took the
//! foreground, `daemon::top_layer`, ADR-0024) moves with its anchor: when
//! the daemon places the anchor somewhere else — a layout submit after a
//! scroll, a resize or a move to another monitor, a workspace switch or its
//! teleported bystanders, the hotplug re-seat, `tiling off`, the shutdown
//! rescue — each of its riders is moved by the same delta, keeping its
//! size. A submit adds the riders to the same animator batch, so they slide
//! with the anchor frame by frame; a teleport moves them right after it. A
//! drag of the anchor carries them along on every move of the gesture.
//!
//! Nothing is moved while the rider or its anchor does not respond
//! (`IsHungAppWindow`, ADR-0035): a synchronous `SetWindowPos` would wait on
//! that thread. A hidden rider (its anchor minimized) is left where it is;
//! Windows shows it again with its owner.

use windows::Win32::Foundation::HWND;

use crate::animation::{IVec2, WindowRef, WindowTarget};
use crate::common::Rect;
use crate::registry::win32 as registry_win32;

use super::top_layer::Rider;
use super::types::FlowWM;

/// Where the last submit sent an anchor and its rider (window rects'
/// top-left corners).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Follow {
    /// The anchor's commanded position.
    pub anchor_to: (i32, i32),
    /// The rider's commanded position.
    pub rider_to: (i32, i32),
}

/// What happens to a rider when its anchor moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FollowVerdict {
    /// Moved by the anchor's delta to `(x, y)`, size kept.
    Move { x: i32, y: i32 },
    /// The anchor does not move: nothing to do.
    Still,
    /// The rider is not shown (hidden with a minimized anchor): left alone.
    Hidden,
    /// The rider (`anchor: false`) or its anchor (`anchor: true`) does not
    /// respond: left where it is.
    NotResponding { anchor: bool },
}

/// Pure decision rule (ADR-0038): the anchor goes from `anchor_from` to
/// `anchor_to`; the rider, now at `rider_from`, follows by the same delta
/// unless it is not shown or either window does not respond.
#[must_use]
pub(super) fn follow_verdict(
    anchor_from: (i32, i32),
    anchor_to: (i32, i32),
    rider_from: (i32, i32),
    rider_shown: bool,
    rider_hung: bool,
    anchor_hung: bool,
) -> FollowVerdict {
    let (dx, dy) = (anchor_to.0 - anchor_from.0, anchor_to.1 - anchor_from.1);
    if dx == 0 && dy == 0 {
        FollowVerdict::Still
    } else if !rider_shown {
        FollowVerdict::Hidden
    } else if anchor_hung {
        FollowVerdict::NotResponding { anchor: true }
    } else if rider_hung {
        FollowVerdict::NotResponding { anchor: false }
    } else {
        FollowVerdict::Move {
            x: rider_from.0 + dx,
            y: rider_from.1 + dy,
        }
    }
}

/// The positions a move of the anchor to a new place starts from: while a
/// batch is in flight and the anchor has not reached the place the last
/// submit sent it (`follow`), the pair that submit commanded (the rider is
/// on its way to its own); otherwise where both windows are now.
#[must_use]
pub(super) fn follow_base(
    follow: Option<Follow>,
    animating: bool,
    anchor_now: (i32, i32),
    rider_now: (i32, i32),
) -> ((i32, i32), (i32, i32)) {
    match follow {
        Some(f) if animating && anchor_now != f.anchor_to => (f.anchor_to, f.rider_to),
        _ => (anchor_now, rider_now),
    }
}

/// The Win32 questions the following asks, behind a trait for the tests.
pub(super) trait FollowProbe {
    /// The window rect (`GetWindowRect`); `None` when the handle is gone.
    fn rect(&self, hwnd: isize) -> Option<Rect>;
    /// Visible and not minimized.
    fn shown(&self, hwnd: isize) -> bool;
    /// `IsHungAppWindow` ([`registry_win32::is_hung`]).
    fn is_hung(&self, hwnd: isize) -> bool;
    /// Move keeping size, z-order and activation
    /// ([`registry_win32::move_window_keep_size`]).
    fn move_to(&self, hwnd: isize, x: i32, y: i32) -> bool;
}

/// The production [`FollowProbe`].
pub(super) struct Win32Follow;

impl FollowProbe for Win32Follow {
    fn rect(&self, hwnd: isize) -> Option<Rect> {
        registry_win32::get_window_rect(HWND(hwnd as *mut _)).ok()
    }

    fn shown(&self, hwnd: isize) -> bool {
        let h = HWND(hwnd as *mut _);
        registry_win32::is_window_visible(h) && !registry_win32::is_iconic(h)
    }

    fn is_hung(&self, hwnd: isize) -> bool {
        registry_win32::is_hung(hwnd)
    }

    fn move_to(&self, hwnd: isize, x: i32, y: i32) -> bool {
        registry_win32::move_window_keep_size(hwnd, x, y)
    }
}

fn pos(r: Rect) -> (i32, i32) {
    (r.x, r.y)
}

/// Log a rider left where it is.
fn log_held(hwnd: isize, anchor: isize, verdict: FollowVerdict) {
    match verdict {
        FollowVerdict::Hidden => {
            log::debug!("riders: {hwnd:#x} (anchor {anchor:#x}) is not shown; not moved");
        }
        FollowVerdict::NotResponding { anchor: a } => log::debug!(
            "riders: {hwnd:#x} (anchor {anchor:#x}) not moved: {} is not responding",
            if a { "the anchor" } else { "the rider" }
        ),
        FollowVerdict::Move { .. } | FollowVerdict::Still => {}
    }
}

impl FlowWM {
    /// The riders whose anchor is `anchor`, in handle order.
    fn riders_of(&self, anchor: isize) -> Vec<(isize, Rider)> {
        let mut out: Vec<(isize, Rider)> = self
            .riders
            .iter()
            .filter(|(_, r)| r.anchor == anchor)
            .map(|(h, r)| (*h, *r))
            .collect();
        out.sort_unstable_by_key(|(h, _)| *h);
        out
    }

    /// The animator targets that move every rider with its anchor, for the
    /// anchors in `targets` (window-rect space, as the animator takes
    /// them). Each rider's target keeps its size and the anchor's
    /// `instant` flag. `animating`: a batch is in flight right now.
    pub(super) fn rider_targets_with<P: FollowProbe>(
        &mut self,
        targets: &[WindowTarget],
        animating: bool,
        probe: &P,
    ) -> Vec<WindowTarget> {
        let mut out = Vec::new();
        if self.riders.is_empty() {
            return out;
        }
        let mut riders: Vec<(isize, Rider)> = self.riders.iter().map(|(h, r)| (*h, *r)).collect();
        riders.sort_unstable_by_key(|(h, _)| *h);
        for (hwnd, rider) in riders {
            let Some(target) = targets.iter().find(|t| t.window_ref.0 == rider.anchor) else {
                continue;
            };
            let (Some(anchor_now), Some(rider_now)) = (probe.rect(rider.anchor), probe.rect(hwnd))
            else {
                continue;
            };
            let to = (target.final_position.x, target.final_position.y);
            let (anchor_from, rider_from) =
                follow_base(rider.follow, animating, pos(anchor_now), pos(rider_now));
            let verdict = follow_verdict(
                anchor_from,
                to,
                rider_from,
                probe.shown(hwnd),
                probe.is_hung(hwnd),
                probe.is_hung(rider.anchor),
            );
            let FollowVerdict::Move { x, y } = verdict else {
                log_held(hwnd, rider.anchor, verdict);
                continue;
            };
            out.push(WindowTarget {
                window_ref: WindowRef(hwnd),
                final_position: IVec2::new(x, y),
                final_size: IVec2::new(rider_now.width, rider_now.height),
                instant: target.instant,
            });
            if let Some(r) = self.riders.get_mut(&hwnd) {
                r.follow = Some(Follow {
                    anchor_to: to,
                    rider_to: (x, y),
                });
            }
            log::debug!(
                "riders: {hwnd:#x} follows anchor {:#x} by ({},{}) to ({x},{y}){}",
                rider.anchor,
                to.0 - anchor_from.0,
                to.1 - anchor_from.1,
                if target.instant { " instantly" } else { "" }
            );
        }
        out
    }

    /// Submit `targets` to the animator with every rider of an anchor in
    /// them following it ([`rider_targets_with`](Self::rider_targets_with)).
    /// Every daemon path that hands the animator a batch goes through here.
    ///
    /// # Errors
    ///
    /// The animator's error (an empty batch, a dead worker).
    pub(super) fn animate_with_riders(
        &mut self,
        mut targets: Vec<WindowTarget>,
    ) -> Result<(), crate::animation::AnimationError> {
        if !self.riders.is_empty() && !targets.is_empty() {
            let animating = self.animator.is_animating();
            let extra = self.rider_targets_with(&targets, animating, &Win32Follow);
            targets.extend(extra);
        }
        self.animator.animate(targets).map(|_| ())
    }

    /// Where `anchor` is now (window rect's top-left), when it has riders;
    /// read before a teleport so they can follow it
    /// ([`follow_anchor_now`](Self::follow_anchor_now)). `None` (no Win32)
    /// for a window without riders.
    #[must_use]
    pub(super) fn anchor_position_for_riders(&self, anchor: isize) -> Option<(i32, i32)> {
        if !self.riders.values().any(|r| r.anchor == anchor) {
            return None;
        }
        Win32Follow.rect(anchor).map(pos)
    }

    /// `anchor` was just moved from `from` outside the animator (a
    /// teleport): its riders follow by the same delta, now.
    pub(super) fn follow_anchor_now(&mut self, anchor: isize, from: (i32, i32)) {
        self.follow_anchor_now_with(anchor, from, &Win32Follow);
    }

    /// [`follow_anchor_now`](Self::follow_anchor_now) against `probe`.
    pub(super) fn follow_anchor_now_with<P: FollowProbe>(
        &mut self,
        anchor: isize,
        from: (i32, i32),
        probe: &P,
    ) {
        let Some(to) = probe.rect(anchor).map(pos) else {
            return;
        };
        for (hwnd, _) in self.riders_of(anchor) {
            let Some(rider_now) = probe.rect(hwnd) else {
                continue;
            };
            let verdict = follow_verdict(
                from,
                to,
                pos(rider_now),
                probe.shown(hwnd),
                probe.is_hung(hwnd),
                probe.is_hung(anchor),
            );
            if let FollowVerdict::Move { x, y } = verdict {
                let moved = probe.move_to(hwnd, x, y);
                log::debug!(
                    "riders: {hwnd:#x} follows anchor {anchor:#x} by ({},{}) to ({x},{y}) (teleport{})",
                    to.0 - from.0,
                    to.1 - from.1,
                    if moved { "" } else { ", refused" }
                );
            } else {
                log_held(hwnd, anchor, verdict);
            }
            if let Some(r) = self.riders.get_mut(&hwnd) {
                r.follow = None;
            }
        }
    }

    /// A drag of `anchor` begins: remember where each of its riders sits
    /// relative to it.
    pub(super) fn riders_drag_begin(&mut self, anchor: isize) {
        self.riders_drag_begin_with(anchor, &Win32Follow);
    }

    /// [`riders_drag_begin`](Self::riders_drag_begin) against `probe`.
    pub(super) fn riders_drag_begin_with<P: FollowProbe>(&mut self, anchor: isize, probe: &P) {
        let riders = self.riders_of(anchor);
        if riders.is_empty() {
            return;
        }
        let Some(a) = probe.rect(anchor) else {
            return;
        };
        for (hwnd, _) in riders {
            let offset = probe.rect(hwnd).map(|r| (r.x - a.x, r.y - a.y));
            if let Some(r) = self.riders.get_mut(&hwnd) {
                r.drag_offset = offset;
                r.follow = None;
            }
            log::debug!("riders: {hwnd:#x} rides the drag of {anchor:#x} (offset {offset:?})");
        }
    }

    /// The dragged `anchor` moved: its riders keep their offset to it.
    /// Cheap for a window without riders (one scan of a few entries).
    pub(super) fn riders_drag_move(&mut self, anchor: isize) {
        self.riders_drag_move_with(anchor, &Win32Follow);
    }

    /// [`riders_drag_move`](Self::riders_drag_move) against `probe`.
    pub(super) fn riders_drag_move_with<P: FollowProbe>(&mut self, anchor: isize, probe: &P) {
        let riders: Vec<(isize, (i32, i32))> = self
            .riders_of(anchor)
            .into_iter()
            .filter_map(|(h, r)| r.drag_offset.map(|o| (h, o)))
            .collect();
        if riders.is_empty() {
            return;
        }
        let Some(a) = probe.rect(anchor) else {
            return;
        };
        for (hwnd, (ox, oy)) in riders {
            let Some(now) = probe.rect(hwnd) else {
                continue;
            };
            let (x, y) = (a.x + ox, a.y + oy);
            let verdict = follow_verdict(
                (now.x - ox, now.y - oy),
                (a.x, a.y),
                (now.x, now.y),
                probe.shown(hwnd),
                probe.is_hung(hwnd),
                probe.is_hung(anchor),
            );
            match verdict {
                FollowVerdict::Move { .. } => {
                    if !probe.move_to(hwnd, x, y) {
                        log::debug!("riders: {hwnd:#x} could not follow the drag of {anchor:#x}");
                    }
                }
                FollowVerdict::Still => {}
                held => log_held(hwnd, anchor, held),
            }
        }
    }

    /// The drag of `anchor` ends: a last catch-up move, then the offsets
    /// are dropped. The drop's layout submit moves the riders on with it.
    pub(super) fn riders_drag_end(&mut self, anchor: isize) {
        self.riders_drag_end_with(anchor, &Win32Follow);
    }

    /// [`riders_drag_end`](Self::riders_drag_end) against `probe`.
    pub(super) fn riders_drag_end_with<P: FollowProbe>(&mut self, anchor: isize, probe: &P) {
        if !self
            .riders
            .values()
            .any(|r| r.anchor == anchor && r.drag_offset.is_some())
        {
            return;
        }
        self.riders_drag_move_with(anchor, probe);
        for r in self.riders.values_mut().filter(|r| r.anchor == anchor) {
            r.drag_offset = None;
        }
        log::debug!("riders: the drag of {anchor:#x} ended");
    }

    /// `anchor` was parked as not responding (ADR-0035): its riders are
    /// left where they are with it; logged so the trace says why they did
    /// not follow a later layout change.
    pub(super) fn note_anchor_hung(&self, anchor: isize) {
        for (hwnd, _) in self.riders_of(anchor) {
            log::debug!(
                "riders: {hwnd:#x} stays where it is: its anchor {anchor:#x} is not responding"
            );
        }
    }

    /// Each rider with its anchor's position now, before the shutdown
    /// rescue moves anchors back on screen (the rider table is cleared by
    /// the layer's release before the rescue runs).
    #[must_use]
    pub(super) fn riders_snapshot(&self) -> Vec<(isize, isize, (i32, i32))> {
        let mut out: Vec<(isize, isize, (i32, i32))> = self
            .riders
            .iter()
            .filter_map(|(h, r)| Win32Follow.rect(r.anchor).map(|a| (*h, r.anchor, pos(a))))
            .collect();
        out.sort_unstable();
        out
    }

    /// After the shutdown rescue: every rider of a snapshot whose anchor
    /// moved follows it, so no dialog is left parked off screen with the
    /// daemon gone. Returns how many moved.
    pub(super) fn follow_rescued_anchors(snapshot: &[(isize, isize, (i32, i32))]) -> usize {
        Self::follow_rescued_anchors_with(snapshot, &Win32Follow)
    }

    /// [`follow_rescued_anchors`](Self::follow_rescued_anchors) against
    /// `probe`.
    pub(super) fn follow_rescued_anchors_with<P: FollowProbe>(
        snapshot: &[(isize, isize, (i32, i32))],
        probe: &P,
    ) -> usize {
        let mut moved = 0;
        for &(hwnd, anchor, from) in snapshot {
            let (Some(to), Some(now)) = (probe.rect(anchor), probe.rect(hwnd)) else {
                continue;
            };
            let verdict = follow_verdict(
                from,
                pos(to),
                pos(now),
                probe.shown(hwnd),
                probe.is_hung(hwnd),
                probe.is_hung(anchor),
            );
            match verdict {
                FollowVerdict::Move { x, y } => {
                    if probe.move_to(hwnd, x, y) {
                        moved += 1;
                        log::info!(
                            "shutdown rescue: rider {hwnd:#x} followed its anchor {anchor:#x} to ({x},{y})"
                        );
                    }
                }
                FollowVerdict::Still => {}
                held => log_held(hwnd, anchor, held),
            }
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

    use super::*;

    #[test]
    fn the_verdict_moves_by_the_anchor_delta_unless_hidden_or_hung() {
        assert_eq!(
            follow_verdict((100, 100), (-1800, 100), (300, 250), true, false, false),
            FollowVerdict::Move { x: -1600, y: 250 }
        );
        assert_eq!(
            follow_verdict((100, 100), (100, -1184), (300, 250), true, false, false),
            FollowVerdict::Move { x: 300, y: -1034 }
        );
        assert_eq!(
            follow_verdict((5, 5), (5, 5), (300, 250), true, true, true),
            FollowVerdict::Still,
            "an anchor that does not move needs nothing, hung or not"
        );
        assert_eq!(
            follow_verdict((0, 0), (10, 0), (0, 0), false, false, false),
            FollowVerdict::Hidden
        );
        assert_eq!(
            follow_verdict((0, 0), (10, 0), (0, 0), true, true, true),
            FollowVerdict::NotResponding { anchor: true }
        );
        assert_eq!(
            follow_verdict((0, 0), (10, 0), (0, 0), true, true, false),
            FollowVerdict::NotResponding { anchor: false }
        );
    }

    #[test]
    fn the_base_is_the_last_commanded_pair_only_while_the_anchor_is_in_flight() {
        let f = Some(Follow {
            anchor_to: (500, 0),
            rider_to: (700, 100),
        });
        // In flight, not there yet: the commanded pair.
        assert_eq!(
            follow_base(f, true, (300, 0), (480, 100)),
            ((500, 0), (700, 100))
        );
        // Arrived, or nothing animating: where both are now.
        assert_eq!(
            follow_base(f, true, (500, 0), (650, 90)),
            ((500, 0), (650, 90))
        );
        assert_eq!(
            follow_base(f, false, (300, 0), (480, 100)),
            ((300, 0), (480, 100))
        );
        assert_eq!(
            follow_base(None, true, (300, 0), (480, 100)),
            ((300, 0), (480, 100))
        );
    }

    /// A scripted desktop for the following.
    #[derive(Default)]
    struct MockFollow {
        rects: RefCell<HashMap<isize, Rect>>,
        hidden: HashSet<isize>,
        hung: HashSet<isize>,
        moves: RefCell<Vec<(isize, i32, i32)>>,
    }

    impl MockFollow {
        fn with(self, hwnd: isize, x: i32, y: i32, w: i32, h: i32) -> Self {
            self.rects.borrow_mut().insert(
                hwnd,
                Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                },
            );
            self
        }

        fn place(&self, hwnd: isize, x: i32, y: i32) {
            if let Some(r) = self.rects.borrow_mut().get_mut(&hwnd) {
                r.x = x;
                r.y = y;
            }
        }
    }

    impl FollowProbe for MockFollow {
        fn rect(&self, hwnd: isize) -> Option<Rect> {
            self.rects.borrow().get(&hwnd).copied()
        }
        fn shown(&self, hwnd: isize) -> bool {
            !self.hidden.contains(&hwnd)
        }
        fn is_hung(&self, hwnd: isize) -> bool {
            self.hung.contains(&hwnd)
        }
        fn move_to(&self, hwnd: isize, x: i32, y: i32) -> bool {
            self.moves.borrow_mut().push((hwnd, x, y));
            self.place(hwnd, x, y);
            true
        }
    }

    const A: isize = 0xA;
    const R: isize = 0x21;
    const R2: isize = 0x22;
    const B: isize = 0xB;

    fn flow_with_riders() -> FlowWM {
        let rules = crate::config::types::WindowRulesConfig::default();
        let registry = crate::registry::WindowRegistry::new(&rules, &rules);
        let screen = crate::common::Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let info = crate::layout::types::MonitorInfo::single(screen);
        let ws = crate::workspace::Workspace::new(
            crate::workspace::WorkspaceId(1),
            crate::workspace::ScrollingSpace::new(
                info,
                936,
                320,
                100,
                100,
                crate::layout::types::Padding {
                    window_gap: 16,
                    up: 0,
                    down: 0,
                },
                4,
            ),
        );
        let m0 = crate::workspace::Monitor::new(screen, screen, vec![ws], 0);
        let mut flow = FlowWM::new_for_test(registry, vec![m0]);
        flow.riders.insert(R, Rider::new(A));
        flow.riders.insert(R2, Rider::new(A));
        flow
    }

    fn target(hwnd: isize, x: i32, y: i32, instant: bool) -> WindowTarget {
        WindowTarget {
            window_ref: WindowRef(hwnd),
            final_position: IVec2::new(x, y),
            final_size: IVec2::new(900, 1000),
            instant,
        }
    }

    #[test]
    fn a_submit_adds_each_rider_of_a_moved_anchor_with_its_size_and_the_instant_flag() {
        let mut flow = flow_with_riders();
        let probe = MockFollow::default()
            .with(A, 100, 40, 900, 1000)
            .with(R, 300, 200, 520, 720)
            .with(R2, 150, 600, 300, 200)
            .with(B, 1100, 40, 900, 1000);
        // A scrolls 600 px left; B is in the batch but has no riders.
        let t = vec![target(A, -500, 40, false), target(B, 500, 40, false)];
        let out = flow.rider_targets_with(&t, false, &probe);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].window_ref.0, R);
        assert_eq!(
            (out[0].final_position.x, out[0].final_position.y),
            (-300, 200)
        );
        assert_eq!((out[0].final_size.x, out[0].final_size.y), (520, 720));
        assert!(!out[0].instant);
        assert_eq!(
            (out[1].final_position.x, out[1].final_position.y),
            (-450, 600)
        );
        // A retarget while the first batch is in flight starts from the
        // commanded pair, not from where the windows are caught mid-slide.
        probe.place(A, -200, 40);
        probe.place(R, 0, 200);
        let t = vec![target(A, -1700, 40, true)];
        let out = flow.rider_targets_with(&t, true, &probe);
        assert_eq!(
            (out[0].final_position.x, out[0].final_position.y),
            (-1500, 200)
        );
        assert!(out[0].instant, "a teleported anchor teleports its riders");
        // An anchor that is not in the batch moves nobody.
        let out = flow.rider_targets_with(&[target(B, 0, 0, false)], false, &probe);
        assert!(out.is_empty());
    }

    #[test]
    fn a_hung_or_hidden_rider_or_a_hung_anchor_is_left_where_it_is() {
        let mut flow = flow_with_riders();
        let mut probe = MockFollow::default()
            .with(A, 100, 40, 900, 1000)
            .with(R, 300, 200, 520, 720)
            .with(R2, 150, 600, 300, 200);
        probe.hidden.insert(R2);
        probe.hung.insert(R);
        let t = vec![target(A, -500, 40, false)];
        assert!(flow.rider_targets_with(&t, false, &probe).is_empty());
        probe.hung.clear();
        probe.hung.insert(A);
        assert!(flow.rider_targets_with(&t, false, &probe).is_empty());
        flow.follow_anchor_now_with(A, (0, 0), &probe);
        assert!(probe.moves.borrow().is_empty());
    }

    #[test]
    fn a_teleport_and_a_drag_carry_the_riders_and_the_rescue_brings_them_home() {
        let mut flow = flow_with_riders();
        let probe = MockFollow::default()
            .with(A, 100, 40, 900, 1000)
            .with(R, 300, 200, 520, 720)
            .with(R2, 150, 600, 300, 200);
        // Teleport: A moved from (100, 40) to (100, -1044).
        probe.place(A, 100, -1044);
        flow.follow_anchor_now_with(A, (100, 40), &probe);
        assert_eq!(probe.rect(R).map(pos), Some((300, -884)));
        assert_eq!(probe.rect(R2).map(pos), Some((150, -484)));
        // Drag: offsets taken at the start, kept on every move and at the end.
        probe.place(A, 100, 40);
        probe.place(R, 300, 200);
        flow.riders_drag_begin_with(A, &probe);
        probe.place(A, 400, 90);
        flow.riders_drag_move_with(A, &probe);
        assert_eq!(probe.rect(R).map(pos), Some((600, 250)));
        probe.place(A, 410, 95);
        flow.riders_drag_end_with(A, &probe);
        assert_eq!(probe.rect(R).map(pos), Some((610, 255)));
        assert!(flow.riders.values().all(|r| r.drag_offset.is_none()));
        // A move of another window touches nobody.
        let before = probe.moves.borrow().len();
        flow.riders_drag_move_with(B, &probe);
        assert_eq!(probe.moves.borrow().len(), before);
        // The rescue: A parked at (100, -1044) comes back to (100, 40).
        probe.place(A, 100, -1044);
        probe.place(R, 300, -884);
        let snapshot = vec![(R, A, (100, -1044))];
        probe.place(A, 100, 40);
        assert_eq!(FlowWM::follow_rescued_anchors_with(&snapshot, &probe), 1);
        assert_eq!(probe.rect(R).map(pos), Some((300, 200)));
    }
}
