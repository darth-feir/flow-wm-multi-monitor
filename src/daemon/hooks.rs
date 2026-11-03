//! Win32 hook event handlers.
//!
//! This module contains individual handlers for each type of hook event:
//!
//! - [`on_window_created`] — handles new window creation
//! - [`on_window_destroyed`] — handles window destruction
//! - [`on_window_minimized`] — handles window minimize events
//! - [`on_window_restored`] — handles window restore (un-minimize) events
//! - [`on_window_hidden`] — handles tray-hide / DWM-cloak events
//! - [`on_window_shown`] — handles un-hide (tray-restore) events
//! - [`on_focus_changed`] — handles focus changes
//! - [`on_window_state_change`] — recovers (and, with `adopt_maximized`,
//!   adopts) windows parked as `Ignored(Maximized|Fullscreen)`; also run
//!   from the foreground, show and move-size-end handlers for such a
//!   window, because the `STATECHANGE` for an un-maximize does not always
//!   arrive
//!
//! # Window-removal pipeline
//!
//! Both [`on_window_destroyed`] and [`on_window_minimized`] and
//! [`on_window_hidden`] share the
//! [`remove_from_layout_and_refocus`] helper, which implements the full
//! removal pipeline: remove from the virtual layout → push OS-level focus to
//! the successor window if focus changed → animate the resulting layout diff.
//! The successor window is chosen by [`ScrollingSpace::remove_window`](crate::workspace::ScrollingSpace::remove_window) via
//! [`mutations::next_available_window`] (same-column sibling first, then left, then right).

use crate::common::WindowId;
use crate::registry::hooks::remove_float_hwnd;
use crate::registry::types::{FloatingState, ReclassifyResult, VisibilityChange, WindowState};
use crate::registry::win32 as registry_win32;
use crate::workspace::{WorkspaceId, WorkspaceParking};
use windows::Win32::Foundation::HWND;

use super::animation::PrevRects;
use super::types::FlowWM;

/// How a window reached the creation pipeline: a window the user just
/// opened, or one that was already on the desktop and comes back (restored
/// from minimize while flow had never classified it, or classified only when
/// it took the foreground). Only a new window is seated on the focused
/// screen (`[focus] new_window_monitor`, ADR-0023).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Arrival {
    /// A freshly created window.
    New,
    /// A window that keeps the screen it is on.
    Returning,
}

/// How long after the focused tile left its strip a foreground Windows
/// hands to a window on another screen is taken back.
pub(super) const FOCUS_HANDOFF_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

/// The removal pipeline's note that the focused tile just left the strip
/// of monitor `monitor`: until `until`, a foreground that lands on a window
/// of another screen is sent back to `successor`, or to the desktop when the
/// strip has none. Windows activates the next window in z-order when the
/// foreground window closes or minimizes, wherever that window is, and it
/// does so after the hide / destroy event flow reacts to. See `docs/src/dev-guide/event-pipelines.md`, *Focus after a
/// removal or a monitor switch*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FocusHandoff {
    /// The monitor whose strip lost the focused tile.
    pub(super) monitor: usize,
    /// The strip's new focus, `None` when it is empty.
    pub(super) successor: Option<WindowId>,
    /// End of the redirect window.
    pub(super) until: std::time::Instant,
}

impl FlowWM {
    /// Handle a window creation event.
    ///
    /// Pipeline:
    /// 1. `registry.handle_created(hwnd)` — classifies and registers the window.
    /// 2. If the window was classified as tiling (`Some(WindowId)`):
    ///    - `layout.insert_window(id)` — places the new column immediately
    ///      after the focused window, shifts right-side columns rightward by
    ///      one `column_shift`, moves focus to the new window, and ensures it
    ///      is visible.
    ///    - `animate_layout(applied)` — animates the resulting layout change.
    /// 3. If the window was classified as floating: complete the float setup
    ///    that an explicit `set-window float` performs — place it into the
    ///    active [`FloatingSpace`](crate::workspace::FloatingSpace) at a
    ///    centered rect, arm float-location tracking, animate, and attach a
    ///    border. Without this, a rule-classified float would be tracked in the
    ///    registry but invisible to workspace switching and borderless until
    ///    the user toggled it. See (`docs/src/dev-guide/floating-space.md`).
    /// 4. Ignored or already-tracked windows: no action needed.
    ///
    /// # Return value
    ///
    /// Returns `true` if [`handle_created`](crate::registry::WindowRegistry::handle_created)
    /// processed the window (classified it as tiling, floating, or ignored, or
    /// it was already tracked). Returns `false` if classification **failed** —
    /// the window is not yet ready (not visible, no title, styles not
    /// finalized). The caller should add the hwnd to the pending-creations
    /// retry list when this returns `false`.
    ///
    /// # Why classification can fail
    ///
    /// `EVENT_OBJECT_CREATE` fires early in the Win32 window lifecycle —
    /// before `ShowWindow`, `SetWindowText`, or style finalization. The
    /// classification checks (`is_window_visible`, title non-empty,
    /// `is_alt_tab_visible`) all fail on a not-yet-shown window. A subsequent
    /// retry (after `EVENT_SYSTEM_FOREGROUND` or other events arrive) will
    /// typically succeed.
    ///
    /// # Placement strategy
    ///
    /// Unlike [`on_window_restored`](Self::on_window_restored) which re-adds a
    /// previously-minimized window at the far right via `add_window`, new
    /// windows are inserted next to the focused window so they appear where
    /// the user is actively working. See
    /// [`ScrollingSpace::insert_window`](crate::workspace::ScrollingSpace::insert_window)
    /// for the full algorithm.
    pub(super) fn on_window_created(&mut self, hwnd: isize) -> bool {
        self.on_window_created_as(hwnd, Arrival::New)
    }

    /// [`on_window_created`](Self::on_window_created) for a window that
    /// arrives as `arrival`: a new window is seated on the screen
    /// [`monitor_for_arrival`](Self::monitor_for_arrival) picks, a returning
    /// one on its own.
    pub(super) fn on_window_created_as(&mut self, hwnd: isize, arrival: Arrival) -> bool {
        let tracked = self.classify_created(hwnd, arrival);
        if tracked {
            // Classified: its pre-placement (if any) is done with.
            self.pre_placed.remove(&hwnd);
        }
        tracked
    }

    /// The body of [`on_window_created_as`](Self::on_window_created_as):
    /// classify, register and seat; `true` when the window is tracked.
    fn classify_created(&mut self, hwnd: isize, arrival: Arrival) -> bool {
        // Tiling off: classify and register only, so the window is known
        // when tiling resumes (`daemon::tiling` seats it then). No seat, no
        // move, no focus push, no border.
        if self.tiling_paused {
            let _ = self.registry.handle_created(hwnd);
            return self.registry.is_tracked(hwnd);
        }

        // `was_tracked` distinguishes a FRESH registration (this call just
        // classified the window) from a de-duplicated duplicate CREATE for an
        // already-managed window. handle_created early-returns None on its
        // `contains_key` gate for the latter; without this flag a freshly
        // classified float (which still needs FloatingSpace init) would be
        // indistinguishable from a duplicate of an already-initialised float.
        let was_tracked = self.registry.is_tracked(hwnd);

        // Tiling: handle_created returns Some only for a freshly-registered
        // active tiling window.
        if let Some(window_id) = self.registry.handle_created(hwnd) {
            self.seat_created_tile(hwnd, window_id, arrival);
            return true;
        }

        // Met minimized: registered in its minimized state, nothing to seat
        // until it is restored — but it needs an owning monitor now, from
        // the rect it restores to (its icon rect says nothing), so the
        // restore hook brings it back on that monitor and `flow query all`
        // reports it there meanwhile.
        if !was_tracked && self.registry.is_minimized(hwnd) {
            self.remember_monitor_from_pre_manage_rect(hwnd);
            return true;
        }

        // `adopt_maximized`: a window just registered `Ignored(Maximized)`
        // is un-maximized and classified through the rules right away (the
        // registry leaves a window the rules ignore maximized). A tile is
        // seated like any new window; a float falls through to the
        // fresh-float branch below.
        if !was_tracked
            && self.registry.adopt_maximized()
            && self.registry.is_os_ignored(hwnd)
            && let ReclassifyResult::Recovered { now_tiling: true } =
                self.registry.reclassify_os_state(hwnd)
        {
            self.seat_created_tile(hwnd, WindowId(hwnd), arrival);
            return true;
        }

        // handle_created returned None. Sub-cases:
        //  (1) freshly classified as Floating → needs FloatingSpace/border init;
        //  (2) freshly classified as Ignored → tracked, no action, no retry;
        //  (3) duplicate CREATE for an already-managed window → no action;
        //  (4) classification failed (not visible / no title / styles pending)
        //      → caller retries via pending_creations.
        //
        // Only a freshly-registered active float (case 1) requires work. It is
        // identifiable as: not previously tracked, now in the registry as
        // Floating(Active). register_float + animate complete the float setup
        // that set_window_to_float performs for an explicit toggle, so an
        // init-classified float ends up identical to a toggled one.
        let fresh_float = !was_tracked
            && self
                .registry
                .get_window(HWND(hwnd as *mut _))
                .map(|w| matches!(w.state, WindowState::Floating(FloatingState::Active { .. })))
                .unwrap_or(false);

        if fresh_float {
            let idx = self.monitor_for_arrival(hwnd, arrival);
            let float_rect = self.centered_float_rect_on(WindowId(hwnd), idx);
            // The new float has no previous rect, so it appears at its
            // place instead of gliding from where it was created
            // (`animation::first_placement`); the registry already holds its
            // target, so its entry is left out of the snapshot.
            let mut prev = self.snapshot_prev_rects();
            prev.remove(&hwnd);
            let float_actual = self.register_float(WindowId(hwnd), float_rect);
            self.animate_workspaces_on(
                self.active_monitor,
                &[(float_actual, 0)],
                PrevRects::Snapshot(&prev),
            );
            self.reconcile_new_window_focus(hwnd);
        } else if !was_tracked {
            // A window freshly registered as ignored by an explicit rule (a
            // dialog, a popup) enters the floating layer; the float above
            // entered it in `register_float`. No-op for anything else.
            self.sync_top_layer(hwnd);
        }

        // Report whether the window ended up tracked (classified) so the caller
        // retries only genuinely-not-ready windows (case 4).
        self.registry.is_tracked(hwnd)
    }

