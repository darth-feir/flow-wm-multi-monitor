//! The floating layer — `[floating] always_on_top`.
//!
//! niri keeps its floating windows above the tiled strip at all times; on
//! Windows the only z-order band above ordinary windows is the topmost band
//! (`WS_EX_TOPMOST`), so the layer's members — active floats and
//! explicit-rule-ignored windows, while the option and tiling are on — are
//! kept there **while flow owns the foreground** and dropped under the
//! foreground the moment it moves to a fullscreen app or a non-flow window
//! (ADR-0003, `docs/adr/0003-float-tile-stacking-invariant.md`). The
//! decision ([`classify_foreground`], [`FlowWM::decide_top_layer_with`]) is
//! taken in the single focus sink, on the foreground window's own resize and
//! by the periodic re-assert pass; every transition that can change a
//! window's membership ends in [`FlowWM::sync_top_layer`], which compares
//! what is wanted ([`layer_wanted`]) with what the daemon holds
//! ([`Window::on_top`]) and applies the one step [`layer_step`] decides.
//! Membership, the decision rule, the transition table and the interplay
//! with `daemon::fullscreen` are in `docs/src/dev-guide/floating-space.md`,
//! *The floating layer*.
//!
//! An untracked window owned by a flow window (a modal dialog, an error
//! box) that takes the foreground is a *rider*: classed by its anchor (the
//! first tracked window on its `GW_OWNER` chain, [`anchor_of`]) and held in
//! the topmost band with the layer, after the members ([`Rider`],
//! ADR-0024, `docs/adr/0024-owned-windows-ride-the-floating-layer.md`).
//!
//! The Win32 questions are behind [`TopmostProbe`] so the transitions run
//! against a mock in the unit tests; the tiling on/off probe extends it.

use std::cmp::Reverse;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::registry::types::{FloatingState, IgnoredReason, TilingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::types::FlowWM;

/// The Win32 questions the layer asks.
pub(super) trait TopmostProbe {
    /// `WS_EX_TOPMOST` in the window's extended style
    /// ([`registry_win32::is_topmost`]).
    fn is_topmost(&self, hwnd: isize) -> bool;
    /// `SetWindowPos(HWND_TOPMOST | HWND_NOTOPMOST)` without moving, sizing
    /// or activating ([`registry_win32::set_topmost`]); `true` when issued
    /// on a live window.
    fn set_topmost(&self, hwnd: isize, topmost: bool) -> bool;
    /// `SetWindowPos(hwnd, after)`: seat `hwnd` directly below `after`,
    /// leaving the topmost band ([`registry_win32::insert_below`]); `true`
    /// when issued on live windows.
    fn insert_below(&self, hwnd: isize, after: isize) -> bool;
    /// Fullscreen by live geometry and style: covers its monitor's full
    /// rect with no caption / thick frame
    /// ([`registry_win32::is_fullscreen_on_its_monitor`]).
    fn is_fullscreen_live(&self, hwnd: isize) -> bool;
    /// The window class ([`registry_win32::class_name`]); `None` when the
    /// handle is gone.
    fn class_name(&self, hwnd: isize) -> Option<String>;
    /// The window's owner (`GW_OWNER`, [`registry_win32::owner_of`]);
    /// `None` for an unowned window or a handle that is gone.
    fn owner_of(&self, hwnd: isize) -> Option<isize>;
    /// `IsWindowVisible` ([`registry_win32::is_window_visible`]).
    fn is_visible(&self, hwnd: isize) -> bool;
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

    fn insert_below(&self, hwnd: isize, after: isize) -> bool {
        registry_win32::insert_below(hwnd, after)
    }

    fn is_fullscreen_live(&self, hwnd: isize) -> bool {
        registry_win32::is_fullscreen_on_its_monitor(hwnd)
    }

    fn class_name(&self, hwnd: isize) -> Option<String> {
        registry_win32::class_name(hwnd)
    }

    fn owner_of(&self, hwnd: isize) -> Option<isize> {
        registry_win32::owner_of(hwnd)
    }

    fn is_visible(&self, hwnd: isize) -> bool {
        registry_win32::is_window_visible(HWND(hwnd as *mut _))
    }
}

/// Window classes whose activation leaves the layer's decision alone: the
/// taskbars, the desktop, Start / search, the Alt+Tab host, and the
/// daemon's own surfaces (the keymap's OSD is shown `NoActivate` and never
/// takes the foreground). Why they are not "non-flow windows" is in the dev
/// guide (*The floating layer*, the shell exception).
pub(super) const SHELL_CLASSES: &[&str] = &[
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Progman",
    "WorkerW",
    "Windows.UI.Core.CoreWindow",
    "XamlExplorerHostIslandWindow",
    "ForegroundStaging",
    "FlowBorderOverlay",
    "FlowSessionSink",
];

/// Whether `class` is one of [`SHELL_CLASSES`].
#[must_use]
pub(super) fn is_shell_class(class: &str) -> bool {
    SHELL_CLASSES.contains(&class)
}

/// How many `GW_OWNER` hops [`anchor_of`] walks before it gives up.
pub(super) const OWNER_HOPS: usize = 8;

/// The first window on `hwnd`'s owner chain (its owner, the owner's owner,
/// ... at most [`OWNER_HOPS`] hops; a cycle ends the walk) for which
/// `is_tracked` holds: a rider's *anchor* (ADR-0024). `None` for an unowned
/// window or a chain that never reaches one.
#[must_use]
pub(super) fn anchor_of(
    hwnd: isize,
    owner_of: impl Fn(isize) -> Option<isize>,
    is_tracked: impl Fn(isize) -> bool,
) -> Option<isize> {
    let mut seen = vec![hwnd];
    let mut current = hwnd;
    for _ in 0..OWNER_HOPS {
        let owner = owner_of(current).filter(|o| *o != 0)?;
        if seen.contains(&owner) {
            return None;
        }
        if is_tracked(owner) {
            return Some(owner);
        }
        seen.push(owner);
        current = owner;
    }
    None
}

/// Whether `target` is on `hwnd`'s owner chain ([`anchor_of`]'s walk).
#[must_use]
pub(super) fn owned_by(
    hwnd: isize,
    target: isize,
    owner_of: impl Fn(isize) -> Option<isize>,
) -> bool {
    anchor_of(hwnd, owner_of, |h| h == target).is_some()
}

/// How many owners `hwnd` has above it (capped at [`OWNER_HOPS`]): riders
/// are raised owner first, so an owned dialog ends above its owner.
fn owner_depth(hwnd: isize, owner_of: impl Fn(isize) -> Option<isize>) -> usize {
    let mut depth = 0;
    let mut current = hwnd;
    while depth < OWNER_HOPS {
        match owner_of(current).filter(|o| *o != 0 && *o != hwnd) {
            Some(owner) => {
                depth += 1;
                current = owner;
            }
            None => break,
        }
    }
    depth
}

/// A rider (ADR-0024): an untracked window owned by a flow window that took
/// the foreground. It gets no registry entry, seat, border or learned rule;
/// the layer only holds it in the topmost band with the members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rider {
    /// The first tracked window on its owner chain ([`anchor_of`]).
    pub anchor: isize,
    /// Held in the topmost band by the layer (a member's `on_top`).
    pub on_top: bool,
    /// Topmost on its own when it entered: topmost then and no owner on
    /// its chain was held by the layer (a flag inherited from the daemon's
    /// own raise is not the window's).
    pub was_topmost: bool,
}

