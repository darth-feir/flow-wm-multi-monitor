//! The floating layer — `[floating] always_on_top`.
//!
//! niri keeps its floating windows above the tiled strip at all times; on
//! Windows the only z-order band above ordinary windows is the topmost band
//! (`WS_EX_TOPMOST`), so the layer's members — active floats and
//! explicit-rule-ignored windows, while the option and tiling are on — are
//! kept there. Every transition that can change a window's membership ends
//! in [`FlowWM::sync_top_layer`], which compares what is wanted
//! ([`layer_wanted`]) with what the daemon holds ([`Window::on_top`]) and
//! applies the one step [`layer_step`] decides. Membership, the transition
//! table and the interplay with `daemon::fullscreen` are in
//! `docs/src/dev-guide/floating-space.md`, *The floating layer*.
//!
//! The Win32 questions are behind [`TopmostProbe`] so the transitions run
//! against a mock in the unit tests; the tiling on/off probe extends it.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::registry::types::{FloatingState, IgnoredReason, WindowState};
use crate::registry::win32 as registry_win32;

use super::types::FlowWM;

/// The two Win32 questions the layer asks.
pub(super) trait TopmostProbe {
    /// `WS_EX_TOPMOST` in the window's extended style
    /// ([`registry_win32::is_topmost`]).
    fn is_topmost(&self, hwnd: isize) -> bool;
    /// `SetWindowPos(HWND_TOPMOST | HWND_NOTOPMOST)` without moving, sizing
    /// or activating ([`registry_win32::set_topmost`]); `true` when issued
    /// on a live window.
    fn set_topmost(&self, hwnd: isize, topmost: bool) -> bool;
}

/// The production [`TopmostProbe`]: plain Win32.
pub(super) struct Win32Topmost;

impl TopmostProbe for Win32Topmost {
    fn is_topmost(&self, hwnd: isize) -> bool {
        registry_win32::is_topmost(hwnd)
    }

    fn set_topmost(&self, hwnd: isize, topmost: bool) -> bool {
        registry_win32::set_topmost(hwnd, topmost)
    }
}

/// Whether a window in `state` belongs in the floating layer: an active
/// float, or a window an explicit rule ignores. Minimized, hidden, tiled
/// and OS-ignored (maximized / fullscreen) windows do not.
#[must_use]
pub(super) fn in_top_layer(state: &WindowState) -> bool {
    matches!(
        state,
        WindowState::Floating(FloatingState::Active { .. })
            | WindowState::Ignored(IgnoredReason::ExplicitRule)
    )
}

/// Whether the daemon wants a window in the layer right now: the option is
/// on, tiling is on, the window is not one the daemon put fullscreen (that
/// code owns its topmost flag), and its state is a member state.
#[must_use]
pub(super) fn layer_wanted(
    always_on_top: bool,
    tiling_on: bool,
    fullscreen: bool,
    state: &WindowState,
) -> bool {
    always_on_top && tiling_on && !fullscreen && in_top_layer(state)
}

/// The one z-order step a sync applies to a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LayerStep {
    /// The window enters the layer; `raise` says `HWND_TOPMOST` is needed
    /// (it is already topmost otherwise, and that flag is remembered).
    Enter { raise: bool },
    /// The window leaves the layer; `restore_to` is the flag to set when
    /// the current one differs from the remembered original (`None`: the
    /// window already has its original flag — nothing to do).
    Leave { restore_to: Option<bool> },
    /// The window is a member that Windows demoted: `HWND_TOPMOST` again.
    Reassert,
}

/// Pure decision behind [`FlowWM::sync_top_layer`]: given whether the
/// window is `wanted` in the layer, whether the daemon holds it there
/// (`on_top`), whether it is topmost on the desktop right now and the flag
/// it had before the daemon touched it, the step to apply — or `None`.
#[must_use]
pub(super) fn layer_step(
    wanted: bool,
    on_top: bool,
    topmost_now: bool,
    was_topmost: bool,
) -> Option<LayerStep> {
    match (wanted, on_top) {
        (true, false) => Some(LayerStep::Enter {
            raise: !topmost_now,
        }),
        (true, true) => (!topmost_now).then_some(LayerStep::Reassert),
        (false, true) => Some(LayerStep::Leave {
            restore_to: (topmost_now != was_topmost).then_some(was_topmost),
        }),
        (false, false) => None,
    }
}