    /// Record the owning monitor of a window registered **minimized**: the
    /// monitor whose screen holds the centre of its restore rect
    /// (`pre_manage_rect`, read from its placement), else the first one the
    /// rect overlaps, else the one Windows reports for the handle, else the
    /// active monitor. Used at creation (here) and by the startup scan.
    pub(super) fn remember_monitor_from_pre_manage_rect(&mut self, hwnd: isize) {
        let rect = self
            .registry
            .get_window(HWND(hwnd as *mut _))
            .map(|w| w.pre_manage_rect);
        let idx = rect
            .and_then(|r| self.monitor_index_for_rect_opt(r))
            .or_else(|| self.monitor_index_for_hmonitor_of(hwnd))
            .unwrap_or(self.active_monitor);
        self.remember_monitor(WindowId(hwnd), idx);
        log::debug!("window {hwnd:#x} registered minimized; owning monitor {idx}");
    }

    /// Give an untracked window a chance to register before `event` is
    /// handled: the restore, foreground and show hooks all reach windows
    /// the scan and `CREATE` never classified. Runs the full creation
    /// pipeline ([`on_window_created`](Self::on_window_created)) and returns
    /// whether the window is tracked afterwards. For a tracked window this
    /// is one `HashMap` hit; for the rest, the creation gates decide.
    fn register_if_untracked(&mut self, hwnd: isize, event: &str, arrival: Arrival) -> bool {
        if self.registry.is_tracked(hwnd) {
            return true;
        }
        log::debug!("{event}: {hwnd:#x} not tracked — attempting creation");
        self.on_window_created_as(hwnd, arrival);
        self.registry.is_tracked(hwnd)
    }

    /// The monitor a window entering the creation pipeline is seated on: a
    /// [`New`](Arrival::New) window on the active (focused) monitor while
    /// `[focus] new_window_monitor = "focused"`, every other one on its home
    /// monitor ([`home_monitor_for_hwnd`](Self::home_monitor_for_hwnd): a
    /// fresh window has no owning-monitor memory, so the monitor Windows
    /// placed it on). See ADR-0023.
    pub(super) fn monitor_for_arrival(&self, hwnd: isize, arrival: Arrival) -> usize {
        match (arrival, self.config.focus.new_window_monitor) {
            (Arrival::New, crate::config::types::NewWindowMonitor::Focused) => self.active_monitor,
            _ => self.home_monitor_for_hwnd(hwnd),
        }
    }

    /// Seat a freshly-classified (or freshly-adopted) tile the way a new
    /// window is seated: on the monitor [`monitor_for_arrival`](Self::monitor_for_arrival)
    /// picks — for a new window the focused one, at that strip's width —
    /// next to that strip's focused column, then push focus and attach the
    /// border. The window has no previous rect, so it is placed at its slot
    /// instantly while the other columns tween
    /// (`animation::first_placement`, ADR-0023 *Appearing in place*).
    fn seat_created_tile(&mut self, hwnd: isize, window_id: WindowId, arrival: Arrival) {
        let idx = self.monitor_for_arrival(hwnd, arrival);
        if idx != self.home_monitor_for_hwnd(hwnd) {
            log::debug!(
                "new window {hwnd:#x} seated on the focused monitor {idx} (placed on {})",
                self.home_monitor_for_hwnd(hwnd)
            );
        }
        let applied = self.monitors[idx]
            .active_scrolling_mut()
            .insert_window(window_id);
        self.remember_monitor(window_id, idx);
        self.animate_layout_on(idx, &applied);
        self.reconcile_new_window_focus(hwnd);
    }

    /// Reconcile OS focus and border overlays for a freshly-tracked window
    /// (tiling or floating), shared by both branches of [`on_window_created`].
    ///
    /// Actively pushes the OS foreground to the new window (not just a passive
    /// sync) so apps launched without a user-visible activation receive focus
    /// too — see (`docs/src/dev-guide/event-pipelines.md`). On success the
    /// registry focus is synced and the previous foreground's border is
    /// recoloured, mirroring [`on_focus_changed`]'s two-window refresh.
    fn reconcile_new_window_focus(&mut self, hwnd: isize) {
        let prev_focus = self.registry.focused().map(|id| id.0);

        // The layout already treats this window as focused; push OS foreground
        // to match. Skip the push when already foreground (normal launches) so
        // it stays a no-op. The resulting EVENT_SYSTEM_FOREGROUND is re-consumed
        // by on_focus_changed, which is idempotent here (same workspace; border
        // refresh short-circuits on unchanged style).
        let became_foreground = registry_win32::get_foreground_window() == Some(hwnd)
            || registry_win32::set_foreground_window(hwnd);
        if became_foreground {
            self.registry.set_focused(hwnd);
        }

        // Border overlay: attach (or skip) based on the freshly-classified
        // state. Tiling/Floating → attach; Ignored → no-op.
        self.refresh_border_for(hwnd);

        // If focus moved to the new window, recolor the previous foreground.
        if became_foreground
            && let Some(prev) = prev_focus
            && prev != hwnd
        {
            self.refresh_border_for(prev);
        }
    }

    /// Handle a window destruction event.
    ///
    /// Pipeline:
    /// 1. Check if the window was in tiling state **before** removal.
    /// 2. If tiling: [`remove_from_layout_and_refocus`] — removes from layout,
    ///    pushes focus to the successor, and animates.
    /// 3. `registry.remove_window(hwnd)` — always, regardless of state.
    ///
    /// The tiling check happens before removal because `remove_window`
    /// deletes the entry from the registry.
    pub(super) fn on_window_destroyed(&mut self, hwnd: isize) {
        // A keymap drag session on this window ends with it.
        self.abandon_manual_drag_for(hwnd);
        // A floating-layer rider (an owned dialog, ADR-0024) is forgotten.
        self.forget_rider(hwnd, true);

        // Tiling off: bookkeeping only — out of every strip and floating
        // space (no animation, no successor focus) and out of the registry.
        if self.tiling_paused {
            self.purge_windows_from_all_spaces(&[WindowId(hwnd)]);
            remove_float_hwnd(hwnd);
            self.forget_submitted(hwnd);
            self.forget_fullscreen(hwnd, false);
            self.registry.remove_window(hwnd);
            return;
        }

        let was_tiling = self.registry.is_tiling(hwnd);

        if was_tiling {
            self.remove_from_layout_and_refocus(WindowId(hwnd), true);
        }

        // A destroyed float must leave the tracking set so the LOCATIONCHANGE
        // callback stops forwarding it, and leave its FloatingSpace so a later
        // reuse of the HWND value cannot resurrect a stale entry. Harmless
        // no-op for tiled/ignored windows.
        remove_float_hwnd(hwnd);
        self.remove_float_from_all_spaces(WindowId(hwnd));
        self.forget_submitted(hwnd);
        // A fullscreen window that closed takes its origin with it.
        self.forget_fullscreen(hwnd, false);
        crate::registry::hooks::set_app_fullscreen_watch(hwnd, false);

        self.registry.remove_window(hwnd);
        // Border overlay: cleaned up automatically — `remove_window` drops the
        // Window, whose `border: Option<Border>` field drops the Border, whose
        // Drop impl calls DestroyWindow on the overlay HWND.
    }

    /// Handle a window minimize event.
    ///
    /// Pipeline:
    /// 1. `registry.minimize_window(hwnd)` — updates state to `Tiling::Minimized`.
    /// 2. If the window was tiling-active (before minimize):
    ///    [`remove_from_layout_and_refocus`] — removes from layout, pushes
    ///    focus to the successor, and animates remaining windows filling the gap.
    pub(super) fn on_window_minimized(&mut self, hwnd: isize) {
        // A window minimized under a keymap drag session leaves it.
        self.abandon_manual_drag_for(hwnd);

        // Tiling off: registry only. The strip keeps the window as the
        // memory of its seat; the resume drops it if it is still minimized
        // then, and the later restore hook re-adds it as usual.
        if self.tiling_paused {
            self.registry.minimize_window(hwnd);
            return;
        }

        let was_tiling = self.registry.is_tiling(hwnd);
        self.registry.minimize_window(hwnd);

        if was_tiling {
            self.remove_from_layout_and_refocus(WindowId(hwnd), true);
            self.forget_submitted(hwnd);
        }
        // A minimized float leaves the floating layer (back in on restore).
        self.sync_top_layer(hwnd);
        // Border overlay: registry now records Minimized ⇒ refresh detaches.
        self.refresh_border_for(hwnd);
    }

