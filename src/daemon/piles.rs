//! Book mode's piles — `[columns] layout = "book"` (ADR-0022).
//!
//! In book mode the projection puts every column out of view **on screen**,
//! at the slot rect of the leftmost or rightmost visible column (its
//! pile's **top**), see
//! [`project_with`](crate::layout::projection::project_with). This module
//! keeps the z-order those rects depend on: within a pile the column
//! nearest to the view is highest, every member is below its top, and a
//! member's border overlay stays directly above its own window
//! ([`pile_chains`], [`FlowWM::order_piles_with`]). The order is checked
//! and repaired before every batch that moves a book-mode strip and once
//! per `[focus] foreground_sync_interval_ms` against the live z-order (an
//! app that raised itself without being activated). The `set-layout`
//! command lives here too.
//!
//! The Win32 side is behind [`PileProbe`] so the ordering runs against a
//! mock in the unit tests. Design, the drift check and what is not handled:
//! `docs/src/dev-guide/piles.md`.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::ipc::message::{LayoutRequest, SocketResponse};
use crate::layout::types::{ActualLayout, LayoutMode, PileSide, VirtualLayout};
use crate::registry::types::{TilingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::types::FlowWM;

/// The Win32 questions the pile ordering asks.
pub(super) trait PileProbe {
    /// Every top-level window, topmost first ([`registry_win32::z_order`]).
    fn z_order(&self) -> Vec<isize>;
    /// Seat `hwnd` directly below `after` without moving, sizing or
    /// activating it and without its owned windows
    /// ([`registry_win32::insert_below_quiet`]); `true` when issued.
    fn insert_below(&self, hwnd: isize, after: isize) -> bool;
}

/// The production [`PileProbe`]: plain Win32.
pub(super) struct Win32Piles;

impl PileProbe for Win32Piles {
    fn z_order(&self) -> Vec<isize> {
        registry_win32::z_order()
    }

    fn insert_below(&self, hwnd: isize, after: isize) -> bool {
        registry_win32::insert_below_quiet(hwnd, after)
    }
}

/// One pile as z-order groups, top first: the top column's rows, then each
/// member column's rows, nearest to the view first. Every window of a group
/// must be above every window of the next one.
pub(super) type Chain = Vec<Vec<isize>>;

/// The piles of one strip ([`project_with`] output `actual` for `layout`):
/// the left chain (top = leftmost visible column, members = the columns
/// before it, nearest first) and the right chain (top = rightmost visible
/// column, members after it), each only when it has a member. With one
/// visible column both chains start with it. A strip with no visible
/// non-fullscreen column (a fullscreen column starts the view, or the strip
/// parking fallback) has no tops and so no chains: what lies under a
/// topmost fullscreen window needs no order.
///
/// [`project_with`]: crate::layout::projection::project_with
#[must_use]
pub(super) fn pile_chains(layout: &VirtualLayout, actual: &ActualLayout) -> Vec<Chain> {
    let side_of = |col: &crate::layout::types::Column| {
        col.rows
            .first()
            .and_then(|r| actual.find(r.window_id))
            .map(|e| e.pile)
    };
    let rows = |i: usize| -> Vec<isize> {
        layout.columns[i]
            .rows
            .iter()
            .map(|r| r.window_id.0)
            .collect()
    };
    let mut visible = Vec::new();
    let mut left = Vec::new();
    let mut right = Vec::new();
    for (i, col) in layout.columns.iter().enumerate() {
        match side_of(col) {
            Some(Some(PileSide::Left)) => left.push(i),
            Some(Some(PileSide::Right)) => right.push(i),
            Some(None) if !col.fullscreen => visible.push(i),
            _ => {}
        }
    }
    let (Some(&first), Some(&last)) = (visible.first(), visible.last()) else {
        return Vec::new();
    };
    let mut chains = Vec::new();
    if !left.is_empty() {
        let mut chain = vec![rows(first)];
        chain.extend(left.iter().rev().map(|&i| rows(i)));
        chains.push(chain);
    }
    if !right.is_empty() {
        let mut chain = vec![rows(last)];
        chain.extend(right.iter().map(|&i| rows(i)));
        chains.push(chain);
    }
    chains
}

/// Whether `chain` is in order in `z` (top-level windows, topmost first):
/// every window of a group above every window of the next group. A window
/// missing from `z` (gone) is ignored.
#[must_use]
pub(super) fn chain_in_order(chain: &Chain, z: &[isize]) -> bool {
    let pos = |h: isize| z.iter().position(|&w| w == h);
    let mut prev_lowest: Option<usize> = None;
    for group in chain {
        let ps: Vec<usize> = group.iter().filter_map(|&h| pos(h)).collect();
        let (Some(&highest), Some(&lowest)) = (ps.iter().min(), ps.iter().max()) else {
            continue;
        };
        if prev_lowest.is_some_and(|p| highest < p) {
            return false;
        }
        prev_lowest = Some(lowest);
    }
    true
}

impl FlowWM {
    /// Put the piles of monitor `monitor_idx`'s active strip in order for
    /// `layout` / `actual` (the layout about to be animated) — the step
    /// every submit path takes before it hands a book-mode batch to the
    /// animator. Strip mode costs nothing (no entry is piled).
    pub(super) fn order_piles(
        &mut self,
        monitor_idx: usize,
        layout: &VirtualLayout,
        actual: &ActualLayout,
    ) {
        if actual.entries.iter().all(|e| e.pile.is_none()) {
            return;
        }
        let _ = self.order_piles_with(&Win32Piles, monitor_idx, layout, actual);
    }

    /// [`order_piles`](Self::order_piles) against `probe`; returns the
    /// windows that were out of order (re-seated), for the drift log.
    ///
    /// One z-order read; a chain already in order is left alone. Otherwise
    /// each group after the top is seated below the lowest window placed so
    /// far: the top's own rows are seated one under the other (rows never
    /// overlap, so this changes nothing visible) and every member row goes
    /// under the previous one, its border overlay re-seated directly above
    /// it so a member's ring never shows over its top.
    pub(super) fn order_piles_with<P: PileProbe>(
        &mut self,
        probe: &P,
        monitor_idx: usize,
        layout: &VirtualLayout,
        actual: &ActualLayout,
    ) -> Vec<isize> {
        // The window a drag holds is the user's: it stays above everything
        // while it follows the mouse, even when the committed layout (an
        // edge-scroll flip mid-drag) still has it in a pile.
        let dragged = self.drag_state.as_ref().map(|d| d.dragged_hwnd());
        let chains: Vec<Chain> = pile_chains(layout, actual)
            .into_iter()
            .map(|c| {
                c.into_iter()
                    .map(|g| g.into_iter().filter(|&h| Some(h) != dragged).collect())
                    .filter(|g: &Vec<isize>| !g.is_empty())
                    .collect()
            })
            .collect();
        if chains.is_empty() {
            return Vec::new();
        }
        let z = probe.z_order();
        let mut reseated = Vec::new();
        for chain in &chains {
            if chain_in_order(chain, &z) {
                continue;
            }
            let mut groups = chain.iter().flatten().copied();
            let Some(mut prev) = groups.next() else {
                continue;
            };
            for hwnd in groups {
                if probe.insert_below(hwnd, prev) {
                    if let Some(border) = self
                        .registry
                        .get_window(HWND(hwnd as *mut _))
                        .and_then(|w| w.border.as_ref())
                    {
                        border.seat_above_target();
                    }
                    reseated.push(hwnd);
                } else {
                    log::debug!(
                        "piles: monitor {monitor_idx}: could not seat hwnd {hwnd} below {prev}"
                    );
                }
                prev = hwnd;
            }
        }
        reseated
    }

    /// The periodic book-mode pass (next to the floating layer's, once per
    /// `[focus] foreground_sync_interval_ms`): every book-mode monitor's
    /// active strip is checked against one z-order read and re-seated when
    /// a member rose above its top without being activated. Activation is
    /// not drift: it flips the book to that window through the focus hook.
    pub(super) fn check_piles(&mut self) {
        if self.tiling_paused || self.drag_state.is_some() {
            return;
        }
        let interval = Duration::from_millis(self.config.focus.foreground_sync_interval_ms);
        if self.last_pile_check.elapsed() < interval {
            return;
        }
        self.last_pile_check = Instant::now();
        let _ = self.check_piles_with(&Win32Piles);
    }

    /// [`check_piles`](Self::check_piles) against `probe`, without the
    /// interval gate; returns the re-seated windows. Also the "seat every
    /// pile" step after startup, `tiling on`, a loadout and a hotplug.
    pub(super) fn check_piles_with<P: PileProbe>(&mut self, probe: &P) -> Vec<isize> {
        let mut all = Vec::new();
        for mi in 0..self.monitors.len() {
            let space = self.monitors[mi].active_scrolling();
            if space.layout_mode() != LayoutMode::Book {
                continue;
            }
            let layout = space.virtual_layout().clone();
            let actual = space.actual_layout().clone();
            all.extend(self.order_piles_with(probe, mi, &layout, &actual));
        }
        if all.is_empty() {
            self.pile_drift.clear();
        } else if all != self.pile_drift {
            log::info!(
                "piles: re-seated {} window(s) out of pile order: {all:?}",
                all.len()
            );
            self.pile_drift = all.clone();
        }
        all
    }

    /// Seat every book-mode pile now, without the interval gate (startup,
    /// `tiling on`, loadout, hotplug).
    pub(super) fn seat_all_piles(&mut self) {
        if self.tiling_paused {
            return;
        }
        let _ = self.check_piles_with(&Win32Piles);
    }

    /// `flow dispatch set-layout strip|book|toggle`: switch the screen of
    /// the focused window (else the active monitor) on every one of its
    /// workspaces and animate its active workspace into the new mode.
    /// Runtime only — a config reload puts every screen back to
    /// `[columns] layout`. Answers `{"layout": "<mode>", "monitor": <i>}`.
    pub(super) fn dispatch_set_layout(&mut self, request: LayoutRequest) -> SocketResponse {
        let idx = self
            .registry
            .focused()
            .filter(|id| {
                self.registry
                    .get_window(HWND(id.0 as *mut _))
                    .is_some_and(|w| {
                        matches!(w.state, WindowState::Tiling(TilingState::Active { .. }))
                    })
            })
            .map(|id| self.monitor_index_for_window(id))
            .filter(|&m| m < self.monitors.len())
            .unwrap_or(self.active_monitor);
        let current = self.monitors[idx].active_scrolling().layout_mode();
        let mode = match request {
            LayoutRequest::Strip => LayoutMode::Strip,
            LayoutRequest::Book => LayoutMode::Book,
            LayoutRequest::Toggle => current.toggled(),
        };
        let wrap = self.monitors[idx].active_scrolling().config().wrap_around;
        log::info!(
            "layout: monitor {idx} {} -> {}",
            current.name(),
            mode.name()
        );
        let active_id = self.monitors[idx].active_workspace_id();
        let mut active_applied = None;
        for ws in self.monitors[idx].workspaces_mut() {
            let applied = ws.scrolling.set_layout_mode(mode, wrap);
            if ws.id == active_id {
                active_applied = Some(applied);
            }
        }
        if let Some(applied) = active_applied {
            self.animate_layout_on(idx, &applied);
        }
        SocketResponse::Data {
            payload: serde_json::json!({
                "layout": mode.name(),
                "monitor": idx,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{Rect, WindowId};
    use crate::layout::projection::project_with;
    use crate::layout::types::{Column, MonitorInfo, Padding, Row};
    use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};
    use std::cell::RefCell;

    const W: i32 = 1920;

    fn monitor() -> MonitorInfo {
        MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: W,
            height: 1080,
        })
    }

    fn pad() -> Padding {
        Padding {
            window_gap: 10,
            up: 0,
            down: 0,
        }
    }

    /// `n` single-row columns of `width`, ids 1..=n, camera at `offset`.
    fn strip(n: usize, width: i32, offset: i32) -> VirtualLayout {
        VirtualLayout::with_columns(
            (1..=n)
                .map(|i| Column::with_row(width, Row::new(WindowId(i as isize), 1060)))
                .collect(),
            offset,
        )
    }

    fn book(vl: &VirtualLayout) -> ActualLayout {
        project_with(vl, &monitor(), &pad(), 0, LayoutMode::Book)
    }

    /// A z-order the mock keeps; `insert_below` really moves the window.
    struct MockZ(RefCell<Vec<isize>>);

    impl MockZ {
        fn new(order: &[isize]) -> Self {
            Self(RefCell::new(order.to_vec()))
        }
        fn order(&self) -> Vec<isize> {
            self.0.borrow().clone()
        }
    }

    impl PileProbe for MockZ {
        fn z_order(&self) -> Vec<isize> {
            self.order()
        }
        fn insert_below(&self, hwnd: isize, after: isize) -> bool {
            let mut z = self.0.borrow_mut();
            let Some(from) = z.iter().position(|&w| w == hwnd) else {
                return false;
            };
            z.remove(from);
            let Some(at) = z.iter().position(|&w| w == after) else {
                return false;
            };
            z.insert(at + 1, hwnd);
            true
        }
    }

    #[test]
    fn half_width_book_has_two_chains_nearest_first() {
        // 1/2: [1 2 | 3 4 5] with the view on 3 and 4.
        let step = (W - 30) / 2 + 10;
        let vl = strip(5, (W - 30) / 2, 2 * step);
        let actual = book(&vl);
        let chains = pile_chains(&vl, &actual);
        assert_eq!(
            chains,
            vec![vec![vec![3], vec![2], vec![1]], vec![vec![4], vec![5]]]
        );
    }

    #[test]
    fn one_column_view_is_the_top_of_both_piles() {
        let vl = strip(3, W - 20, W - 10);
        let actual = book(&vl);
        assert_eq!(
            pile_chains(&vl, &actual),
            vec![vec![vec![2], vec![1]], vec![vec![2], vec![3]]]
        );
    }

    #[test]
    fn strip_mode_and_a_fullscreen_view_have_no_chains() {
        let vl = strip(4, (W - 30) / 2, 0);
        let strip_actual = project_with(&vl, &monitor(), &pad(), 0, LayoutMode::Strip);
        assert!(pile_chains(&vl, &strip_actual).is_empty());
        let mut fs = strip(3, (W - 30) / 2, 0);
        fs.columns[0].fullscreen = true;
        let actual = book(&fs);
        assert!(actual.entries[1].pile.is_some(), "members pile under F");
        assert!(pile_chains(&fs, &actual).is_empty());
    }

    #[test]
    fn order_check_is_per_group() {
        let chain: Chain = vec![vec![10, 11], vec![2], vec![1]];
        // The top's two rows in any order, members below both.
        assert!(chain_in_order(&chain, &[11, 10, 2, 1]));
        assert!(chain_in_order(&chain, &[10, 99, 11, 2, 98, 1]));
        // A member between the top's rows, or members swapped: out of order.
        assert!(!chain_in_order(&chain, &[10, 2, 11, 1]));
        assert!(!chain_in_order(&chain, &[10, 11, 1, 2]));
        // A window gone from the z-order is ignored.
        assert!(chain_in_order(&chain, &[10, 11, 1]));
    }

    fn flow_with(vl: VirtualLayout) -> FlowWM {
        let rules = crate::config::types::WindowRulesConfig::default();
        let registry = crate::registry::WindowRegistry::new(&rules, &rules);
        let screen = monitor().work_area;
        let ws = Workspace::new(
            WorkspaceId(1),
            ScrollingSpace::new(monitor(), 945, 320, 100, 100, pad(), 2),
        );
        let m = Monitor::new(screen, screen, vec![ws], 0);
        let mut flow = FlowWM::new_for_test(registry, vec![m]);
        let space = flow.monitors[0].active_scrolling_mut();
        let _ = space.set_layout_mode(LayoutMode::Book, Default::default());
        let _ = space.commit_layout(vl);
        flow
    }

    #[test]
    fn seating_puts_members_under_their_top_nearest_first_and_skips_ordered_piles() {
        let step = (W - 30) / 2 + 10;
        let vl = strip(5, (W - 30) / 2, 2 * step);
        let mut flow = flow_with(vl.clone());
        let actual = flow.monitors[0].active_scrolling().actual_layout().clone();
        // Everything inverted: 1 on top, 5 at the bottom, foreign 99 between.
        let z = MockZ::new(&[1, 2, 99, 3, 4, 5]);
        let moved = flow.order_piles_with(&z, 0, &vl, &actual);
        assert_eq!(moved, vec![2, 1]);
        let order = z.order();
        let pos = |h: isize| order.iter().position(|&w| w == h).unwrap();
        assert!(pos(3) < pos(2) && pos(2) < pos(1), "{order:?}");
        // The right pile was already in order and was not touched.
        assert!(pos(4) < pos(5));
        // In order now: a second pass moves nothing.
        assert!(flow.order_piles_with(&z, 0, &vl, &actual).is_empty());
    }

    #[test]
    fn the_periodic_pass_repairs_a_member_that_rose_and_ignores_strip_screens() {
        let step = (W - 30) / 2 + 10;
        let vl = strip(4, (W - 30) / 2, step);
        let mut flow = flow_with(vl);
        let z = MockZ::new(&[2, 1, 3, 4]);
        assert!(flow.check_piles_with(&z).is_empty());
        // Window 4 raised itself above its top (3) without activation.
        let z = MockZ::new(&[4, 2, 1, 3]);
        assert_eq!(flow.check_piles_with(&z), vec![4]);
        let order = z.order();
        let pos = |h: isize| order.iter().position(|&w| w == h).unwrap();
        assert!(pos(3) < pos(4), "{order:?}");
        // Strip mode: nothing is read or moved.
        let space = flow.monitors[0].active_scrolling_mut();
        let _ = space.set_layout_mode(LayoutMode::Strip, Default::default());
        let z = MockZ::new(&[4, 2, 1, 3]);
        assert!(flow.check_piles_with(&z).is_empty());
        assert_eq!(z.order(), vec![4, 2, 1, 3]);
    }
}