/// What one pass over every window did (`tiling on|off`, reload, startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct LayerSummary {
    /// Windows that entered the layer.
    pub entered: usize,
    /// Windows that left it.
    pub left: usize,
    /// Members raised again after Windows demoted them.
    pub reasserted: usize,
}

impl LayerSummary {
    fn count(&mut self, step: Option<LayerStep>) {
        match step {
            Some(LayerStep::Enter { .. }) => self.entered += 1,
            Some(LayerStep::Leave { .. }) => self.left += 1,
            Some(LayerStep::Reassert) => self.reasserted += 1,
            None => {}
        }
    }

    /// Whether anything changed.
    #[must_use]
    pub fn changed(&self) -> bool {
        self.entered + self.left + self.reasserted > 0
    }
}

impl FlowWM {
    /// Whether `hwnd` is wanted in the layer right now ([`layer_wanted`]
    /// over the daemon's state); `None` for an untracked window.
    fn layer_wanted_for(&self, hwnd: isize) -> Option<bool> {
        let window = self.registry.get_window(HWND(hwnd as *mut _))?;
        Some(layer_wanted(
            self.config.floating.always_on_top,
            !self.tiling_paused,
            self.is_fullscreen(hwnd),
            &window.state,
        ))
    }

    /// Bring `hwnd`'s place in the layer in line with its state: enter,
    /// leave, re-assert or nothing (see the [module docs](self)). Called
    /// after every transition that can change membership. Cheap for a
    /// window that is neither wanted nor held: one registry lookup, no
    /// Win32. Returns the step applied.
    pub(super) fn sync_top_layer(&mut self, hwnd: isize) -> Option<LayerStep> {
        self.sync_top_layer_with(hwnd, &Win32Topmost)
    }

    /// [`sync_top_layer`](Self::sync_top_layer) against `probe`.
    pub(super) fn sync_top_layer_with<P: TopmostProbe>(
        &mut self,
        hwnd: isize,
        probe: &P,
    ) -> Option<LayerStep> {
        let wanted = self.layer_wanted_for(hwnd)?;
        self.apply_layer_step(hwnd, wanted, probe)
    }

    /// Apply [`layer_step`] for `hwnd` given `wanted`, reading the
    /// ex-style only when the window is wanted or held, and keep the
    /// record's `on_top` / `was_topmost` in step.
    fn apply_layer_step<P: TopmostProbe>(
        &mut self,
        hwnd: isize,
        wanted: bool,
        probe: &P,
    ) -> Option<LayerStep> {
        let (on_top, was_topmost) = {
            let w = self.registry.get_window(HWND(hwnd as *mut _))?;
            (w.on_top, w.was_topmost)
        };
        if !wanted && !on_top {
            return None;
        }
        let topmost_now = probe.is_topmost(hwnd);
        let step = layer_step(wanted, on_top, topmost_now, was_topmost)?;
        match step {
            LayerStep::Enter { raise } => {
                if raise && !probe.set_topmost(hwnd, true) {
                    log::debug!("top layer: could not raise hwnd {hwnd:#x}");
                }
                if let Some(w) = self.registry.get_window_mut(HWND(hwnd as *mut _)) {
                    w.on_top = true;
                    w.was_topmost = topmost_now;
                }
                log::debug!("top layer: hwnd {hwnd:#x} entered (was_topmost={topmost_now})");
            }
            LayerStep::Leave { restore_to } => {
                if let Some(flag) = restore_to
                    && !probe.set_topmost(hwnd, flag)
                {
                    log::debug!("top layer: could not restore topmost={flag} on hwnd {hwnd:#x}");
                }
                if let Some(w) = self.registry.get_window_mut(HWND(hwnd as *mut _)) {
                    w.on_top = false;
                    w.was_topmost = false;
                }
                log::debug!("top layer: hwnd {hwnd:#x} left (restored to {restore_to:?})");
            }
            LayerStep::Reassert => {
                if !probe.set_topmost(hwnd, true) {
                    log::debug!("top layer: could not re-assert hwnd {hwnd:#x}");
                }
                log::debug!("top layer: hwnd {hwnd:#x} re-asserted");
            }
        }
        Some(step)
    }