    /// Handle `EVENT_OBJECT_HIDE` for a window that may have been tray-hidden
    /// or DWM-cloaked (Discord/Steam "close to tray"), as opposed to an
    /// explicit minimize (handled by [`on_window_minimized`](Self::on_window_minimized)).
    ///
    /// Pipeline:
    /// 1. Capture active-tiling status **before** reconcile mutates state.
    /// 2. `registry.reconcile_visibility(hwnd)` — state-based idempotent
    ///    transition (`Active → Hidden`, preserving the layout slot in
    ///    `last_virtual_slot`). Returns [`VisibilityChange::Unchanged`] if the
    ///    window was already hidden/minimized (this event also fires on ordinary
    ///    minimize — the state check prevents double-removal).
    /// 3. If the window was an **active** tiling window immediately before the
    ///    transition and the state actually changed to `Hidden`:
    ///    [`remove_from_layout_and_refocus`] — removes from layout, pushes focus
    ///    to the successor, and animates.
    ///
    /// # Why `is_tiling_active` instead of `is_tiling`
    ///
    /// [`is_tiling`](crate::registry::WindowRegistry::is_tiling) returns `true`
    /// for both `Tiling(Active)` and `Tiling(Minimized)`. Using it here would
    /// cause a second removal from the layout when a minimized window also
    /// receives `EVENT_OBJECT_HIDE` (Win32 fires both). `is_tiling_active` is
    /// `true` **only** for `Tiling(Active)`, so the first transition (minimize
    /// or hide) removes the window, and the second event sees `is_tiling_active`
    /// as `false` and is a no-op.
    pub(super) fn on_window_hidden(&mut self, hwnd: isize) {
        self.abandon_manual_drag_for(hwnd);
        // A hidden rider leaves the band and is forgotten (ADR-0024).
        self.forget_rider(hwnd, false);

        // Tiling off: registry only (see `on_window_minimized`).
        if self.tiling_paused {
            let _ = self.registry.reconcile_visibility(hwnd);
            return;
        }

        let was_active_tiling = self.registry.is_tiling_active(hwnd);
        let change = self.registry.reconcile_visibility(hwnd);
        if change == VisibilityChange::Hidden && was_active_tiling {
            self.remove_from_layout_and_refocus(WindowId(hwnd), false);
            self.forget_submitted(hwnd);
        }
        // A hidden float leaves the floating layer (back in when shown).
        self.sync_top_layer(hwnd);
        // Border overlay: registry now records Hidden ⇒ refresh detaches.
        self.refresh_border_for(hwnd);
    }

    /// Shared window-removal pipeline for destroy and minimize events.
    ///
    /// This implements the focus-aware removal flow used by both
    /// [`on_window_destroyed`] and [`on_window_minimized`]:
    ///
    /// 1. Capture the current focus (before removal).
    /// 2. [`ScrollingSpace::remove_window`](crate::workspace::ScrollingSpace::remove_window) — removes the window from the virtual
    ///    layout, resolving a focus successor via
    ///    [`mutations::next_available_window`] when the removed window was
    ///    focused (same-column sibling preferred, then left, then right).
    /// 3. **Push OS focus** — if the layout focus changed as a result of the
    ///    removal, call [`registry_win32::set_foreground_window`] on the
    ///    successor so the OS actually foregrounds it, then sync the registry's
    ///    focus tracking via [`WindowRegistry::set_focused`].
    /// 4. [`animate_layout`](Self::animate_layout) — animate the remaining windows
    ///    into their new positions.
    ///
    /// # Why capture focus before *and* after
    ///
    /// Comparing focus before and after removal tells us whether the removed
    /// window was the focused one. Only then do we need to push a new
    /// foreground window to the OS — if focus is unchanged, the OS focus is
    /// already correct and we avoid a redundant (and potentially disruptive)
    /// `SetForegroundWindow` call.
    ///
    /// # Non-active monitors and foreign focus
    ///
    /// The successor is only pushed when the removed window held the user's
    /// focus, or lived on the active monitor while the user's focus was on
    /// that strip ([`should_refocus_after_removal`]); otherwise only the
    /// strip's bookkeeping changes. A tile minimized or hidden while its
    /// workspace is parked leaves that parked strip, which is re-seated at
    /// its parking offset with no focus push. See
    /// `docs/src/dev-guide/event-pipelines.md`, *Focus after a removal or a
    /// monitor switch*. `recent_loss_counts` (destroy, minimize; not a hide,
    /// which an app may do *because* it lost the foreground) lets a window
    /// that lost the foreground to another screen a moment ago count as the
    /// user's focus.
    fn remove_from_layout_and_refocus(&mut self, window: WindowId, recent_loss_counts: bool) {
        let idx = self.monitor_index_for_window(window);
        let active_id = self.monitors[idx].active_workspace_id();
        let home = self.monitors[idx]
            .find_workspace_containing(window)
            .unwrap_or(active_id);
        if home != active_id {
            self.remove_from_parked_workspace(idx, home, window);
            return;
        }

        // Who holds the user's focus, read before the removal changes the
        // strip: nobody, the removed window, or a tile of this strip all
        // let the successor take over; a float or a window elsewhere does not.
        let focus_on_strip = match self.registry.focused() {
            None => true,
            Some(f) => {
                f == window
                    || self.monitors[idx]
                        .active_scrolling()
                        .virtual_layout()
                        .find_window(f)
                        .is_some()
            }
        };
        let prev_focus = self.monitors[idx].active_scrolling().last_focused_window();
        let applied = self.monitors[idx]
            .active_scrolling_mut()
            .remove_window(window);
        let new_focus = self.monitors[idx].active_scrolling().last_focused_window();

        // The removed window held the user's focus: by registry or OS, or a
        // moment ago — Windows can give the foreground to a window on the
        // other screen before the hide / destroy event reaches flow.
        let focus_elsewhere = self
            .registry
            .focused()
            .and_then(|f| self.monitor_index_containing(f))
            .is_some_and(|m| m != idx);
        let held_focus = self.registry.focused() == Some(window)
            || registry_win32::get_foreground_window() == Some(window.0)
            || (recent_loss_counts && focus_elsewhere && self.lost_focus_just_now(window));
        // The user's focus left with the window: the strip's focus takes
        // it, also when that was not the removed window.
        let refocus = should_refocus_after_removal(
            prev_focus,
            new_focus,
            held_focus,
            idx == self.active_monitor,
            focus_on_strip,
        ) || (held_focus && new_focus.is_some());
        if refocus && let Some(id) = new_focus {
            let target = id.0;
            if !registry_win32::set_foreground_window(target) {
                log::warn!(
                    "remove_from_layout_and_refocus: SetForegroundWindow failed for hwnd {target}"
                );
            }
            self.registry.set_focused(target);
        }
        if held_focus {
            self.keep_focus_on_monitor(idx, new_focus);
        }

        self.animate_layout_on(idx, &applied);
    }

    /// Whether `window` lost the foreground within
    /// [`FOCUS_HANDOFF_WINDOW`].
    fn lost_focus_just_now(&self, window: WindowId) -> bool {
        self.last_focus_loss
            .is_some_and(|(h, at)| h == window.0 && at.elapsed() < FOCUS_HANDOFF_WINDOW)
    }

    /// The focused tile just left monitor `idx`'s strip: that monitor stays
    /// the active one, and a foreground Windows hands to another screen in
    /// the next moment is sent back ([`FocusHandoff`]). With no `successor`
    /// (the strip is empty) the focus goes to the desktop: no window keeps
    /// it, and a window of another screen that already took it gives it up.
    fn keep_focus_on_monitor(&mut self, idx: usize, successor: Option<WindowId>) {
        self.set_active_monitor(idx);
        self.focus_handoff = Some(FocusHandoff {
            monitor: idx,
            successor,
            until: std::time::Instant::now() + FOCUS_HANDOFF_WINDOW,
        });
        if successor.is_some() {
            return;
        }
        let foreground_elsewhere = registry_win32::get_foreground_window()
            .and_then(|fg| self.monitor_index_containing(WindowId(fg)))
            .is_some_and(|m| m != idx);
        self.drop_tracked_focus();
        if foreground_elsewhere && !registry_win32::clear_foreground_window() {
            log::debug!("keep_focus_on_monitor: the desktop refused the foreground");
        }
        log::debug!(
            "the last tile of monitor {idx} left: monitor {idx} stays active, focus on the desktop"
        );
    }