/// Who the layer sees holding the foreground.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ForegroundOwner {
    /// A shell surface, or no window at all: the last decision stands.
    Shell,
    /// A tile, a float or an explicit-rule ignore that is not fullscreen on
    /// the desktop: flow owns the foreground, members are raised.
    Flow,
    /// A window fullscreen by live geometry (tracked or not), an untracked
    /// window, or a registry window in any other `Ignored` state: members
    /// are dropped under it.
    Foreign,
}

impl ForegroundOwner {
    /// The decision: raised (`Some(true)`), dropped (`Some(false)`) or kept
    /// (`None`).
    #[must_use]
    pub(super) fn raised(self) -> Option<bool> {
        match self {
            Self::Shell => None,
            Self::Flow => Some(true),
            Self::Foreign => Some(false),
        }
    }
}

/// Whether a registry window in `state` is one flow owns when it holds the
/// foreground: an active tile, an active float or an explicit-rule ignore
/// (a tile or a layer member). `Ignored(Maximized | Fullscreen)` and the
/// minimized / hidden states are not.
#[must_use]
pub(super) fn owns_foreground(state: &WindowState) -> bool {
    matches!(
        state,
        WindowState::Tiling(TilingState::Active { .. })
            | WindowState::Floating(FloatingState::Active { .. })
            | WindowState::Ignored(IgnoredReason::ExplicitRule)
    )
}