    /// Sync every tracked window ([`sync_top_layer`](Self::sync_top_layer)):
    /// startup, `tiling on|off`, config reload, loadout load, a display
    /// change. O(tracked windows); the Win32 reads happen only for members
    /// and candidates.
    pub(super) fn sync_all_top_layer(&mut self) -> LayerSummary {
        self.sync_all_top_layer_with(&Win32Topmost)
    }

    /// [`sync_all_top_layer`](Self::sync_all_top_layer) against `probe`.
    pub(super) fn sync_all_top_layer_with<P: TopmostProbe>(&mut self, probe: &P) -> LayerSummary {
        let hwnds: Vec<isize> = self.registry.windows().map(|w| w.hwnd.0 as isize).collect();
        let mut summary = LayerSummary::default();
        for hwnd in hwnds {
            summary.count(self.sync_top_layer_with(hwnd, probe));
        }
        if summary.changed() {
            log::info!(
                "top layer: {} window(s) entered, {} left, {} re-asserted",
                summary.entered,
                summary.left,
                summary.reasserted
            );
        }
        summary
    }

    /// Take every window the daemon holds in the layer out of it, putting
    /// the flag it had back — the hand-back used by the shutdown rescue
    /// (`flow stop`, the fail-back, session end), whatever the option and
    /// the tiling state say. Returns how many left.
    pub(super) fn release_top_layer(&mut self) -> usize {
        self.release_top_layer_with(&Win32Topmost)
    }

    /// [`release_top_layer`](Self::release_top_layer) against `probe`.
    pub(super) fn release_top_layer_with<P: TopmostProbe>(&mut self, probe: &P) -> usize {
        let held: Vec<isize> = self
            .registry
            .windows()
            .filter(|w| w.on_top)
            .map(|w| w.hwnd.0 as isize)
            .collect();
        let mut left = 0;
        for hwnd in held {
            if matches!(
                self.apply_layer_step(hwnd, false, probe),
                Some(LayerStep::Leave { .. })
            ) {
                left += 1;
            }
        }
        if left > 0 {
            log::info!("top layer: released {left} window(s)");
        }
        left
    }

    /// The periodic backstop (main loop, once per
    /// `[focus] foreground_sync_interval_ms`): raise again any member that
    /// Windows demoted. Walks only the windows the daemon holds in the
    /// layer; a member whose ex-style still says topmost costs one read.
    pub(super) fn reassert_top_layer(&mut self) {
        if !self.config.floating.always_on_top || self.tiling_paused {
            return;
        }
        let interval = Duration::from_millis(self.config.focus.foreground_sync_interval_ms);
        if self.last_top_layer_reassert.elapsed() < interval {
            return;
        }
        self.last_top_layer_reassert = Instant::now();
        let held: Vec<isize> = self
            .registry
            .windows()
            .filter(|w| w.on_top)
            .map(|w| w.hwnd.0 as isize)
            .collect();
        for hwnd in held {
            self.sync_top_layer(hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use super::*;
    use crate::common::{Rect, WindowId};
    use crate::config::types::WindowRulesConfig;
    use crate::ipc::message::{SocketMessage, SocketResponse, WindowMode};
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::registry::WindowRegistry;
    use crate::registry::types::TilingState;
    use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};

    const A: isize = 0xA;
    const F: isize = 0xF;
    const I: isize = 0x1;
    const M: isize = 0x3;

    const F_RECT: Rect = Rect {
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    };

    fn tile() -> WindowState {
        WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
    }

    fn float() -> WindowState {
        WindowState::Floating(FloatingState::Active { rect: F_RECT })
    }

    fn ignored_rule() -> WindowState {
        WindowState::Ignored(IgnoredReason::ExplicitRule)
    }

    fn fake_window(registry: &mut WindowRegistry, hwnd: isize, state: WindowState) {
        let info = crate::registry::win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: format!("win{hwnd}"),
            class: "C".into(),
            rect: Rect {
                x: 50,
                y: 60,
                width: 800,
                height: 600,
            },
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

    /// One monitor; tile A, float F (seated), explicit-rule-ignored I and
    /// Ignored(Maximized) M. Borders off, the option ON.
    fn fixture() -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        fake_window(&mut registry, A, tile());
        fake_window(&mut registry, F, float());
        fake_window(&mut registry, I, ignored_rule());
        fake_window(
            &mut registry,
            M,
            WindowState::Ignored(IgnoredReason::Maximized),
        );
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let info = MonitorInfo::single(screen);
        let ws = |id: u32| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(
                    info,
                    936,
                    320,
                    100,
                    100,
                    Padding {
                        window_gap: 16,
                        up: 0,
                        down: 0,
                    },
                    4,
                ),
            )
        };
        let mut m0 = Monitor::new(screen, screen, vec![ws(1), ws(2)], 0);
        m0.active_scrolling_mut().add_window(WindowId(A));
        m0.active_workspace_mut().floating.add(WindowId(F), F_RECT);
        registry.set_monitor(A, 0);
        registry.set_monitor(F, 0);
        registry.set_focused(A);
        let mut flow = FlowWM::new_for_test(registry, vec![m0]);
        flow.config.borders.enabled = false;
        flow.config.floating.always_on_top = true;
        flow
    }