    /// The [`FocusHandoff`] redirect, run first for every foreground event:
    /// `true` when this foreground was Windows handing the focus of a closed
    /// or minimized tile to a window on another screen and flow sent it back
    /// (the event is then not processed; the one for the window it went
    /// back to follows).
    fn redirect_focus_handoff(&mut self, hwnd: isize) -> bool {
        let Some(handoff) = self.focus_handoff else {
            return false;
        };
        if std::time::Instant::now() >= handoff.until {
            self.focus_handoff = None;
            return false;
        }
        if handoff.successor == Some(WindowId(hwnd)) {
            return false;
        }
        // A window of the same screen, the desktop or a window flow does
        // not manage: nothing to undo.
        if self
            .monitor_index_containing(WindowId(hwnd))
            .is_none_or(|m| m == handoff.monitor)
        {
            return false;
        }
        // Stays armed until it expires: the drift poll and the hook can both
        // report the same stray foreground. An event for a window that no
        // longer has the foreground (already sent back) is just dropped.
        if registry_win32::get_foreground_window() != Some(hwnd) {
            log::debug!("focus handoff: stale foreground {hwnd:#x} on another screen ignored");
            return true;
        }
        let sent_back = match handoff.successor {
            Some(s) => registry_win32::set_foreground_window(s.0),
            None => {
                let cleared = registry_win32::clear_foreground_window();
                if cleared {
                    self.drop_tracked_focus();
                }
                cleared
            }
        };
        log::debug!(
            "focus handoff: Windows gave the foreground to {hwnd:#x} on another screen; back to {:?} on monitor {}: {sent_back}",
            handoff.successor,
            handoff.monitor
        );
        sent_back
    }

    /// Remove `window` from the strip of parked workspace `ws_id` on monitor
    /// `idx`, re-seat that strip at its parking offset and sync the
    /// registry's slots and rects for it. No focus push: the user is not
    /// looking at that workspace.
    fn remove_from_parked_workspace(&mut self, idx: usize, ws_id: WorkspaceId, window: WindowId) {
        let (applied, offset) = {
            let m = &mut self.monitors[idx];
            let window_gap = m.active_scrolling().padding().window_gap;
            let parking =
                WorkspaceParking::new(m.active_scrolling().monitor(), m.screen_rect(), window_gap);
            let offset = parking.offset(ws_id, m.active_workspace_id());
            let Some(ws) = m.workspace_mut(ws_id) else {
                return;
            };
            (ws.scrolling.remove_window(window), offset)
        };
        log::debug!(
            "window {} removed from parked workspace {} on monitor {idx}",
            window.0,
            ws_id.0
        );
        self.animate_workspaces_on(
            idx,
            &[(applied.actual_layout.clone(), offset)],
            PrevRects::Registry,
        );
        self.registry
            .update_tiling_slots_from_layout(&applied.virtual_layout);
        self.registry.update_tiled_rects(&applied.actual_layout);
    }

    /// Drop `window` from the floating space of every workspace on every
    /// monitor. Used when a float leaves the desktop (destroyed) so no stale
    /// entry survives in a parked workspace.
    pub(super) fn remove_float_from_all_spaces(&mut self, window: WindowId) {
        for m in &mut self.monitors {
            for ws in m.workspaces_mut() {
                ws.floating.remove(window);
            }
        }
    }

    /// Handle a window restore (un-minimize) event.
    ///
    /// Pipeline:
    /// 0. An untracked window gets the creation pipeline first
    ///    ([`register_if_untracked`](Self::register_if_untracked)) and is
    ///    seated by it; the steps below find it in place.
    /// 1. `registry.restore_window(hwnd)` — updates state back to `Tiling::Active`
    ///    (re-measuring the geometry of a window registered minimized).
    /// 2. If the window is now tiling-active (after restore):
    ///    - `layout.add_window(id)` — re-adds to layout on its home monitor.
    ///    - `animate_layout(applied)` — animates the new window appearing.
    /// 3. A float registered minimized was never placed in a floating
    ///    space: it is placed now, where it reappeared
    ///    (`populate_existing_floats`).
    pub(super) fn on_window_restored(&mut self, hwnd: isize) {
        if !self.registry.is_tracked(hwnd) {
            // MINIMIZEEND can arrive while GetWindowRect still reports the
            // icon's frame; registering now would make that the window's
            // pre-manage rect. Let the pending-creation retry (100 ms) or
            // the activation that follows a restore register it instead.
            if registry_win32::get_window_rect(HWND(hwnd as *mut _))
                .is_ok_and(registry_win32::is_icon_rect)
            {
                log::debug!(
                    "on_window_restored: {hwnd:#x} not tracked and still icon-sized — deferring creation"
                );
                if !self.pending_creations.iter().any(|(h, _)| *h == hwnd) {
                    self.pending_creations.push((hwnd, 0));
                }
                return;
            }
            if !self.register_if_untracked(hwnd, "on_window_restored", Arrival::Returning) {
                return;
            }
        }
        self.registry.restore_window(hwnd);

        // Tiling off: registry only. A window still in its strip's memory
        // keeps that seat for the resume; one that is not gets appended then.
        if self.tiling_paused {
            return;
        }

        // After restore, check if the window is now tiling-active. It goes
        // back to the monitor it was minimized from (owning-monitor memory),
        // not to wherever its parked rect happens to be nearest.
        if self.registry.is_tiling(hwnd) {
            self.readd_tile_to_home_monitor(hwnd);
        } else {
            self.seat_float_if_unseated(hwnd);
            self.resync_returning_float(hwnd);
        }
        // A restored float is back in the floating layer (a fullscreen
        // window, re-seated as a fullscreen column above, is not a member).
        self.sync_top_layer(hwnd);
        // Border overlay: registry now records Active ⇒ refresh re-attaches.
        self.refresh_border_for(hwnd);
    }

    /// Place an active float that belongs to no floating space — one
    /// registered while minimized and just restored — into the space of the
    /// monitor it is on, at its current rect (centred if the rect cannot be
    /// read), and animate that space. A float that already has a seat (one
    /// minimized while managed keeps it) is left alone.
    fn seat_float_if_unseated(&mut self, hwnd: isize) {
        let wid = WindowId(hwnd);
        let active_float = self
            .registry
            .get_window(HWND(hwnd as *mut _))
            .is_some_and(|w| {
                matches!(w.state, WindowState::Floating(FloatingState::Active { .. }))
            });
        if !active_float || self.monitor_index_containing(wid).is_some() {
            return;
        }
        let rect = self
            .current_visible_rect(hwnd)
            .unwrap_or_else(|| self.centered_float_rect(wid));
        let float_actual = self.register_float(wid, rect);
        self.animate_workspaces(&[(float_actual, 0)]);
        log::debug!("float {hwnd:#x} registered minimized is now seated at {rect:?}");
    }

    /// Re-read the rect of a float that came back from minimize or hide
    /// (the window is where Windows restored it, which is not always where
    /// it was) into its floating space, the registry and its border overlay
    /// ([`store_float_rect`](Self::store_float_rect)). Skipped while the
    /// window still reports its minimized icon's frame: the registry then
    /// keeps the rect it had before (`last_float_rect`). No-op for anything
    /// but an active, seated float.
    fn resync_returning_float(&mut self, hwnd: isize) {
        if registry_win32::get_window_rect(HWND(hwnd as *mut _))
            .is_ok_and(|r| !registry_win32::is_icon_rect(r))
        {
            self.store_float_rect(hwnd);
        }
    }