/// Pure decision rule (ADR-0003): classify the foreground window from its
/// class (`None`: the handle is gone), its registry state (`None`:
/// untracked) and the live fullscreen check. Shell first, so a shell
/// surface never flips the layer; then fullscreen, so a tile that just went
/// fullscreen (F11) drops the layer even though its state still says
/// tiled.
#[must_use]
pub(super) fn classify_foreground(
    class: Option<&str>,
    state: Option<&WindowState>,
    fullscreen_live: bool,
) -> ForegroundOwner {
    match class {
        None => return ForegroundOwner::Shell,
        Some(c) if is_shell_class(c) => return ForegroundOwner::Shell,
        Some(_) => {}
    }
    if fullscreen_live {
        return ForegroundOwner::Foreign;
    }
    if state.is_some_and(owns_foreground) {
        ForegroundOwner::Flow
    } else {
        ForegroundOwner::Foreign
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
/// on, tiling is on, the layer is raised (flow owns the foreground), the
/// window is not one the daemon put fullscreen (that code owns its topmost
/// flag), and its state is a member state.
#[must_use]
pub(super) fn layer_wanted(
    always_on_top: bool,
    tiling_on: bool,
    raised: bool,
    fullscreen: bool,
    state: &WindowState,
) -> bool {
    always_on_top && tiling_on && raised && !fullscreen && in_top_layer(state)
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
        // Start / search surfaces an explicit rule ignores are never members
        // (they would get `HWND_TOPMOST` on every raise).
        if is_shell_class(&window.class) {
            return Some(false);
        }
        Some(layer_wanted(
            self.config.floating.always_on_top,
            !self.tiling_paused,
            self.top_layer_raised,
            self.is_fullscreen(hwnd),
            &window.state,
        ))
    }

    /// The layer's state for `flow ping`: `"raised"` / `"dropped"` while
    /// the option and tiling are on, `None` otherwise.
    #[must_use]
    pub(super) fn top_layer_status(&self) -> Option<&'static str> {
        (self.config.floating.always_on_top && !self.tiling_paused).then_some(
            if self.top_layer_raised {
                "raised"
            } else {
                "dropped"
            },
        )
    }

    /// Remember `hwnd` as the foreground the focus sink last saw — for the
    /// layer's re-decisions from paths without a foreground event of their
    /// own, and for the hook callback, which forwards this window's
    /// `LOCATIONCHANGE` (an F11 is a resize).
    pub(super) fn note_foreground(&mut self, hwnd: isize) {
        self.foreground = Some(hwnd);
        crate::registry::hooks::set_foreground_hwnd(hwnd);
    }

    /// [`classify_foreground`] over the daemon's view of `fg`, and the
    /// anchor it was classed by: a tracked window's class and state come
    /// from its record (no Win32); an untracked one costs a class read and,
    /// unless it is a shell surface, an owner walk ([`anchor_of`]) — owned
    /// by a flow window, it is classed by that window's state (ADR-0024).
    /// The live fullscreen check is only asked for a window flow would
    /// otherwise own (for a rider: the rider and its anchor).
    fn foreground_view_with<P: TopmostProbe>(
        &self,
        fg: Option<isize>,
        probe: &P,
    ) -> (ForegroundOwner, Option<isize>) {
        let Some(hwnd) = fg.filter(|h| *h != 0) else {
            return (ForegroundOwner::Shell, None);
        };
        if let Some(w) = self.registry.get_window(HWND(hwnd as *mut _)) {
            let fullscreen_live = owns_foreground(&w.state) && probe.is_fullscreen_live(hwnd);
            return (
                classify_foreground(Some(&w.class), Some(&w.state), fullscreen_live),
                None,
            );
        }
        let class = probe.class_name(hwnd);
        let anchor = class
            .as_deref()
            .filter(|c| !is_shell_class(c))
            .and_then(|_| anchor_of(hwnd, |h| probe.owner_of(h), |h| self.registry.is_tracked(h)));
        let state = anchor
            .and_then(|a| self.registry.get_window(HWND(a as *mut _)))
            .map(|w| &w.state);
        let fullscreen_live = state.is_some_and(owns_foreground)
            && (probe.is_fullscreen_live(hwnd)
                || anchor.is_some_and(|a| probe.is_fullscreen_live(a)));
        (
            classify_foreground(class.as_deref(), state, fullscreen_live),
            anchor,
        )
    }

    /// Decide the layer's state for foreground `fg` (ADR-0003): raised when
    /// flow owns it, dropped when a fullscreen or non-flow window has it,
    /// kept for a shell surface. Records the decision (and the window to
    /// drop under) without touching any window; `Some(raised)` when the
    /// decision flipped, so the caller applies it to every window. Nothing
    /// while the option or tiling is off.
    pub(super) fn decide_top_layer_with<P: TopmostProbe>(
        &mut self,
        fg: Option<isize>,
        probe: &P,
    ) -> Option<bool> {
        if !self.config.floating.always_on_top || self.tiling_paused {
            return None;
        }
        let (owner, anchor) = self.foreground_view_with(fg, probe);
        if owner == ForegroundOwner::Flow
            && let (Some(hwnd), Some(anchor)) = (fg, anchor)
            && probe.is_visible(hwnd)
        {
            self.note_rider(hwnd, anchor);
        }
        let raise = owner.raised()?;
        self.top_layer_below = if raise { None } else { fg };
        if raise == self.top_layer_raised {
            return None;
        }
        self.top_layer_raised = raise;
        log::info!(
            "top layer: {} (foreground {:#x} is {owner:?})",
            if raise { "raised" } else { "dropped" },
            fg.unwrap_or(0)
        );
        Some(raise)
    }

    /// Decide for `fg` and, when the decision flipped, apply it to every
    /// window. The focus sink, the foreground window's resize / state
    /// change and the periodic pass all end here. Returns the new decision
    /// when it flipped.
    pub(super) fn evaluate_top_layer(&mut self, fg: Option<isize>) -> Option<bool> {
        self.evaluate_top_layer_with(fg, &Win32Topmost)
    }

    /// [`evaluate_top_layer`](Self::evaluate_top_layer) against `probe`.
    pub(super) fn evaluate_top_layer_with<P: TopmostProbe>(
        &mut self,
        fg: Option<isize>,
        probe: &P,
    ) -> Option<bool> {
        let raise = self.decide_top_layer_with(fg, probe)?;
        let summary = self.apply_all_top_layer_with(probe);
        log::debug!(
            "top layer: {}: {} entered, {} left, {} re-asserted",
            if raise { "raised" } else { "dropped" },
            summary.entered,
            summary.left,
            summary.reasserted
        );
        Some(raise)
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
        if let Some(wanted) = self.layer_wanted_for(hwnd) {
            return self.apply_layer_step(hwnd, wanted, probe);
        }
        let wanted = self.rider_wanted_with(hwnd, probe)?;
        self.apply_rider_step(hwnd, wanted, probe)
    }

    /// Remember `hwnd` as a rider of `anchor` (the foreground is classed by
    /// it, ADR-0024). Keeps the flags of a rider already known.
    fn note_rider(&mut self, hwnd: isize, anchor: isize) {
        let rider = self.riders.entry(hwnd).or_insert(Rider {
            anchor,
            on_top: false,
            was_topmost: false,
        });
        rider.anchor = anchor;
    }

    /// Whether the rider `hwnd` is wanted in the topmost band: the layer is
    /// on and raised, its anchor is still tracked and it is visible. `None`
    /// for a window that is not a rider.
    fn rider_wanted_with<P: TopmostProbe>(&self, hwnd: isize, probe: &P) -> Option<bool> {
        let rider = self.riders.get(&hwnd)?;
        Some(
            self.config.floating.always_on_top
                && !self.tiling_paused
                && self.top_layer_raised
                && self.registry.is_tracked(rider.anchor)
                && probe.is_visible(hwnd),
        )
    }

    /// Whether a window on `hwnd`'s owner chain is held in the band by the
    /// layer (a member or a rider): a topmost flag `hwnd` has now may be
    /// inherited from the daemon's own raise.
    fn owner_held<P: TopmostProbe>(&self, hwnd: isize, probe: &P) -> bool {
        anchor_of(
            hwnd,
            |h| probe.owner_of(h),
            |h| {
                self.riders.get(&h).is_some_and(|r| r.on_top)
                    || self
                        .registry
                        .get_window(HWND(h as *mut _))
                        .is_some_and(|w| w.on_top)
            },
        )
        .is_some()
    }

    /// The window to seat `hwnd` under when it leaves because the layer
    /// dropped: the dropping foreground, unless that window is owned by
    /// `hwnd` (an owner is never placed under its own popup).
    fn seat_below<P: TopmostProbe>(
        &self,
        hwnd: isize,
        restore_to: Option<bool>,
        probe: &P,
    ) -> Option<isize> {
        (restore_to == Some(false) && !self.top_layer_raised)
            .then_some(self.top_layer_below)
            .flatten()
            .filter(|after| !owned_by(*after, hwnd, |h| probe.owner_of(h)))
    }

    /// [`apply_layer_step`](Self::apply_layer_step) for a rider: an entering
    /// rider is always raised (`HWND_TOPMOST` puts it at the top of the
    /// band, above members raised before it, even when it inherited the
    /// flag), and the flag it had on its own is the one put back.
    fn apply_rider_step<P: TopmostProbe>(
        &mut self,
        hwnd: isize,
        wanted: bool,
        probe: &P,
    ) -> Option<LayerStep> {
        let rider = *self.riders.get(&hwnd)?;
        if !wanted && !rider.on_top {
            return None;
        }
        let topmost_now = probe.is_topmost(hwnd);
        let step = if wanted && !rider.on_top {
            LayerStep::Enter { raise: true }
        } else {
            layer_step(wanted, rider.on_top, topmost_now, rider.was_topmost)?
        };
        match step {
            LayerStep::Enter { .. } => {
                let was_topmost = topmost_now && !self.owner_held(hwnd, probe);
                if !probe.set_topmost(hwnd, true) {
                    log::debug!("top layer: could not raise rider {hwnd:#x}");
                }
                if let Some(r) = self.riders.get_mut(&hwnd) {
                    r.on_top = true;
                    r.was_topmost = was_topmost;
                }
                log::info!(
                    "top layer: rider {hwnd:#x} (anchor {:#x}) entered (was_topmost={was_topmost})",
                    rider.anchor
                );
            }
            LayerStep::Leave { restore_to } => {
                if let Some(after) = self.seat_below(hwnd, restore_to, probe)
                    && probe.insert_below(hwnd, after)
                {
                    log::debug!("top layer: rider {hwnd:#x} seated under {after:#x}");
                } else if let Some(flag) = restore_to
                    && !probe.set_topmost(hwnd, flag)
                {
                    log::debug!("top layer: could not restore topmost={flag} on rider {hwnd:#x}");
                }
                if let Some(r) = self.riders.get_mut(&hwnd) {
                    r.on_top = false;
                    r.was_topmost = false;
                }
                log::info!(
                    "top layer: rider {hwnd:#x} (anchor {:#x}) left (restored to {restore_to:?})",
                    rider.anchor
                );
            }
            LayerStep::Reassert => {
                if !probe.set_topmost(hwnd, true) {
                    log::debug!("top layer: could not re-assert rider {hwnd:#x}");
                }
                log::debug!("top layer: rider {hwnd:#x} re-asserted");
            }
        }
        Some(step)
    }

    /// The riders in the order they are raised: owner first (an owned
    /// dialog above its owner), the foreground last.
    fn rider_order<P: TopmostProbe>(&self, probe: &P) -> Vec<isize> {
        let mut riders: Vec<(bool, usize, isize)> = self
            .riders
            .keys()
            .map(|h| {
                (
                    Some(*h) == self.foreground,
                    owner_depth(*h, |o| probe.owner_of(o)),
                    *h,
                )
            })
            .collect();
        riders.sort_unstable();
        riders.into_iter().map(|(_, _, h)| h).collect()
    }

    /// Drop the riders that are gone (no class), hidden, or whose anchor is
    /// no longer tracked, putting back the flag of one the layer holds
    /// (owned windows first). Run by every pass. Returns how many held
    /// riders left.
    fn purge_riders_with<P: TopmostProbe>(&mut self, probe: &P) -> usize {
        let mut stale: Vec<(Reverse<usize>, isize)> = self
            .riders
            .iter()
            .filter(|(h, r)| {
                probe.class_name(**h).is_none()
                    || !probe.is_visible(**h)
                    || !self.registry.is_tracked(r.anchor)
            })
            .map(|(h, _)| (Reverse(owner_depth(*h, |o| probe.owner_of(o))), *h))
            .collect();
        stale.sort_unstable();
        let mut left = 0;
        for (_, hwnd) in stale {
            if probe.class_name(hwnd).is_some()
                && matches!(
                    self.apply_rider_step(hwnd, false, probe),
                    Some(LayerStep::Leave { .. })
                )
            {
                left += 1;
            }
            if let Some(r) = self.riders.remove(&hwnd) {
                log::info!(
                    "top layer: rider {hwnd:#x} (anchor {:#x}) forgotten",
                    r.anchor
                );
            }
        }
        left
    }

    /// A rider was hidden or destroyed: out of the band (when it is still
    /// there to be touched) and forgotten. One map lookup for any other
    /// window.
    pub(super) fn forget_rider(&mut self, hwnd: isize, destroyed: bool) {
        self.forget_rider_with(hwnd, destroyed, &Win32Topmost);
    }

    /// [`forget_rider`](Self::forget_rider) against `probe`.
    pub(super) fn forget_rider_with<P: TopmostProbe>(
        &mut self,
        hwnd: isize,
        destroyed: bool,
        probe: &P,
    ) {
        if !self.riders.contains_key(&hwnd) {
            return;
        }
        if !destroyed {
            let _ = self.apply_rider_step(hwnd, false, probe);
        }
        if let Some(r) = self.riders.remove(&hwnd) {
            log::info!(
                "top layer: rider {hwnd:#x} (anchor {:#x}) forgotten ({})",
                r.anchor,
                if destroyed { "destroyed" } else { "hidden" }
            );
        }
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
                // Leaving because the layer dropped: straight under the
                // foreground that dropped it (one call clears the topmost
                // flag too); otherwise the flag it had goes back.
                if let Some(after) = self.seat_below(hwnd, restore_to, probe)
                    && probe.insert_below(hwnd, after)
                {
                    log::debug!("top layer: hwnd {hwnd:#x} seated under {after:#x}");
                } else if let Some(flag) = restore_to
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

    /// Sync every tracked window ([`sync_top_layer`](Self::sync_top_layer))
    /// after re-deciding against the last foreground the sink saw: startup,
    /// `tiling on|off`, config reload, loadout load, a display change.
    /// O(tracked windows); the Win32 reads happen only for members and
    /// candidates.
    pub(super) fn sync_all_top_layer(&mut self) -> LayerSummary {
        self.sync_all_top_layer_with(&Win32Topmost)
    }

    /// [`sync_all_top_layer`](Self::sync_all_top_layer) against `probe`.
    pub(super) fn sync_all_top_layer_with<P: TopmostProbe>(&mut self, probe: &P) -> LayerSummary {
        let _ = self.decide_top_layer_with(self.foreground, probe);
        self.apply_all_top_layer_with(probe)
    }

    /// Apply the current decision to every tracked window and every rider:
    /// raising, members first and riders after them (the foreground window
    /// last); leaving, riders first.
    fn apply_all_top_layer_with<P: TopmostProbe>(&mut self, probe: &P) -> LayerSummary {
        let mut summary = LayerSummary {
            left: self.purge_riders_with(probe),
            ..LayerSummary::default()
        };
        let members: Vec<isize> = self.registry.windows().map(|w| w.hwnd.0 as isize).collect();
        let riders = self.rider_order(probe);
        let lifting =
            self.config.floating.always_on_top && !self.tiling_paused && self.top_layer_raised;
        let order: Vec<isize> = if lifting {
            // The foreground (a member or a rider) last: it ends on top.
            let fg = self.foreground;
            let (mut order, last): (Vec<isize>, Vec<isize>) = members
                .into_iter()
                .chain(riders)
                .partition(|h| Some(*h) != fg);
            order.extend(last);
            order
        } else {
            riders.into_iter().rev().chain(members).collect()
        };
        for hwnd in order {
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
        let mut left = 0;
        // Riders first (owned windows before their owners), then members.
        for hwnd in self.rider_order(probe).into_iter().rev() {
            if matches!(
                self.apply_rider_step(hwnd, false, probe),
                Some(LayerStep::Leave { .. })
            ) {
                left += 1;
            }
        }
        self.riders.clear();
        let held: Vec<isize> = self
            .registry
            .windows()
            .filter(|w| w.on_top)
            .map(|w| w.hwnd.0 as isize)
            .collect();
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
    /// `[focus] foreground_sync_interval_ms`): re-decide against the
    /// desktop's real foreground (`GetForegroundWindow`, so drift is
    /// corrected both ways — a dropped foreground event, a window that went
    /// fullscreen without an event), then raise again any member that
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
        let fg = registry_win32::get_foreground_window();
        self.reassert_top_layer_with(fg, &Win32Topmost);
    }

    /// [`reassert_top_layer`](Self::reassert_top_layer) against `probe`,
    /// for foreground `fg`, without the interval gate.
    pub(super) fn reassert_top_layer_with<P: TopmostProbe>(
        &mut self,
        fg: Option<isize>,
        probe: &P,
    ) {
        self.purge_riders_with(probe);
        if self.evaluate_top_layer_with(fg, probe).is_some() || !self.top_layer_raised {
            // Just re-applied everywhere, or nothing is held while dropped.
            return;
        }
        let held: Vec<isize> = self
            .registry
            .windows()
            .filter(|w| w.on_top)
            .map(|w| w.hwnd.0 as isize)
            .collect();
        for hwnd in held {
            self.sync_top_layer_with(hwnd, probe);
        }
        // Riders after the members: a demoted one is raised again, and a
        // foreground rider whose own event was missed enters.
        for hwnd in self.rider_order(probe) {
            self.sync_top_layer_with(hwnd, probe);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};

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

    /// A scripted desktop: which handles are topmost, which are fullscreen
    /// by live geometry, the class of the untracked windows, every
    /// `SetWindowPos(TOPMOST|NOTOPMOST)` issued in order, and every
    /// insert-below.
    #[derive(Default)]
    struct MockTopmost {
        topmost: RefCell<HashSet<isize>>,
        fullscreen: RefCell<HashSet<isize>>,
        classes: RefCell<HashMap<isize, String>>,
        calls: RefCell<Vec<(isize, bool)>>,
        below: RefCell<Vec<(isize, isize)>>,
        owners: RefCell<HashMap<isize, isize>>,
        hidden: RefCell<HashSet<isize>>,
    }

    impl MockTopmost {
        fn with_topmost(hwnds: &[isize]) -> Self {
            let m = Self::default();
            m.topmost.borrow_mut().extend(hwnds.iter().copied());
            m
        }

        /// An untracked window of class `class` on the desktop.
        fn with_window(self, hwnd: isize, class: &str) -> Self {
            self.classes.borrow_mut().insert(hwnd, class.to_owned());
            self
        }

        /// An untracked window of class `class` owned by `owner`.
        fn with_owned(self, hwnd: isize, class: &str, owner: isize) -> Self {
            self.owners.borrow_mut().insert(hwnd, owner);
            self.with_window(hwnd, class)
        }

        fn set_fullscreen(&self, hwnd: isize, fullscreen: bool) {
            if fullscreen {
                self.fullscreen.borrow_mut().insert(hwnd);
            } else {
                self.fullscreen.borrow_mut().remove(&hwnd);
            }
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

        fn insert_below(&self, hwnd: isize, after: isize) -> bool {
            self.below.borrow_mut().push((hwnd, after));
            self.topmost.borrow_mut().remove(&hwnd);
            true
        }

        fn is_fullscreen_live(&self, hwnd: isize) -> bool {
            self.fullscreen.borrow().contains(&hwnd)
        }

        fn class_name(&self, hwnd: isize) -> Option<String> {
            self.classes.borrow().get(&hwnd).cloned()
        }

        fn owner_of(&self, hwnd: isize) -> Option<isize> {
            self.owners.borrow().get(&hwnd).copied()
        }

        fn is_visible(&self, hwnd: isize) -> bool {
            !self.hidden.borrow().contains(&hwnd)
        }
    }

    /// An untracked game / video window and a taskbar on the desktop.
    const G: isize = 0x6;
    const T: isize = 0x7;

    fn desktop() -> MockTopmost {
        MockTopmost::default()
            .with_window(G, "UnityWndClass")
            .with_window(T, "Shell_TrayWnd")
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
    fn wanted_needs_the_option_tiling_raised_and_no_fullscreen() {
        assert!(layer_wanted(true, true, true, false, &float()));
        assert!(layer_wanted(true, true, true, false, &ignored_rule()));
        assert!(
            !layer_wanted(false, true, true, false, &float()),
            "option off"
        );
        assert!(
            !layer_wanted(true, false, true, false, &float()),
            "tiling off"
        );
        assert!(
            !layer_wanted(true, true, false, false, &float()),
            "dropped: a non-flow window has the foreground"
        );
        assert!(
            !layer_wanted(true, true, true, true, &float()),
            "the fullscreen code owns its topmost flag"
        );
        assert!(!layer_wanted(true, true, true, false, &tile()));
    }

    #[test]
    fn foreground_rule_shell_keeps_flow_raises_everything_else_drops() {
        use ForegroundOwner::{Flow, Foreign, Shell};
        // A shell surface or a vanished handle: no decision.
        assert_eq!(classify_foreground(None, None, false), Shell);
        for c in SHELL_CLASSES {
            assert_eq!(classify_foreground(Some(c), None, false), Shell, "{c}");
            assert_eq!(classify_foreground(Some(c), Some(&tile()), true), Shell);
        }
        // Flow owns a tile, a float, an explicit-rule ignore.
        assert_eq!(classify_foreground(Some("App"), Some(&tile()), false), Flow);
        assert_eq!(
            classify_foreground(Some("App"), Some(&float()), false),
            Flow
        );
        assert_eq!(
            classify_foreground(Some("App"), Some(&ignored_rule()), false),
            Flow
        );
        // Fullscreen by live geometry wins over the stored state (F11).
        assert_eq!(
            classify_foreground(Some("App"), Some(&tile()), true),
            Foreign
        );
        assert_eq!(classify_foreground(Some("App"), None, true), Foreign);
        // Untracked, or ignored for any other reason.
        assert_eq!(classify_foreground(Some("App"), None, false), Foreign);
        assert_eq!(
            classify_foreground(
                Some("App"),
                Some(&WindowState::Ignored(IgnoredReason::Maximized)),
                false
            ),
            Foreign
        );
        assert_eq!(
            classify_foreground(
                Some("App"),
                Some(&WindowState::Ignored(IgnoredReason::Fullscreen)),
                false
            ),
            Foreign
        );
        assert_eq!(
            classify_foreground(
                Some("App"),
                Some(&WindowState::Tiling(TilingState::Minimized)),
                false
            ),
            Foreign
        );
        assert_eq!(Shell.raised(), None);
        assert_eq!(Flow.raised(), Some(true));
        assert_eq!(Foreign.raised(), Some(false));
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
        // The periodic pass only walks members (no foreground: the decision
        // stands); the interval gates the production wrapper.
        probe.topmost.borrow_mut().remove(&F);
        flow.reassert_top_layer_with(None, &probe);
        assert!(on_top(&flow, F) && probe.is_topmost(F));
        assert_eq!(probe.calls.borrow().len(), 3);
        flow.last_top_layer_reassert = Instant::now() - Duration::from_secs(1);
        // (The production pass uses Win32; the bookkeeping stays held —
        // the test host's foreground is not a flow window, so the real
        // pass is not run here.)
    }

    // ── the foreground decision (ADR-0003) ────────────────────────────

    #[test]
    fn foreground_round_trip_drops_under_a_foreign_window_and_raises_for_a_tile() {
        let mut flow = fixture();
        let probe = desktop();
        flow.sync_all_top_layer_with(&probe);
        assert!(on_top(&flow, F) && on_top(&flow, I));
        assert_eq!(flow.top_layer_status(), Some("raised"));
        // A tile takes the foreground: nothing changes.
        assert_eq!(flow.evaluate_top_layer_with(Some(A), &probe), None);
        assert!(flow.top_layer_raised);
        // An untracked window takes it: dropped, members seated under it.
        assert_eq!(flow.evaluate_top_layer_with(Some(G), &probe), Some(false));
        assert!(!flow.top_layer_raised);
        assert_eq!(flow.top_layer_status(), Some("dropped"));
        assert!(!on_top(&flow, F) && !on_top(&flow, I));
        assert!(!probe.is_topmost(F) && !probe.is_topmost(I));
        let mut below = probe.below.borrow().clone();
        below.sort_unstable();
        assert_eq!(below, vec![(I, G), (F, G)]);
        assert_eq!(
            probe.calls.borrow().len(),
            2,
            "no NOTOPMOST call: the seat did it"
        );
        // Already dropped: a second foreign foreground changes nothing.
        assert_eq!(flow.evaluate_top_layer_with(Some(G), &probe), None);
        // A float toggled while dropped does not enter until the raise.
        set_state(&mut flow, A, float());
        assert_eq!(flow.sync_top_layer_with(A, &probe), None);
        set_state(&mut flow, A, tile());
        // The tile takes the foreground again: raised.
        assert_eq!(flow.evaluate_top_layer_with(Some(A), &probe), Some(true));
        assert!(on_top(&flow, F) && on_top(&flow, I));
        assert!(probe.is_topmost(F) && probe.is_topmost(I));
        assert!(!was_topmost(&flow, F), "raised by the daemon again");
        assert_eq!(probe.calls.borrow().len(), 4);
        // A float as the foreground keeps it raised; so does the ignore.
        assert_eq!(flow.evaluate_top_layer_with(Some(F), &probe), None);
        assert_eq!(flow.evaluate_top_layer_with(Some(I), &probe), None);
    }

    #[test]
    fn f11_on_the_foreground_tile_drops_and_its_chrome_coming_back_raises() {
        let mut flow = fixture();
        let probe = desktop();
        flow.sync_all_top_layer_with(&probe);
        assert_eq!(flow.evaluate_top_layer_with(Some(A), &probe), None);
        // The tile resizes to its monitor's full rect without chrome: the
        // registry still says tiled, the live check says fullscreen.
        probe.set_fullscreen(A, true);
        assert_eq!(flow.evaluate_top_layer_with(Some(A), &probe), Some(false));
        assert!(!on_top(&flow, F) && !probe.is_topmost(F));
        assert!(
            probe.below.borrow().contains(&(F, A)),
            "seated under the tile"
        );
        // Chrome back, ordinary rect: raised again.
        probe.set_fullscreen(A, false);
        assert_eq!(flow.evaluate_top_layer_with(Some(A), &probe), Some(true));
        assert!(on_top(&flow, F) && probe.is_topmost(F));
        // The same for a float that goes fullscreen on its own.
        probe.set_fullscreen(F, true);
        assert_eq!(flow.evaluate_top_layer_with(Some(F), &probe), Some(false));
        assert!(!on_top(&flow, I));
        probe.set_fullscreen(F, false);
        assert_eq!(flow.evaluate_top_layer_with(Some(F), &probe), Some(true));
    }

    #[test]
    fn a_shell_surface_or_no_foreground_leaves_the_decision_alone() {
        let mut flow = fixture();
        let probe = desktop();
        flow.sync_all_top_layer_with(&probe);
        // Raised: the taskbar, Start, nothing at all — still raised.
        assert_eq!(flow.evaluate_top_layer_with(Some(T), &probe), None);
        assert_eq!(flow.evaluate_top_layer_with(None, &probe), None);
        assert_eq!(flow.evaluate_top_layer_with(Some(0), &probe), None);
        assert!(flow.top_layer_raised && on_top(&flow, F));
        // Dropped: the same surfaces keep it dropped.
        assert_eq!(flow.evaluate_top_layer_with(Some(G), &probe), Some(false));
        assert_eq!(flow.evaluate_top_layer_with(Some(T), &probe), None);
        assert_eq!(flow.evaluate_top_layer_with(None, &probe), None);
        assert!(!flow.top_layer_raised && !on_top(&flow, F));
        // A handle that vanished before the sink ran: kept too.
        assert_eq!(flow.evaluate_top_layer_with(Some(0x99), &probe), None);
        assert!(!flow.top_layer_raised);
    }

    #[test]
    fn an_os_ignored_registry_window_in_the_foreground_drops() {
        let mut flow = fixture();
        let probe = desktop();
        flow.sync_all_top_layer_with(&probe);
        // M is Ignored(Maximized): flow does not own it.
        assert_eq!(flow.evaluate_top_layer_with(Some(M), &probe), Some(false));
        assert!(!on_top(&flow, F));
        assert!(probe.below.borrow().contains(&(F, M)), "seated under M");
        // Recovered as a tile (un-maximized): raised.
        set_state(&mut flow, M, tile());
        assert_eq!(flow.evaluate_top_layer_with(Some(M), &probe), Some(true));
        assert!(on_top(&flow, F));
    }

    #[test]
    fn a_member_topmost_on_its_own_keeps_its_flag_across_a_drop() {
        let mut flow = fixture();
        let probe = desktop();
        probe.topmost.borrow_mut().insert(I);
        flow.sync_all_top_layer_with(&probe);
        assert!(was_topmost(&flow, I));
        assert_eq!(flow.evaluate_top_layer_with(Some(G), &probe), Some(false));
        assert!(
            probe.is_topmost(I),
            "the daemon never clears a flag it did not set"
        );
        assert!(!probe.is_topmost(F));
        assert!(
            probe.below.borrow().iter().all(|(h, _)| *h != I),
            "I is not seated under G"
        );
        assert_eq!(flow.evaluate_top_layer_with(Some(A), &probe), Some(true));
        assert!(on_top(&flow, I) && was_topmost(&flow, I));
        assert!(on_top(&flow, F) && !was_topmost(&flow, F));
    }

    #[test]
    fn the_periodic_pass_corrects_drift_both_ways() {
        let mut flow = fixture();
        let probe = desktop();
        flow.sync_all_top_layer_with(&probe);
        // The foreground event for the game was dropped: the pass sees it.
        flow.reassert_top_layer_with(Some(G), &probe);
        assert!(!flow.top_layer_raised && !on_top(&flow, F));
        // And the way back, plus a demoted member on the next pass.
        flow.reassert_top_layer_with(Some(A), &probe);
        assert!(flow.top_layer_raised && on_top(&flow, F) && probe.is_topmost(F));
        probe.topmost.borrow_mut().remove(&F);
        flow.reassert_top_layer_with(Some(A), &probe);
        assert!(probe.is_topmost(F), "re-asserted");
        // While dropped, the pass walks nothing.
        flow.reassert_top_layer_with(Some(G), &probe);
        let n = probe.calls.borrow().len();
        flow.reassert_top_layer_with(Some(T), &probe);
        assert_eq!(probe.calls.borrow().len(), n);
    }

    #[test]
    fn tiling_off_and_on_and_the_stop_path_while_dropped() {
        let mut flow = fixture();
        let probe = desktop();
        flow.sync_all_top_layer_with(&probe);
        flow.note_foreground(G);
        assert_eq!(flow.evaluate_top_layer_with(Some(G), &probe), Some(false));
        // Tiling off: nothing held, nothing to release; no status.
        flow.tiling_paused = true;
        assert_eq!(
            flow.sync_all_top_layer_with(&probe),
            LayerSummary::default()
        );
        assert_eq!(flow.top_layer_status(), None);
        assert_eq!(flow.release_top_layer_with(&probe), 0);
        // Tiling on with the game still in front: stays dropped.
        flow.tiling_paused = false;
        assert_eq!(flow.sync_all_top_layer_with(&probe).entered, 0);
        assert_eq!(flow.top_layer_status(), Some("dropped"));
        // Tiling on after the user went back to a tile (the sink saw it
        // while paused): raised by the resume's own sync.
        flow.note_foreground(A);
        assert_eq!(flow.sync_all_top_layer_with(&probe).entered, 2);
        assert_eq!(flow.top_layer_status(), Some("raised"));
        // Option off: nothing is decided or reported.
        flow.config.floating.always_on_top = false;
        flow.sync_all_top_layer_with(&probe);
        assert_eq!(flow.evaluate_top_layer_with(Some(G), &probe), None);
        assert_eq!(flow.top_layer_status(), None);
        // Stop while raised releases as before.
        flow.config.floating.always_on_top = true;
        flow.sync_all_top_layer_with(&probe);
        assert_eq!(flow.release_top_layer_with(&probe), 2);
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

    // ── riders: owned windows of flow windows (ADR-0024) ──────────────

    /// A connect dialog owned by float F, an error box owned by the dialog,
    /// an error box owned by tile A, a popup owned by a hidden untracked
    /// helper, and an owner cycle.
    const CONNECT: isize = 0x20;
    const ERROR: isize = 0x21;
    const TILE_ERR: isize = 0x22;
    const HELPER: isize = 0x23;
    const ORPHAN: isize = 0x24;
    const LOOP_A: isize = 0x25;
    const LOOP_B: isize = 0x26;

    fn owned_desktop() -> MockTopmost {
        let probe = desktop()
            .with_owned(CONNECT, "#32770", F)
            .with_owned(ERROR, "#32770", CONNECT)
            .with_owned(TILE_ERR, "#32770", A)
            .with_window(HELPER, "HelperClass")
            .with_owned(ORPHAN, "WindowsForms10.Window", HELPER)
            .with_owned(LOOP_A, "#32770", LOOP_B)
            .with_owned(LOOP_B, "#32770", LOOP_A);
        probe.hidden.borrow_mut().insert(HELPER);
        probe
    }

    fn rider(flow: &FlowWM, hwnd: isize) -> Option<Rider> {
        flow.riders.get(&hwnd).copied()
    }

    /// `hwnd` takes the foreground: the focus sink's two layer calls.
    fn activate(flow: &mut FlowWM, hwnd: isize, probe: &MockTopmost) -> Option<bool> {
        flow.note_foreground(hwnd);
        let flipped = flow.evaluate_top_layer_with(Some(hwnd), probe);
        if flipped.is_none() {
            flow.sync_top_layer_with(hwnd, probe);
        }
        flipped
    }

    #[test]
    fn the_anchor_is_the_first_tracked_owner_with_a_hop_cap_and_a_cycle_guard() {
        let owners: HashMap<isize, isize> = (1..=20).map(|h| (h, h + 1)).collect();
        let owner_of = |h: isize| owners.get(&h).copied();
        assert_eq!(anchor_of(1, owner_of, |h| h == 5), Some(5));
        assert_eq!(anchor_of(1, owner_of, |h| h == 1), None, "not itself");
        let last = 1 + OWNER_HOPS as isize;
        assert_eq!(anchor_of(1, owner_of, |h| h == last), Some(last));
        assert_eq!(anchor_of(1, owner_of, |h| h == last + 1), None);
        assert_eq!(anchor_of(30, owner_of, |_| true), None, "unowned");
        let cycle = |h: isize| Some(if h == 1 { 2 } else { 1 });
        assert_eq!(anchor_of(1, cycle, |h| h == 7), None);
        assert!(owned_by(1, 4, owner_of));
        assert!(!owned_by(4, 1, owner_of));
        assert_eq!(owner_depth(5, owner_of), OWNER_HOPS);
        assert_eq!(owner_depth(20, owner_of), 1);
        assert_eq!(owner_depth(30, owner_of), 0);
    }

    #[test]
    fn a_dialog_of_a_float_is_flow_and_rides_above_the_members() {
        let mut flow = fixture();
        let probe = owned_desktop();
        flow.sync_all_top_layer_with(&probe);
        // The connect dialog takes the foreground: no drop, it enters.
        assert_eq!(activate(&mut flow, CONNECT, &probe), None);
        assert!(flow.top_layer_raised);
        assert_eq!(rider(&flow, CONNECT).map(|r| r.anchor), Some(F));
        assert!(probe.is_topmost(CONNECT) && on_top(&flow, F));
        // The error box owned by the dialog: anchored at F through it.
        assert_eq!(activate(&mut flow, ERROR, &probe), None);
        assert_eq!(rider(&flow, ERROR).map(|r| r.anchor), Some(F));
        assert!(probe.is_topmost(ERROR));
        assert!(probe.below.borrow().is_empty(), "nothing was seated");
        // A foreign window drops everything; a tile raises it again with
        // the members first and the riders after them, owner first and the
        // foreground rider last.
        assert_eq!(activate(&mut flow, G, &probe), Some(false));
        assert!(!probe.is_topmost(ERROR) && !probe.is_topmost(CONNECT));
        probe.calls.borrow_mut().clear();
        flow.note_foreground(ERROR);
        assert_eq!(flow.evaluate_top_layer_with(Some(A), &probe), Some(true));
        let calls = probe.calls.borrow().clone();
        let pos = |h: isize| calls.iter().position(|c| *c == (h, true)).expect("raised");
        assert!(pos(F) < pos(CONNECT) && pos(I) < pos(CONNECT), "{calls:?}");
        assert!(pos(CONNECT) < pos(ERROR), "{calls:?}");
        assert!(rider(&flow, ERROR).is_some_and(|r| r.on_top && !r.was_topmost));
    }

    #[test]
    fn a_raise_puts_the_foreground_member_above_the_riders() {
        let mut flow = fixture();
        let probe = owned_desktop();
        flow.sync_all_top_layer_with(&probe);
        activate(&mut flow, CONNECT, &probe);
        // A message box that took the foreground before it was registered
        // (I, an explicit-rule ignore) dropped the layer; registered, it
        // raises it again and must end above the connect dialog.
        assert_eq!(activate(&mut flow, G, &probe), Some(false));
        probe.calls.borrow_mut().clear();
        flow.note_foreground(I);
        assert_eq!(flow.evaluate_top_layer_with(Some(I), &probe), Some(true));
        let calls = probe.calls.borrow().clone();
        assert_eq!(calls.last(), Some(&(I, true)), "{calls:?}");
        assert!(calls.contains(&(CONNECT, true)));
    }

    #[test]
    fn a_dialog_of_a_tile_is_flow_and_held_topmost() {
        let mut flow = fixture();
        let probe = owned_desktop();
        flow.sync_all_top_layer_with(&probe);
        assert_eq!(activate(&mut flow, TILE_ERR, &probe), None);
        assert!(rider(&flow, TILE_ERR).is_some_and(|r| r.on_top && r.anchor == A));
        assert!(probe.is_topmost(TILE_ERR) && on_top(&flow, F));
        assert!(!on_top(&flow, A), "riders never make the anchor a member");
        // Windows demotes it: the periodic pass raises it again.
        probe.topmost.borrow_mut().remove(&TILE_ERR);
        flow.reassert_top_layer_with(Some(TILE_ERR), &probe);
        assert!(probe.is_topmost(TILE_ERR));
        // Riders never get a registry entry.
        assert!(!flow.registry.is_tracked(TILE_ERR));
    }

    #[test]
    fn a_dialog_of_a_live_fullscreen_anchor_or_an_untracked_owner_is_foreign() {
        let mut flow = fixture();
        let probe = owned_desktop();
        flow.sync_all_top_layer_with(&probe);
        probe.set_fullscreen(A, true);
        assert_eq!(activate(&mut flow, TILE_ERR, &probe), Some(false));
        assert!(rider(&flow, TILE_ERR).is_none(), "not a rider");
        probe.set_fullscreen(A, false);
        assert_eq!(activate(&mut flow, A, &probe), Some(true));
        // A popup owned by a hidden untracked helper (testwin29 -Owned).
        assert_eq!(activate(&mut flow, ORPHAN, &probe), Some(false));
        assert!(flow.riders.is_empty());
        assert_eq!(activate(&mut flow, A, &probe), Some(true));
        // An owner cycle ends the walk: foreign.
        assert_eq!(activate(&mut flow, LOOP_A, &probe), Some(false));
        assert!(flow.riders.is_empty());
    }

    #[test]
    fn a_shell_surface_wins_even_when_owned() {
        let mut flow = fixture();
        let probe = owned_desktop().with_owned(0x30, "XamlExplorerHostIslandWindow", F);
        flow.sync_all_top_layer_with(&probe);
        assert_eq!(activate(&mut flow, 0x30, &probe), None);
        assert!(flow.riders.is_empty());
        assert!(flow.top_layer_raised);
    }

    #[test]
    fn a_drop_demotes_riders_first_and_never_seats_an_owner_under_its_popup() {
        let mut flow = fixture();
        let probe = owned_desktop();
        flow.sync_all_top_layer_with(&probe);
        activate(&mut flow, CONNECT, &probe);
        // A foreign window: the rider is seated before the members.
        assert_eq!(activate(&mut flow, G, &probe), Some(false));
        let below = probe.below.borrow().clone();
        assert_eq!(below.first(), Some(&(CONNECT, G)), "{below:?}");
        assert!(below.contains(&(F, G)) && below.contains(&(I, G)));
        assert!(rider(&flow, CONNECT).is_some_and(|r| !r.on_top));
        // Back up; then F goes fullscreen by live geometry and its own
        // dialog takes the foreground: foreign, and F is not seated under
        // the window it owns (NOTOPMOST instead).
        assert_eq!(activate(&mut flow, A, &probe), Some(true));
        probe.below.borrow_mut().clear();
        probe.calls.borrow_mut().clear();
        probe.set_fullscreen(F, true);
        assert_eq!(activate(&mut flow, CONNECT, &probe), Some(false));
        for (hwnd, after) in probe.below.borrow().iter() {
            assert!(
                !owned_by(*after, *hwnd, |h| probe.owner_of(h)),
                "{hwnd:#x} seated under its own popup {after:#x}"
            );
        }
        assert!(probe.calls.borrow().contains(&(F, false)));
        assert!(!probe.is_topmost(F));
    }

    #[test]
    fn release_and_tiling_off_take_riders_out_and_an_inherited_flag_does_not_survive() {
        let mut flow = fixture();
        let probe = owned_desktop();
        flow.sync_all_top_layer_with(&probe);
        // The dialog inherited topmost from F (raised by the daemon).
        probe.topmost.borrow_mut().insert(CONNECT);
        activate(&mut flow, CONNECT, &probe);
        assert!(rider(&flow, CONNECT).is_some_and(|r| r.on_top && !r.was_topmost));
        // Tiling off: the rider leaves with the members.
        flow.tiling_paused = true;
        let summary = flow.sync_all_top_layer_with(&probe);
        assert_eq!(summary.left, 3);
        assert!(!probe.is_topmost(CONNECT) && !probe.is_topmost(F));
        assert!(rider(&flow, CONNECT).is_some_and(|r| !r.on_top));
        // Tiling on, rider back in; then the stop path.
        flow.tiling_paused = false;
        flow.sync_all_top_layer_with(&probe);
        assert!(rider(&flow, CONNECT).is_some_and(|r| r.on_top) && probe.is_topmost(CONNECT));
        assert_eq!(flow.release_top_layer_with(&probe), 3);
        assert!(flow.riders.is_empty());
        assert!(!probe.is_topmost(CONNECT), "the inherited flag is cleared");
        assert!(!probe.is_topmost(F));
        // A rider that was topmost on its own keeps the flag.
        let mut flow = fixture();
        let probe = owned_desktop();
        probe.topmost.borrow_mut().insert(TILE_ERR);
        flow.sync_all_top_layer_with(&probe);
        activate(&mut flow, TILE_ERR, &probe);
        assert!(rider(&flow, TILE_ERR).is_some_and(|r| r.was_topmost));
        flow.release_top_layer_with(&probe);
        assert!(probe.is_topmost(TILE_ERR));
    }

    #[test]
    fn stale_riders_are_purged_hidden_and_destroyed_ones_forgotten() {
        let mut flow = fixture();
        let probe = owned_desktop();
        flow.sync_all_top_layer_with(&probe);
        for h in [CONNECT, ERROR, TILE_ERR] {
            activate(&mut flow, h, &probe);
        }
        assert_eq!(flow.riders.len(), 3);
        // ERROR dismissed (gone), CONNECT hidden: purged by the next pass,
        // the hidden one lowered first.
        probe.classes.borrow_mut().remove(&ERROR);
        probe.hidden.borrow_mut().insert(CONNECT);
        flow.reassert_top_layer_with(Some(A), &probe);
        assert!(rider(&flow, ERROR).is_none() && rider(&flow, CONNECT).is_none());
        assert!(!probe.is_topmost(CONNECT));
        // The anchor goes away: its rider is purged.
        flow.registry.remove_window(A);
        flow.reassert_top_layer_with(Some(F), &probe);
        assert!(flow.riders.is_empty());
        assert!(!probe.is_topmost(TILE_ERR));
        // Hide / destroy hooks.
        let mut flow = fixture();
        let probe = owned_desktop();
        flow.sync_all_top_layer_with(&probe);
        activate(&mut flow, CONNECT, &probe);
        flow.forget_rider_with(CONNECT, false, &probe);
        assert!(flow.riders.is_empty() && !probe.is_topmost(CONNECT));
        flow.riders.insert(
            ERROR,
            Rider {
                anchor: F,
                on_top: true,
                was_topmost: false,
            },
        );
        flow.on_window_destroyed(ERROR);
        assert!(flow.riders.is_empty());
    }

    #[test]
    fn an_explicitly_ignored_shell_surface_is_never_a_member() {
        let mut flow = fixture();
        if let Some(w) = flow.registry.get_window_mut(HWND(I as *mut _)) {
            w.class = "Windows.UI.Core.CoreWindow".into();
        }
        let probe = MockTopmost::default();
        let summary = flow.sync_all_top_layer_with(&probe);
        assert_eq!(summary.entered, 1);
        assert!(!on_top(&flow, I) && !probe.is_topmost(I));
        assert!(on_top(&flow, F));
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