    /// A scripted desktop: which handles are topmost, and every
    /// `SetWindowPos` issued, in order.
    #[derive(Default)]
    struct MockTopmost {
        topmost: RefCell<HashSet<isize>>,
        calls: RefCell<Vec<(isize, bool)>>,
    }

    impl MockTopmost {
        fn with_topmost(hwnds: &[isize]) -> Self {
            let m = Self::default();
            m.topmost.borrow_mut().extend(hwnds.iter().copied());
            m
        }
    }

    impl TopmostProbe for MockTopmost {
        fn is_topmost(&self, hwnd: isize) -> bool {
            self.topmost.borrow().contains(&hwnd)
        }

        fn set_topmost(&self, hwnd: isize, topmost: bool) -> bool {
            self.calls.borrow_mut().push((hwnd, topmost));
            if topmost {
                self.topmost.borrow_mut().insert(hwnd);
            } else {
                self.topmost.borrow_mut().remove(&hwnd);
            }
            true
        }
    }

    fn on_top(flow: &FlowWM, hwnd: isize) -> bool {
        flow.registry
            .get_window(HWND(hwnd as *mut _))
            .is_some_and(|w| w.on_top)
    }

    fn was_topmost(flow: &FlowWM, hwnd: isize) -> bool {
        flow.registry
            .get_window(HWND(hwnd as *mut _))
            .is_some_and(|w| w.was_topmost)
    }

    fn set_state(flow: &mut FlowWM, hwnd: isize, state: WindowState) {
        if let Some(w) = flow.registry.get_window_mut(HWND(hwnd as *mut _)) {
            w.state = state;
        }
    }

    // ── the pure rules ────────────────────────────────────────────────

    #[test]
    fn membership_is_active_floats_and_explicit_rule_ignores_only() {
        assert!(in_top_layer(&float()));
        assert!(in_top_layer(&ignored_rule()));
        assert!(!in_top_layer(&tile()));
        assert!(!in_top_layer(&WindowState::Tiling(TilingState::Minimized)));
        assert!(!in_top_layer(&WindowState::Tiling(TilingState::Hidden)));
        assert!(!in_top_layer(&WindowState::Floating(
            FloatingState::Minimized
        )));
        assert!(!in_top_layer(&WindowState::Floating(FloatingState::Hidden)));
        assert!(!in_top_layer(&WindowState::Ignored(
            IgnoredReason::Maximized
        )));
        assert!(!in_top_layer(&WindowState::Ignored(
            IgnoredReason::Fullscreen
        )));
    }

    #[test]
    fn wanted_needs_the_option_tiling_and_no_fullscreen() {
        assert!(layer_wanted(true, true, false, &float()));
        assert!(layer_wanted(true, true, false, &ignored_rule()));
        assert!(!layer_wanted(false, true, false, &float()), "option off");
        assert!(!layer_wanted(true, false, false, &float()), "tiling off");
        assert!(
            !layer_wanted(true, true, true, &float()),
            "the fullscreen code owns its topmost flag"
        );
        assert!(!layer_wanted(true, true, false, &tile()));
    }