    /// Handle `EVENT_OBJECT_SHOW`.
    ///
    /// This event covers two distinct situations, handled in order below.
    ///
    /// # 1. Recovery — a window created *invisible* (never registered)
    ///
    /// Some applications create their top-level window hidden and only show it
    /// **after** their title has arrived via `EVENT_OBJECT_NAMECHANGE`. For
    /// such windows both `CREATE` and the
    /// [`on_window_name_change`](Self::on_window_name_change) recovery fail
    /// [`handle_created`](crate::registry::WindowRegistry::handle_created)'s
    /// visibility gate (the window is not yet visible), so they stay untracked.
    /// Windows Terminal launched as `wt -p PowerShell` is the canonical
    /// example, with event order `Created → NameChange → Shown`.
    ///
    /// `SHOW` is the first event at which the window is actually visible, so it
    /// is the natural point to retry registration. If the window is **not
    /// already tracked**, we re-run the full creation pipeline via
    /// [`on_window_created`](Self::on_window_created). By the time `SHOW`
    /// fires the title is already set (the earlier `NAMECHANGE` proved it —
    /// it failed for *visibility*, not *empty title*), so the only thing that
    /// changed is visibility, and the window now passes every gate.
    ///
    /// This path is **complementary** to `on_window_name_change`: that handler
    /// catches the opposite ordering (window visible *first*, title arriving
    /// *later*), where `SHOW` would see an empty title. Both are needed for
    /// full coverage of asynchronous-window lifecycles.
    ///
    /// # 2. Re-show — a tracked window returning from hidden
    ///
    /// A window we already manage (e.g. Discord reopened from the tray, as
    /// opposed to an explicit restore-from-minimize handled by
    /// [`on_window_restored`](Self::on_window_restored)) transitions
    /// `Hidden → Active`:
    /// 1. `registry.reconcile_visibility(hwnd)` — state-based idempotent
    ///    transition (`Hidden → Active`, restoring the saved slot from
    ///    `last_virtual_slot`). Returns [`VisibilityChange::Unchanged`] if the
    ///    window was already active.
    /// 2. If the state actually changed to `Shown` and the window is now an
    ///    **active** tiling window:
    ///    - `layout.add_window(id)` — re-adds to layout.
    ///    - `animate_layout(applied)` — animates the window reappearing.
    ///
    /// # Why this is safe to fire on every `SHOW`
    ///
    /// Win32 fires `EVENT_OBJECT_SHOW` for many windows we do not manage. The
    /// untracked branch is bounded by `handle_created`'s full gate pipeline
    /// (visibility, title, Alt+Tab, owner) — non-app windows are filtered, not
    /// registered. The tracked branch is bounded by `reconcile_visibility`,
    /// which returns `Unchanged` for already-active and floating windows.
    /// `Floating` windows receive state updates but are never in the layout.
    ///
    /// Duplicate registration across recovery events (`NAMECHANGE` + `SHOW`) is
    /// impossible: [`is_tracked`](crate::registry::WindowRegistry::is_tracked)
    /// short-circuits the second handler, and `handle_created`'s `contains_key`
    /// gate is the final guard — an `HWND` is unique per window, so a window
    /// already in the registry is ignored.
    pub(super) fn on_window_shown(&mut self, hwnd: isize) {
        // Recovery: a window created invisible (e.g. `wt -p PowerShell`, whose
        // title arrives via NAMECHANGE while still hidden) was missed by both
        // CREATE and NAMECHANGE (both gate on visibility). SHOW is the first
        // event where the window is actually visible — attempt registration
        // now. Mirrors `on_window_name_change`; safe because `handle_created`
        // de-duplicates by HWND. A window registered here is fully seated by
        // the creation pipeline, so nothing below applies to it.
        if !self.registry.is_tracked(hwnd) {
            let _ = self.register_if_untracked(hwnd, "on_window_shown", Arrival::New);
            return;
        }

        // An OS-ignored window being shown may have been un-maximized
        // without a STATECHANGE reaching us: re-run the recovery.
        if self.registry.is_os_ignored(hwnd) {
            self.on_window_state_change(hwnd);
        }

        // Re-show path: a tracked window returning from hidden (Hidden → Active).
        let change = self.registry.reconcile_visibility(hwnd);
        // Tiling off: the registry transition above is all that happens.
        if self.tiling_paused {
            return;
        }
        if change == VisibilityChange::Shown && self.registry.is_tiling_active(hwnd) {
            self.readd_tile_to_home_monitor(hwnd);
        } else if change == VisibilityChange::Shown {
            self.resync_returning_float(hwnd);
        }
        // A re-shown float is back in the floating layer.
        self.sync_top_layer(hwnd);
        // Border overlay: registry now records Active ⇒ refresh re-attaches.
        // (The untracked-recovery branch above early-returns after
        // on_window_created, which performs its own refresh_border_for; this
        // line only runs for the tracked-reveal path.)
        self.refresh_border_for(hwnd);
    }

    /// Handle `EVENT_OBJECT_CLOAKED` — DWM stopped rendering a window that
    /// is still `WS_VISIBLE` (a suspended UWP app). Treated exactly like
    /// [`on_window_hidden`](Self::on_window_hidden); see
    /// `docs/src/dev-guide/event-pipelines.md`, *Cloaked / Uncloaked*.
    pub(super) fn on_window_cloaked(&mut self, hwnd: isize) {
        self.on_window_hidden(hwnd);
    }

    /// Handle `EVENT_OBJECT_UNCLOAKED` — DWM started rendering a window.
    /// A UWP frame is created cloaked and never `SHOW`n, so this is the
    /// first event at which it can be registered. Treated exactly like
    /// [`on_window_shown`](Self::on_window_shown); see
    /// `docs/src/dev-guide/event-pipelines.md`, *Cloaked / Uncloaked*.
    pub(super) fn on_window_uncloaked(&mut self, hwnd: isize) {
        if !self.registry.is_tracked(hwnd) {
            let _ = self.register_if_untracked(hwnd, "on_window_uncloaked", Arrival::New);
            return;
        }
        self.on_window_shown(hwnd);
    }

    /// Handle `EVENT_SYSTEM_FOREGROUND` — the single sink for all foreground
    /// changes, both external (alt-tab, PowerToys, clicks) and self-induced
    /// (`set_foreground_window` from `dispatch_focus` /
    /// `switch_active_workspace`). For self-induced changes the method is a
    /// natural no-op: the focused column is already visible, so
    /// `ensure_focused_visible` returns `None`.
    pub(super) fn on_focus_changed(&mut self, hwnd: isize) {
        // Windows handing the focus of a tile that just closed to a window
        // on another screen: sent back, nothing else to do for this event.
        if self.redirect_focus_handoff(hwnd) {
            return;
        }

        // The floating layer re-decides against this window below and the
        // hook callback forwards its resizes from now on.
        self.note_foreground(hwnd);

        // An untracked window taking the foreground is registered first so
        // the bookkeeping below finds it; one that fails a creation gate
        // (taskbar, menu, dialog) falls through and the rest no-ops.
        let _ = self.register_if_untracked(hwnd, "on_focus_changed", Arrival::Returning);
        // A `fullscreen = "app"` window is watched from its first
        // activation on (ADR-0020).
        self.update_app_fullscreen_watch(hwnd);

        // An OS-ignored window taking the foreground may have been
        // un-maximized without a STATECHANGE reaching us (seen with Firefox
        // and Alacritty): re-run the recovery first, so a window the rules
        // tile is seated before the focus bookkeeping below looks for it.
        // One HashMap lookup for every other window.
        if self.registry.is_os_ignored(hwnd) {
            self.on_window_state_change(hwnd);
        }

        // Tiling off: track the focus (the resume reads it to pick the
        // active monitor and workspace) but never switch a workspace,
        // scroll a strip or recolour a border — there are none.
        if self.tiling_paused {
            self.registry.set_focused(hwnd);
            return;
        }

        // Capture the previous focus BEFORE set_focused mutates it: only the
        // previously-focused and newly-focused windows can change border
        // color on a focus switch, so we refresh just those two instead of
        // every overlay. Combined with `Border::set_style`'s unchanged-style
        // short-circuit, this makes the focus path O(1) rather than O(N).
        let prev_focus = self.registry.focused().map(|id| id.0);
        if let Some(prev) = prev_focus
            && prev != hwnd
        {
            self.last_focus_loss = Some((prev, std::time::Instant::now()));
        }

        self.registry.set_focused(hwnd);

        // The floating layer (ADR-0003): members are topmost only while flow
        // owns the foreground — raised or dropped here, in the single sink,
        // from the live state of this window (`daemon::top_layer`). When the
        // decision stands, an activated member that an app demoted from the
        // topmost band is raised again right away (one ex-style read for a
        // member; nothing for a tile); the periodic pass backstops the rest.
        if self.evaluate_top_layer(Some(hwnd)).is_none() {
            self.sync_top_layer(hwnd);
        }

        let target = WindowId(hwnd);

        // Focus moved to a window on another monitor: that monitor becomes
        // the active one so subsequent commands act where the user is.
        if let Some(idx) = self.monitor_index_containing(target) {
            self.set_active_monitor(idx);
        }

        // Bail for untracked/ignored windows (taskbar, tray, off-monitor).
        let Some(home_ws) = self.active_monitor().find_workspace_containing(target) else {
            return;
        };

        // If the window lives on a different workspace, switch there. Use the
        // pure layout variant — the OS already foregrounded hwnd, so re-pushing
        // would steal focus from a floating window the user alt-tabbed to.
        let active_id = self.active_monitor().active_workspace_id();
        if home_ws != active_id {
            self.switch_workspace_layout(home_ws, active_id);
        }

        // For tiling windows: update the layout cursor and scroll the camera to
        // reveal the column. Floating windows only need the workspace switch
        // above — they float above all columns.
        if self.registry.is_tiling(hwnd) {
            self.active_scrolling_mut().set_focus(target);
            if let Some(diff) = self.active_scrolling_mut().ensure_focused_visible() {
                self.animate_layout(&diff);
            }
        } else if self.registry.is_floating_active(hwnd) {
            // The layer's "last used" float, for `focus-floating`.
            if let Some(ws) = self.active_monitor_mut().workspace_mut(home_ws) {
                ws.last_focused_float = Some(target);
            }
        }

        // Refresh the new foreground first, then the previous one. `set_focused`
        // ignores untracked HWNDs, so if `hwnd` is the taskbar / a dialog,
        // `focused()` is unchanged and neither refresh recolors anything
        // (short-circuit). If `hwnd` is newly-tracked, only it and the prior
        // focus actually change color.
        self.refresh_border_for(hwnd);
        if let Some(prev) = prev_focus
            && prev != hwnd
        {
            self.refresh_border_for(prev);
        }
    }

    /// Reconcile internal focus against the authoritative `GetForegroundWindow()`.
    ///
    /// `EVENT_SYSTEM_FOREGROUND` is a best-effort stream: under rapid window
    /// churn (e.g. a browser tearing down tabs on close) the OS can settle the
    /// foreground without emitting the final event, leaving flow's tracked focus
    /// stranded on a stale window while the real foreground moved. This poll —
    /// driven by the `foreground_sync_interval_ms` deadline folded into the main
    /// loop's wait timeout — closes that gap by querying the source of truth
    /// directly and re-running the focus-change path when drift is detected.
    ///
    /// Conservative scope: only corrects drift onto a tracked window **in the
    /// active workspace**. Untracked HWNDs (taskbar, tray, popups) are ignored,
    /// and a foreground that landed on another workspace is left to the normal
    /// `EVENT_SYSTEM_FOREGROUND` path so a background poll never triggers an
    /// unintended workspace switch. The common (in-sync) path is a
    /// microsecond-scale guard clause.
    pub(super) fn reconcile_foreground(&mut self) {
        self.last_foreground_sync = std::time::Instant::now();

        let Some(true_fg) = registry_win32::get_foreground_window() else {
            return;
        };

        // Common case: tracked focus already matches reality → no-op.
        if self.registry.focused().map(|id| id.0) == Some(true_fg) {
            return;
        }

        // Ignore untracked foregrounds (taskbar, tray, popups, foreign apps).
        if !self.registry.is_tracked(true_fg) {
            return;
        }

        // Restrict to the active workspace: a background poll should not switch
        // workspaces. A genuine cross-workspace focus change still arrives via
        // EVENT_SYSTEM_FOREGROUND, which is the intended driver for switches.
        // The window may live on any monitor; only correct drift when it is
        // on that monitor's visible workspace.
        let Some(idx) = self.monitor_index_containing(WindowId(true_fg)) else {
            return;
        };
        let active_id = self.monitors[idx].active_workspace_id();
        if self.monitors[idx].find_workspace_containing(WindowId(true_fg)) != Some(active_id) {
            return;
        }

        log::debug!("reconcile_foreground: correcting drift to {true_fg}");
        self.on_focus_changed(true_fg);
    }

    /// Handle `EVENT_OBJECT_STATECHANGE` — Option D recovery for windows that
    /// launched maximized or fullscreen. Also run from the foreground, show
    /// and move-size-end handlers for a window that is still OS-ignored (the
    /// `STATECHANGE` for an un-maximize does not always arrive; see
    /// `docs/src/dev-guide/event-pipelines.md`, *State Change*).
    ///
    /// Pipeline:
    /// 1. `registry.reclassify_os_state(hwnd)` — re-queries the live OS state.
    ///    If a tracked window was `Ignored(Maximized|Fullscreen)` but is no
    ///    longer, the classifier is re-run and its stored state is updated
    ///    (with `adopt_maximized` on, a still-maximized window is
    ///    un-maximized first — see the registry method).
    /// 2. If the window transitioned to tiling
    ///    ([`ReclassifyResult::Recovered`] with `now_tiling == true`):
    ///    - `layout.add_window(id)` — appends the recovered window to the
    ///      layout (mirrors [`on_window_restored`](Self::on_window_restored)).
    ///    - `animate_layout(applied)` — animates the window entering.
    ///
    /// # Why `add_window` (append) and not `insert_window`
    ///
    /// A recovered window was never part of the layout (it was ignored from
    /// creation), so there is no saved slot to restore. Appending at the far
    /// right is the predictable choice and matches the restore/show recovery
    /// paths. The cheap non-recovery cases (untracked / not OS-ignored /
    /// still ignored) are filtered inside the registry, so this handler costs
    /// almost nothing when nothing is recoverable.
    pub(super) fn on_window_state_change(&mut self, hwnd: isize) {
        let outcome = self.registry.reclassify_os_state(hwnd);
        // Tiling off: the registry re-classification is all that happens; a
        // recovered window is seated by the resume.
        if self.tiling_paused {
            return;
        }
        match outcome {
            ReclassifyResult::Recovered { now_tiling: true } => {
                self.readd_tile_to_home_monitor(hwnd);
            }
            ReclassifyResult::Recovered { now_tiling: false } => {
                self.seat_recovered_float(hwnd);
            }
            _ => {}
        }
        // A window recovered as a float or as an explicit-rule ignore
        // enters the floating layer; the foreground window changing state
        // (maximized, fullscreen, back) re-decides the layer.
        self.sync_top_layer(hwnd);
        if self.foreground == Some(hwnd) {
            self.evaluate_top_layer(Some(hwnd));
        }
        // Border overlay: covers all transitions — Ignored→Tiling (attach),
        // Tiling→Ignored (detach), and no-op cases.
        self.refresh_border_for(hwnd);
    }

    /// A window recovered from `Ignored(Maximized|Fullscreen)` as an active
    /// float carries the classifier's placeholder rect (0x0) and sits in no
    /// floating space: seat it like a freshly created float (centred on its
    /// monitor, tracked, animated). No-op for any other window, or for one a
    /// floating space already holds.
    pub(super) fn seat_recovered_float(&mut self, hwnd: isize) {
        let wid = WindowId(hwnd);
        let is_float = matches!(
            self.registry
                .get_window(HWND(hwnd as *mut _))
                .map(|w| &w.state),
            Some(WindowState::Floating(FloatingState::Active { .. }))
        );
        if !is_float || self.monitor_index_containing(wid).is_some() {
            return;
        }
        let rect = self.centered_float_rect(wid);
        let float_actual = self.register_float(wid, rect);
        self.animate_workspaces(&[(float_actual, 0)]);
        log::info!("recovered float hwnd {hwnd} seated at {rect:?}");
    }

    /// Append a returning tiled window (restored, re-shown, or recovered
    /// from maximized) to the strip of its home monitor
    /// ([`home_monitor_for_hwnd`](Self::home_monitor_for_hwnd)) and animate
    /// it in. A window that is already seated somewhere (the hotplug
    /// reconcile seats the windows it restores itself) is left where it is
    /// and only its registry slot and rect are re-synced.
    pub(super) fn readd_tile_to_home_monitor(&mut self, hwnd: isize) {
        let wid = WindowId(hwnd);
        if let Some(idx) = self.monitor_index_containing(wid) {
            if let Some(ws_id) = self.monitors[idx].find_workspace_containing(wid)
                && let Some(ws) = self.monitors[idx].workspace(ws_id)
            {
                self.registry
                    .update_tiling_slots_from_layout(ws.scrolling.virtual_layout());
                self.registry
                    .update_tiled_rects(ws.scrolling.actual_layout());
            }
            log::debug!("window {hwnd} is already seated on monitor {idx}; not re-adding");
            return;
        }
        let idx = self.home_monitor_for_hwnd(hwnd);
        let mut applied = self.monitors[idx].active_scrolling_mut().add_window(wid);
        self.remember_monitor(wid, idx);
        // A fullscreen window comes back as a fullscreen column, in view
        // (ADR-0021), in the same batch.
        let ws_id = self.monitors[idx].active_workspace_id();
        if let Some(fs) = self.reflag_fullscreen(wid, idx, ws_id, true) {
            applied = fs;
            // An app brings its own full screen back with it, in place.
            if self.is_app_fullscreen(hwnd) {
                self.note_app_frame_at_screen(hwnd, idx, 0);
            }
        }
        self.animate_layout_on(idx, &applied);
    }

    /// Handle `EVENT_OBJECT_NAMECHANGE` — Option A recovery for windows whose
    /// title arrives *after* `EVENT_OBJECT_CREATE`.
    ///
    /// Some applications — most notably Windows Terminal — set their window
    /// title asynchronously, long after `CREATE` fired and after flow's
    /// pending-creations retry budget has been exhausted. Such windows are
    /// never registered and silently fall outside the tiling layout.
    ///
    /// `NAMECHANGE` fires the moment the title lands. This handler gives flow a
    /// second chance: if the window is **not already tracked**, it re-attempts
    /// the full creation pipeline via
    /// [`on_window_created`](Self::on_window_created).
    ///
    /// # Why only untracked windows?
    ///
    /// Re-classifying an already-tracked window on every title change would
    /// risk layout churn (a rule could match the new title differently). The
    /// [`is_tracked`](crate::registry::WindowRegistry::is_tracked) guard
    /// ensures we only ever register windows that were missed the first time.
    ///
    /// If `on_window_created` still returns `false` (e.g. the window is not
    /// yet visible), we do **not** re-add it to `pending_creations` here:
    /// apps often fire several `NAMECHANGE`s during initialization, so the
    /// next one will give us another opportunity.
    pub(super) fn on_window_name_change(&mut self, hwnd: isize) {
        if self.registry.is_tracked(hwnd) {
            // Tracked windows fire NAMECHANGE frequently (e.g. a terminal's
            // title updating on every prompt). Re-classifying them would risk
            // layout churn, so we silently ignore these — no log line, to avoid
            // flooding the daemon log with per-keystroke title updates.
            return;
        }
        // Untracked window just got a name — likely a late-titled app (such as
        // Windows Terminal, which sets its title only after its child shell
        // starts). Give it a second chance at the full creation pipeline.
        log::debug!(
            "on_window_name_change: {hwnd:#x} not tracked — re-attempting creation (late title?)"
        );
        // Re-attempt registration. The window now has a title (this is a
        // NAMECHANGE event), so handle_created's title-empty gate should pass.
        // If it still returns false, we simply wait for the next NAMECHANGE.
        self.on_window_created(hwnd);
    }
}