    #[test]
    fn step_table_enters_leaves_and_reasserts_remembering_the_original_flag() {
        // Enter: raise unless already topmost (then only remember it).
        assert_eq!(
            layer_step(true, false, false, false),
            Some(LayerStep::Enter { raise: true })
        );
        assert_eq!(
            layer_step(true, false, true, false),
            Some(LayerStep::Enter { raise: false })
        );
        // Held and still topmost: nothing; held and demoted: re-assert.
        assert_eq!(layer_step(true, true, true, false), None);
        assert_eq!(
            layer_step(true, true, false, false),
            Some(LayerStep::Reassert)
        );
        // Leave: back to the original flag, only when it differs.
        assert_eq!(
            layer_step(false, true, true, false),
            Some(LayerStep::Leave {
                restore_to: Some(false)
            })
        );
        assert_eq!(
            layer_step(false, true, true, true),
            Some(LayerStep::Leave { restore_to: None })
        );
        assert_eq!(
            layer_step(false, true, false, true),
            Some(LayerStep::Leave {
                restore_to: Some(true)
            })
        );
        assert_eq!(
            layer_step(false, true, false, false),
            Some(LayerStep::Leave { restore_to: None })
        );
        // Neither wanted nor held: nothing, whatever the desktop says.
        assert_eq!(layer_step(false, false, true, true), None);
        assert_eq!(layer_step(false, false, false, false), None);
    }

    // ── the daemon side ───────────────────────────────────────────────

    #[test]
    fn option_off_touches_nothing() {
        let mut flow = fixture();
        flow.config.floating.always_on_top = false;
        let probe = MockTopmost::default();
        assert_eq!(
            flow.sync_all_top_layer_with(&probe),
            LayerSummary::default()
        );
        assert!(probe.calls.borrow().is_empty());
        assert!(!on_top(&flow, F) && !on_top(&flow, I));
    }

    #[test]
    fn sync_all_raises_floats_and_explicit_rule_ignores_only() {
        let mut flow = fixture();
        let probe = MockTopmost::default();
        let summary = flow.sync_all_top_layer_with(&probe);
        assert_eq!(
            summary,
            LayerSummary {
                entered: 2,
                left: 0,
                reasserted: 0
            }
        );
        let mut calls = probe.calls.borrow().clone();
        calls.sort_unstable();
        assert_eq!(calls, vec![(I, true), (F, true)]);
        assert!(on_top(&flow, F) && on_top(&flow, I));
        assert!(!on_top(&flow, A) && !on_top(&flow, M));
        assert!(!was_topmost(&flow, F), "F was raised by the daemon");
        // Idempotent: a second pass finds everything in place.
        assert_eq!(
            flow.sync_all_top_layer_with(&probe),
            LayerSummary::default()
        );
        assert_eq!(probe.calls.borrow().len(), 2);
        // The query reports it.
        let SocketResponse::Data { payload } = flow.query_windows_all() else {
            panic!("query");
        };
        let on_top_of = |hwnd: isize| {
            payload["windows"]
                .as_array()
                .expect("windows")
                .iter()
                .find(|w| w["hwnd"] == hwnd as i64)
                .expect("listed")["on_top"]
                .clone()
        };
        assert_eq!(on_top_of(F), serde_json::json!(true));
        assert_eq!(on_top_of(I), serde_json::json!(true));
        assert_eq!(on_top_of(A), serde_json::json!(false));
    }