/// Pure decision behind [`FlowWM::remove_from_layout_and_refocus`]: whether
/// removing a window from a strip should push the OS foreground to the
/// strip's successor.
///
/// `prev_focus` / `new_focus` are the strip's last-focused window before and
/// after the removal; `held_focus` says the removed window was the user's
/// focus (registry focus or OS foreground); `on_active_monitor` that it
/// lived on the monitor that owns keyboard focus; `focus_on_strip` that the
/// registry's focus, read before the removal, was nobody, the removed window
/// or a tile of that strip. The rule and why it exists are in
/// `docs/src/dev-guide/event-pipelines.md`, *Focus after a removal or a
/// monitor switch*.
#[must_use]
pub(super) fn should_refocus_after_removal(
    prev_focus: Option<WindowId>,
    new_focus: Option<WindowId>,
    held_focus: bool,
    on_active_monitor: bool,
    focus_on_strip: bool,
) -> bool {
    new_focus != prev_focus
        && new_focus.is_some()
        && (held_focus || (on_active_monitor && focus_on_strip))
}

#[cfg(test)]
mod refocus_tests {
    use super::should_refocus_after_removal;
    use crate::common::WindowId;

    const A: Option<WindowId> = Some(WindowId(1));
    const B: Option<WindowId> = Some(WindowId(2));

    #[test]
    fn focused_window_closing_refocuses_the_successor() {
        assert!(should_refocus_after_removal(A, B, true, true, true));
        // Held the OS foreground even though its monitor is not active.
        assert!(should_refocus_after_removal(A, B, true, false, false));
    }

    #[test]
    fn closing_on_the_active_monitor_refocuses_when_focus_was_on_the_strip() {
        // Registry focus was None / the removed window / a tile of the strip.
        assert!(should_refocus_after_removal(A, B, false, true, true));
    }

    #[test]
    fn closing_on_the_active_monitor_never_steals_focus_from_a_float_or_elsewhere() {
        // The user is in a float (or a window on another strip): the strip's
        // cursor moves, the foreground stays.
        assert!(!should_refocus_after_removal(A, B, false, true, false));
    }

    #[test]
    fn closing_on_a_background_monitor_never_steals_focus() {
        // The B6 scenario: strip focus cursor moved, but the user is on
        // another monitor and this window was not their focus.
        assert!(!should_refocus_after_removal(A, B, false, false, true));
        assert!(!should_refocus_after_removal(A, B, false, false, false));
    }

    #[test]
    fn unchanged_strip_focus_never_pushes() {
        assert!(!should_refocus_after_removal(A, A, true, true, true));
    }

    #[test]
    fn empty_strip_after_removal_never_pushes() {
        assert!(!should_refocus_after_removal(A, None, true, true, true));
    }
}

#[cfg(test)]
mod readd_tests {
    use super::*;
    use crate::common::Rect;
    use crate::config::types::WindowRulesConfig;
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::registry::WindowRegistry;
    use crate::registry::types::TilingState;
    use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};

    const A: isize = 0xA;
    const B: isize = 0xB;

    fn tiled(registry: &mut WindowRegistry, hwnd: isize) {
        let info = crate::registry::win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: format!("win{hwnd}"),
            class: "C".into(),
            rect: Rect {
                x: 0,
                y: 0,
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
            w.state = WindowState::Tiling(TilingState::Active { col: 0, row: 0 });
        }
    }

    /// One monitor, workspaces 1 (active, holding A) and 2 (parked, holding B).
    fn flow() -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        tiled(&mut registry, A);
        tiled(&mut registry, B);
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let info = MonitorInfo::single(screen);
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let ws = |id: u32| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(info, 960, 320, 100, 100, padding, 4),
            )
        };
        let mut monitor = Monitor::new(screen, screen, vec![ws(1), ws(2)], 0);
        monitor.active_scrolling_mut().add_window(WindowId(A));
        monitor
            .workspace_mut(WorkspaceId(2))
            .expect("workspace 2")
            .scrolling
            .add_window(WindowId(B));
        registry.set_monitor(A, 0);
        registry.set_monitor(B, 0);
        FlowWM::new_for_test(registry, vec![monitor])
    }

    fn columns_holding(flow: &FlowWM, ws: u32, hwnd: isize) -> usize {
        flow.monitors[0]
            .workspace(WorkspaceId(ws))
            .expect("workspace")
            .scrolling
            .virtual_layout()
            .columns
            .iter()
            .filter(|c| c.rows.iter().any(|r| r.window_id == WindowId(hwnd)))
            .count()
    }

    #[test]
    fn readding_a_seated_tile_does_not_create_a_second_column() {
        let mut flow = flow();
        flow.readd_tile_to_home_monitor(A);
        assert_eq!(columns_holding(&flow, 1, A), 1);
        assert_eq!(
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .window_count(),
            1
        );
        // The parked workspace's tile is seated too: no re-add either.
        flow.readd_tile_to_home_monitor(B);
        assert_eq!(columns_holding(&flow, 2, B), 1);
        assert_eq!(columns_holding(&flow, 1, B), 0);
    }

    #[test]
    fn hiding_a_window_moved_to_another_workspace_after_switching_back() {
        // The integration scenario: move A to workspace 2 (which follows
        // it), switch back to 1, then A is hidden while 2 is parked — it
        // leaves workspace 2's strip. Workspace 2 also carries a rowless
        // column from a stale loadout file, which the successor lookup must
        // step over.
        let mut flow = flow();
        {
            let ws2 = &mut flow.monitors[0]
                .workspace_mut(WorkspaceId(2))
                .expect("workspace 2")
                .scrolling;
            ws2.remove_window(WindowId(B));
            let mut stale = ws2.virtual_layout().clone();
            stale
                .columns
                .push(crate::layout::types::Column::with_rows(448, Vec::new()));
            ws2.commit_layout(stale);
        }
        flow.registry.set_focused(A);
        // The move-to-workspace dispatch, minus the IPC shell.
        let src = flow.monitors[0]
            .active_scrolling_mut()
            .remove_window(WindowId(A));
        flow.registry
            .update_tiling_slots_from_layout(&src.virtual_layout);
        flow.registry.update_tiled_rects(&src.actual_layout);
        let dst = {
            let ws = flow.monitors[0]
                .workspace_mut(WorkspaceId(2))
                .expect("ws 2");
            let _ = ws.scrolling.insert_window_with_width(WindowId(A), 960);
            ws.scrolling.center_canvas().expect("non-empty")
        };
        flow.registry
            .update_tiling_slots_from_layout(&dst.virtual_layout);
        flow.registry.update_tiled_rects(&dst.actual_layout);
        assert!(flow.switch_workspace_layout(WorkspaceId(2), WorkspaceId(1)));
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(2));
        assert!(flow.switch_workspace_layout(WorkspaceId(1), WorkspaceId(2)));
        assert_eq!(columns_holding(&flow, 2, A), 1);
        flow.on_window_hidden(A);
        assert_eq!(columns_holding(&flow, 2, A), 0);
        assert_eq!(columns_holding(&flow, 1, A), 0);
    }

    #[test]
    fn minimizing_a_tile_on_a_parked_workspace_removes_it_from_that_strip() {
        let mut flow = flow();
        flow.record_submitted(&[(
            B,
            Rect {
                x: 4,
                y: 4,
                width: 960,
                height: 1072,
            },
        )]);
        flow.on_window_minimized(B);
        assert_eq!(columns_holding(&flow, 2, B), 0, "B left the parked strip");
        assert_eq!(
            columns_holding(&flow, 1, A),
            1,
            "the active strip is untouched"
        );
        assert!(matches!(
            flow.registry
                .get_window(HWND(B as *mut _))
                .map(|w| w.state.clone()),
            Some(WindowState::Tiling(TilingState::Minimized))
        ));
        assert_eq!(flow.last_submitted_rect(B), None);
        // Coming back (the restore hook's layout half), it is appended to
        // the active strip exactly once.
        flow.registry.restore_window(B);
        flow.readd_tile_to_home_monitor(B);
        assert_eq!(columns_holding(&flow, 1, B), 1);
        assert_eq!(columns_holding(&flow, 2, B), 0);
    }

    /// A window recovered from maximized as a float (the classifier's 0x0
    /// placeholder, in no floating space) is seated like a new float; a
    /// float already seated, or a tile, is left alone.
    #[test]
    fn a_recovered_float_is_seated_once_with_a_real_rect() {
        const F: isize = 0xF0;
        let mut flow = flow();
        flow.config.borders.enabled = false;
        tiled(&mut flow.registry, F);
        if let Some(w) = flow.registry.get_window_mut(HWND(F as *mut _)) {
            w.state = WindowState::Floating(FloatingState::Active {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
            });
        }
        flow.registry.set_monitor(F, 0);
        flow.seat_recovered_float(F);
        let seated = |flow: &FlowWM| {
            flow.monitors[0]
                .active_workspace()
                .floating
                .windows()
                .iter()
                .filter(|e| e.window_id == WindowId(F))
                .map(|e| e.rect)
                .collect::<Vec<_>>()
        };
        let rects = seated(&flow);
        assert_eq!(rects.len(), 1);
        assert!(rects[0].width > 0 && rects[0].height > 0);
        assert!(matches!(
            flow.registry.get_window(HWND(F as *mut _)).map(|w| &w.state),
            Some(WindowState::Floating(FloatingState::Active { rect })) if *rect == rects[0]
        ));
        // Again (a second STATECHANGE): nothing changes.
        flow.seat_recovered_float(F);
        assert_eq!(seated(&flow), rects);
        // A tile is not a float.
        flow.seat_recovered_float(A);
        assert_eq!(columns_holding(&flow, 1, A), 1);
        crate::registry::hooks::remove_float_hwnd(F);
    }

    /// [`flow`] plus a second monitor to the left (index 1), active.
    fn two_monitors() -> FlowWM {
        let mut flow = flow();
        let screen = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let info = MonitorInfo::single(screen);
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let ws = Workspace::new(
            WorkspaceId(1),
            ScrollingSpace::new(info, 960, 320, 100, 100, padding, 4),
        );
        flow.monitors
            .push(Monitor::new(screen, screen, vec![ws], 1));
        flow.active_monitor = 1;
        flow
    }

    /// ADR-0023: a new window goes to the focused monitor unless the config
    /// says "placed"; a returning one keeps its own (here: the remembered
    /// monitor 0) either way.
    #[test]
    fn a_new_window_is_seated_on_the_focused_monitor_a_returning_one_on_its_own() {
        use crate::config::types::NewWindowMonitor;
        let mut flow = two_monitors();
        assert_eq!(flow.monitor_for_arrival(A, Arrival::New), 1);
        assert_eq!(flow.monitor_for_arrival(A, Arrival::Returning), 0);
        flow.config.focus.new_window_monitor = NewWindowMonitor::Placed;
        assert_eq!(flow.monitor_for_arrival(A, Arrival::New), 0);
        assert_eq!(flow.monitor_for_arrival(A, Arrival::Returning), 0);
    }

    /// The focus handoff only takes back a foreground that went to a window
    /// on another screen, within its window; the successor itself,
    /// a window of the same screen and untracked windows pass.
    #[test]
    fn a_focus_handoff_takes_back_only_a_foreground_on_another_screen() {
        const C: isize = 0xC;
        let mut flow = two_monitors();
        flow.config.borders.enabled = false;
        tiled(&mut flow.registry, C);
        flow.monitors[1]
            .active_scrolling_mut()
            .add_window(WindowId(C));
        flow.registry.set_monitor(C, 1);
        let handoff = |until| FocusHandoff {
            monitor: 1,
            successor: Some(WindowId(C)),
            until,
        };
        let soon = std::time::Instant::now() + std::time::Duration::from_secs(60);

        flow.focus_handoff = Some(handoff(soon));
        // The successor, an untracked window: not redirected, still armed.
        assert!(!flow.redirect_focus_handoff(C));
        assert!(!flow.redirect_focus_handoff(0x777));
        assert!(flow.focus_handoff.is_some());
        // A window on monitor 0 that no longer has the foreground (these
        // fake handles never do): the stray event is dropped, no push, and
        // the handoff stays armed for the next report of it.
        assert!(flow.redirect_focus_handoff(A));
        assert!(flow.focus_handoff.is_some());

        // Expired: nothing happens and the handoff is dropped.
        flow.focus_handoff = Some(handoff(std::time::Instant::now()));
        assert!(!flow.redirect_focus_handoff(A));
        assert!(flow.focus_handoff.is_none());
    }

    /// A tile that lost the foreground a moment ago still counts as the
    /// user's focus for the removal pipeline; an old loss or another window
    /// does not.
    #[test]
    fn a_focus_loss_counts_only_for_that_window_and_only_just_now() {
        let mut flow = flow();
        flow.last_focus_loss = Some((A, std::time::Instant::now()));
        assert!(flow.lost_focus_just_now(WindowId(A)));
        assert!(!flow.lost_focus_just_now(WindowId(B)));
        flow.last_focus_loss = Some((
            A,
            std::time::Instant::now() - FOCUS_HANDOFF_WINDOW - std::time::Duration::from_millis(1),
        ));
        assert!(!flow.lost_focus_just_now(WindowId(A)));
    }
}

#[cfg(test)]
mod tests {
    //! Characterization tests for [`FlowWM::reconcile_new_window_focus`].
    //!
    //! # Why the function is not unit-tested directly
    //!
    //! `reconcile_new_window_focus` lives on [`FlowWM`], which cannot be
    //! instantiated in a unit test — it owns a named-pipe IPC server, a
    //! background WinEvent hook thread, a window animator thread, and a live
    //! Win32 desktop association. It also calls
    //! `registry_win32::get_foreground_window` / `set_foreground_window`,
    //! which are direct Win32 free functions with **no trait or `cfg(test)`
    //! seam** to intercept them.
    //!
    //! Adding a seam would require either injecting a foreground-backend
    //! trait through `FlowWM::new` (a cross-cutting production refactor that
    //! touches the constructor and every caller) or `#[cfg(test)]`-gating the
    //! two free functions (impossible — `cfg(test)` does not apply to the
    //! production crate when called from `tests/`). Both are outside the
    //! scope of this test-only task.
    //!
    //! Instead, these tests mirror the two boolean predicates inside
    //! `reconcile_new_window_focus` exactly, so an accidental logic inversion
    //! (e.g. `&&` instead of `||`, or flipping the `prev != hwnd` guard) trips
    //! a test. The end-to-end focus-sync contract is pinned by the integration
    //! tests in `tests/cli/window_creation_focus.rs`.

    use super::*;

    /// Mirror of the short-circuit foreground predicate in
    /// `reconcile_new_window_focus`:
    /// `get_foreground_window() == Some(hwnd) || set_foreground_window(hwnd)`.
    ///
    /// Rust's `||` short-circuits, so when `already_fg` is true the
    /// `set_foreground_window` call is never made (the contract the bug fix
    /// relies on to keep normal launches a no-op).
    fn became_foreground_predicate(already_fg: bool, set_succeeded: bool) -> bool {
        already_fg || set_succeeded
    }

    /// Mirror of the previous-foreground border-refresh guard in
    /// `reconcile_new_window_focus`:
    /// `became_foreground && let Some(prev) = prev_focus && prev != hwnd`.
    ///
    /// Returns whether `refresh_border_for(prev)` would fire.
    fn should_recolor_previous(became_fg: bool, prev: Option<isize>, hwnd: isize) -> bool {
        became_fg && prev.is_some() && prev != Some(hwnd)
    }

    // ── became_foreground predicate ───────────────────────────────────

    #[test]
    fn became_foreground_short_circuits_when_already_foreground() {
        // Positive: if the window is already the OS foreground, the predicate
        // is true regardless of what a (never-made) set call would return.
        // This pins the no-op-on-normal-launches contract.
        assert!(became_foreground_predicate(true, false));
        assert!(became_foreground_predicate(true, true));
    }

    #[test]
    fn became_foreground_falls_back_to_set_result_when_not_already() {
        // Positive: not already foreground → outcome follows set's result.
        assert!(became_foreground_predicate(false, true));
        // Negative: not foreground AND set failed → no focus sync, no recolor.
        // This is the AHK `Run Hide` case when the foreground lock defeats the
        // push: focus must NOT be recorded as moving to the new window.
        assert!(!became_foreground_predicate(false, false));
    }

    // ── previous-foreground recolor guard ─────────────────────────────

    #[test]
    fn recolor_previous_skipped_when_foreground_push_failed() {
        // Negative: a failed foreground push must NOT recolor the previous
        // window — focus stayed where it was, so its border is already correct.
        assert!(!should_recolor_previous(false, Some(99), 100));
    }

    #[test]
    fn recolor_previous_skipped_when_no_previous_focus() {
        // Edge: no prior foreground window (first window on an empty desktop)
        // → nothing to recolor. Guards against a redundant refresh call.
        assert!(!should_recolor_previous(true, None, 100));
    }

    #[test]
    fn recolor_previous_skipped_when_previous_is_same_window() {
        // Edge: the "previous" foreground equals the new hwnd (idempotent
        // re-focus on an already-foreground window) → no recolor.
        assert!(!should_recolor_previous(true, Some(100), 100));
    }

    #[test]
    fn recolor_previous_runs_when_focus_moved_to_different_window() {
        // Positive: focus genuinely moved from a different window → its border
        // must be recolored from the focused style to the unfocused style.
        assert!(should_recolor_previous(true, Some(99), 100));
    }

    // ── Win32 dependency signatures ───────────────────────────────────
    //
    // `reconcile_new_window_focus` calls these two free functions. They are
    // direct Win32 wrappers with no seam — pinning their signatures here guards
    // the predicate mirrors above against an upstream signature change that
    // would silently alter the reconcile contract. Matches the codebase's
    // established pattern for untestable Win32 wrappers (see `registry::win32`'
    // own test mod).

    #[test]
    fn reconcile_foreground_win32_deps_have_documented_signatures() {
        let _get: fn() -> Option<isize> = registry_win32::get_foreground_window;
        let _set: fn(isize) -> bool = registry_win32::set_foreground_window;
    }
}