    #[test]
    fn a_window_the_daemon_raised_is_lowered_when_it_leaves() {
        let mut flow = fixture();
        let probe = MockTopmost::default();
        assert_eq!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Enter { raise: true })
        );
        // Tiled: lowered again.
        set_state(&mut flow, F, tile());
        assert_eq!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Leave {
                restore_to: Some(false)
            })
        );
        assert_eq!(*probe.calls.borrow(), vec![(F, true), (F, false)]);
        assert!(!on_top(&flow, F));
        assert!(!probe.is_topmost(F));
        // Not held, not wanted: no read, no call.
        assert_eq!(flow.sync_top_layer_with(F, &probe), None);
        assert_eq!(probe.calls.borrow().len(), 2);
    }

    #[test]
    fn a_window_that_was_topmost_on_its_own_keeps_the_flag() {
        let mut flow = fixture();
        let probe = MockTopmost::with_topmost(&[F]);
        assert_eq!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Enter { raise: false })
        );
        assert!(on_top(&flow, F) && was_topmost(&flow, F));
        assert!(probe.calls.borrow().is_empty(), "already topmost: no call");
        set_state(&mut flow, F, tile());
        assert_eq!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Leave { restore_to: None })
        );
        assert!(probe.calls.borrow().is_empty());
        assert!(
            probe.is_topmost(F),
            "the daemon never clears a flag it did not set"
        );
        assert!(!on_top(&flow, F) && !was_topmost(&flow, F));
    }

    #[test]
    fn a_member_windows_demoted_is_raised_again() {
        let mut flow = fixture();
        let probe = MockTopmost::default();
        flow.sync_top_layer_with(F, &probe);
        // The app cleared the flag itself.
        probe.topmost.borrow_mut().remove(&F);
        assert_eq!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Reassert)
        );
        assert_eq!(*probe.calls.borrow(), vec![(F, true), (F, true)]);
        assert!(on_top(&flow, F));
        // The periodic pass only walks members; the interval gates it.
        probe.topmost.borrow_mut().remove(&F);
        flow.last_top_layer_reassert = Instant::now() - Duration::from_secs(1);
        // (The production pass uses Win32; the bookkeeping stays held.)
        flow.reassert_top_layer();
        assert!(on_top(&flow, F));
    }

    #[test]
    fn release_takes_every_held_window_out_whatever_the_option_says() {
        let mut flow = fixture();
        let probe = MockTopmost::with_topmost(&[I]);
        flow.sync_all_top_layer_with(&probe);
        assert!(on_top(&flow, F) && on_top(&flow, I));
        assert_eq!(flow.release_top_layer_with(&probe), 2);
        assert!(!on_top(&flow, F) && !on_top(&flow, I));
        assert!(!probe.is_topmost(F), "raised by the daemon: lowered");
        assert!(probe.is_topmost(I), "topmost on its own: kept");
        assert_eq!(flow.release_top_layer_with(&probe), 0);
    }

    #[test]
    fn turning_the_option_off_and_on_again_leaves_and_re_enters() {
        let mut flow = fixture();
        let probe = MockTopmost::default();
        flow.sync_all_top_layer_with(&probe);
        flow.config.floating.always_on_top = false;
        let summary = flow.sync_all_top_layer_with(&probe);
        assert_eq!((summary.entered, summary.left), (0, 2));
        assert!(!on_top(&flow, F) && !probe.is_topmost(F));
        flow.config.floating.always_on_top = true;
        let summary = flow.sync_all_top_layer_with(&probe);
        assert_eq!((summary.entered, summary.left), (2, 0));
        assert!(on_top(&flow, F) && probe.is_topmost(F));
    }

    #[test]
    fn tiling_off_is_a_leave_and_a_fullscreen_window_is_not_wanted() {
        let mut flow = fixture();
        let probe = MockTopmost::default();
        flow.sync_all_top_layer_with(&probe);
        flow.tiling_paused = true;
        assert_eq!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Leave {
                restore_to: Some(false)
            })
        );
        flow.tiling_paused = false;
        assert_eq!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Enter { raise: true })
        );
        // Fullscreen: the fullscreen code owns the flag; the layer lets go
        // (lowering it first — the fullscreen code raises it right after).
        flow.fullscreen.insert(
            F,
            super::super::fullscreen::FullscreenOrigin::Float {
                monitor: 0,
                workspace: WorkspaceId(1),
                rect: F_RECT,
            },
        );
        assert!(matches!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Leave { .. })
        ));
        assert!(!on_top(&flow, F));
        flow.fullscreen.remove(&F);
        assert!(matches!(
            flow.sync_top_layer_with(F, &probe),
            Some(LayerStep::Enter { .. })
        ));
    }

    // ── the transitions, through the production paths (Win32 fails cleanly
    //    on a fake handle; the bookkeeping is what is observed) ───────

    #[test]
    fn set_window_float_enters_and_tile_leaves() {
        let mut flow = fixture();
        // `set-window` records the learned rule into the config dir: keep
        // that out of the working directory.
        let config_dir =
            std::env::temp_dir().join(format!("flow-top-layer-{}", std::process::id()));
        std::fs::create_dir_all(&config_dir).expect("temp config dir");
        flow.config_dir = config_dir.clone();
        flow.registry.set_focused(A);
        assert_eq!(
            flow.dispatch(&SocketMessage::SetWindow {
                mode: WindowMode::Float
            }),
            SocketResponse::Ok
        );
        assert!(on_top(&flow, A), "a toggled float enters");
        assert_eq!(
            flow.dispatch(&SocketMessage::SetWindow {
                mode: WindowMode::Tile
            }),
            SocketResponse::Ok
        );
        assert!(!on_top(&flow, A), "tiled again: left");
        assert_eq!(
            flow.dispatch(&SocketMessage::SetWindow {
                mode: WindowMode::Cycle
            }),
            SocketResponse::Ok
        );
        assert!(on_top(&flow, A), "cycle to float enters");
        // Adopting an explicit-rule ignore as a tile leaves too.
        flow.sync_top_layer(I);
        assert!(on_top(&flow, I));
        flow.registry.set_focused(I);
        // (adopt_ignored_window asks Win32 whether the fake handle is
        // zoomed — false — so the adoption goes through.)
        assert_eq!(
            flow.dispatch(&SocketMessage::SetWindow {
                mode: WindowMode::Tile
            }),
            SocketResponse::Ok
        );
        assert!(!on_top(&flow, I));
        let _ = std::fs::remove_dir_all(config_dir);
    }

    #[test]
    fn minimize_hide_and_restore_leave_and_re_enter() {
        let mut flow = fixture();
        flow.sync_top_layer(F);
        assert!(on_top(&flow, F));
        flow.on_window_minimized(F);
        assert!(!on_top(&flow, F), "a minimized float is out");
        flow.on_window_restored(F);
        assert!(on_top(&flow, F), "restored: in again");
        // Hidden (the fake handle reads as not visible).
        flow.on_window_hidden(F);
        assert!(matches!(
            flow.registry
                .get_window(HWND(F as *mut _))
                .map(|w| &w.state),
            Some(WindowState::Floating(FloatingState::Hidden))
        ));
        assert!(!on_top(&flow, F), "a hidden float is out");
    }

    #[test]
    fn fullscreen_hands_the_flag_over_and_gets_it_back_on_return() {
        let mut flow = fixture();
        flow.sync_top_layer(F);
        assert_eq!(
            flow.set_window_to_fullscreen(WindowId(F)),
            SocketResponse::Ok
        );
        assert!(flow.is_fullscreen(F));
        assert!(
            !on_top(&flow, F),
            "fullscreen: the fullscreen code owns topmost"
        );
        assert_eq!(
            flow.restore_from_fullscreen(
                WindowId(F),
                super::super::fullscreen::FullscreenExit::Origin
            ),
            SocketResponse::Ok
        );
        assert!(!flow.is_fullscreen(F));
        assert!(on_top(&flow, F), "back to a float: in the layer again");
        // Fullscreen from a tile, then out to a tile: never in the layer.
        assert_eq!(
            flow.set_window_to_fullscreen(WindowId(A)),
            SocketResponse::Ok
        );
        assert!(!on_top(&flow, A));
        assert_eq!(
            flow.restore_from_fullscreen(
                WindowId(A),
                super::super::fullscreen::FullscreenExit::Tile
            ),
            SocketResponse::Ok
        );
        assert!(!on_top(&flow, A));
    }

    #[test]
    fn a_fresh_explicit_rule_ignore_and_a_destroyed_member_need_no_special_case() {
        let mut flow = fixture();
        flow.sync_all_top_layer();
        assert!(on_top(&flow, I));
        flow.on_window_destroyed(I);
        assert!(!flow.registry.is_tracked(I));
        flow.on_window_destroyed(F);
        assert_eq!(flow.release_top_layer(), 0);
    }
}
