//! Animation bridge — converts layout snapshots to animation targets.
//!
//! This module contains:
//!
//! - [`FlowWM::animate_layout`] — converts an [`AppliedLayout`]
//!   into animation targets and submits to the animator.
//! - [`FlowWM::animate_workspaces`] — multi-workspace variant
//!   that submits a single combined batch with per-workspace vertical
//!   `y_offset` applied (used by `SwitchWorkspace` / `MoveWindowToWorkspace`).
//! - [`FlowWM::teleport_workspaces`] — bypass-animator variant
//!   that directly `SetWindowPos`-es bystander workspaces into place with no
//!   animation, used to maintain the workspace stacking invariant during
//!   switches without visual noise.
//! - [`animate_layout_raw`] — standalone version used during construction when
//!   `FlowWM` doesn't exist yet.

use std::collections::HashMap;

use windows::Win32::Foundation::HWND;

use crate::animation::{IVec2, WindowAnimator, WindowRef, WindowTarget};
use crate::common::Rect;
use crate::layout::types::{ActualLayout, AppliedLayout, MonitorInfo};
use crate::registry::WindowRegistry;
use crate::registry::types::{FloatingState, Window, WindowState};
use crate::workspace::WorkspaceParking;

use super::types::FlowWM;

/// Where the bridge reads each window's **pre-batch** rect from when it
/// decides whether a tween would cross a neighbouring monitor
/// ([`plan_move`]).
///
/// The decision compares where the window is now with where the layout puts
/// it, so the "now" rect must predate the mutation being animated. Callers
/// that sync the registry *after* submitting use [`Registry`](Self::Registry);
/// callers that have already synced it (the loadout apply path) capture the
/// rects first and pass [`Snapshot`](Self::Snapshot).
///
/// The registry's rects are **workspace-local**: a window in a parked
/// workspace is recorded at its on-screen layout rect, while it really sits
/// one parking offset above or below. [`Registry`](Self::Registry) is
/// therefore only right for windows of the *active* workspace (offset 0 —
/// the float toggles). Anything that moves a parked workspace (a switch,
/// the loadout seating) must pass a
/// [`FlowWM::snapshot_stacked_prev_rects`] snapshot, which applies the
/// current parking offsets; otherwise a window coming back from behind a
/// stacked neighbour reads as on-screen → on-screen and tweens across it.
///
/// Both sources are only consulted for a window that has **no** entry in
/// [`FlowWM::submitted_rects`]: the rect a previous submit (committed or
/// not) last commanded always wins, see
/// [`FlowWM::last_submitted_rect`].
#[derive(Debug, Clone, Copy)]
pub(super) enum PrevRects<'a> {
    /// The registry's tiled / float rects still describe the pre-batch
    /// positions.
    Registry,
    /// Pre-batch rects captured by the caller, keyed by HWND value.
    Snapshot(&'a HashMap<isize, Rect>),
}

impl PrevRects<'_> {
    /// The pre-batch rect of `hwnd`, if known.
    fn rect_for(self, hwnd: isize, window: Option<&Window>) -> Option<Rect> {
        match self {
            PrevRects::Registry => registry_prev_rect(window),
            PrevRects::Snapshot(map) => map.get(&hwnd).copied(),
        }
    }
}

/// The rect the registry says `window` currently occupies: its tiled rect
/// while tiled, its float rect while floating, nothing otherwise.
fn registry_prev_rect(window: Option<&Window>) -> Option<Rect> {
    let w = window?;
    match w.state {
        WindowState::Tiling(_) => w.tiled_rect,
        WindowState::Floating(FloatingState::Active { rect }) => Some(rect),
        _ => None,
    }
}

impl FlowWM {
    /// Convert an [`AppliedLayout`] into animation targets and submit to the animator.
    ///
    /// This is the critical conversion point between the layout engine's output
    /// (flow types) and the animation system's input (animation types):
    ///
    /// | flow Type | Animation Type |
    /// |----------|---------------|
    /// | `WindowId(isize)` | `WindowRef(isize)` |
    /// | `Rect { x, y, width, height }` position | `IVec2::new(x, y)` |
    /// | `Rect { x, y, width, height }` size | `IVec2::new(width, height)` |
    ///
    /// Also synchronizes the registry's tiling state (col/row indices and tiled
    /// rects) from the new layout. This happens even when there are no animation
    /// moves — a swap can change a window's logical position without triggering
    /// a pixel-level move if the swapped columns have the same width.
    ///
    /// # Why pass ALL windows (not just "changed" ones)
    ///
    /// Targets are built from **every** entry in `actual_layout`, not just the
    /// windows whose logical position changed. The animator's `build_tweens`
    /// compares each target rect against the window's real on-screen position
    /// and drops no-ops (windows already at their target). This ensures that
    /// windows which are still mid-flight from a previous (interrupted) animation
    /// are correctly retargeted even when their target rect didn't change in
    /// this mutation — fixing the "rapid swap-column stranding" bug.
    ///
    /// Animation errors are logged as warnings but not propagated — a jarring
    /// animation is better than a crash.
    ///
    /// # Visible-Rect to Window-Rect Translation
    ///
    /// The layout engine computes **visible rects** — the coordinates of the
    /// content area the user actually sees. However, `SetWindowPos` (called by
    /// the Win32 animation backend) positions windows using **window rects** —
    /// the full rect from `GetWindowRect` which includes invisible borders
    /// (shadows, resize hit-test areas).
    ///
    /// This method translates each target's rect from visible-rect space to
    /// window-rect space using the window's stored
    /// [`InvisibleBounds`](crate::common::InvisibleBounds). Without this
    /// translation, windows would appear with gaps larger than configured
    /// because `SetWindowPos` would place the *outer* edge at the intended
    /// *inner* edge position.
    ///
    /// This also fixes animation correctness: the animator's retarget path
    /// queries `GetWindowRect` for the current `from` position (window-rect
    /// space). If `to` were in visible-rect space, interpolation between
    /// mismatched coordinate spaces would cause visual distortion during
    /// the animation.
    pub(super) fn animate_layout(&mut self, layout: &AppliedLayout) {
        self.animate_layout_on(self.active_monitor, layout);
    }

    /// Animate a layout produced by `monitor_idx`'s scrolling space.
    ///
    /// Registry bookkeeping (tiling slots + tiled rects) is refreshed so the
    /// registry knows where every window is after the layout change. The
    /// submission happens *before* the tiled rects are updated so the
    /// teleport decision can compare each window's previous rect with its
    /// new one.
    pub(super) fn animate_layout_on(&mut self, monitor_idx: usize, layout: &AppliedLayout) {
        self.animate_layouts_on(&[(monitor_idx, layout)]);
    }

    /// Animate several layouts (one per monitor) as a **single** animator
    /// batch. The animator supersedes an in-flight batch when a new one is
    /// submitted, so two monitors changed by one command (a window moving
    /// between them) must be submitted together or the first monitor's
    /// windows stop wherever the second submission caught them.
    pub(super) fn animate_layouts_on(&mut self, layouts: &[(usize, &AppliedLayout)]) {
        let mut targets: Vec<WindowTarget> = Vec::new();
        let mut submitted: Vec<(isize, Rect)> = Vec::new();
        for (idx, layout) in layouts {
            self.collect_targets(layout, *idx, &mut targets, &mut submitted);
        }
        if !targets.is_empty() {
            log::debug!(
                "animate_layouts_on: submitting {} targets across {} layout(s)",
                targets.len(),
                layouts.len()
            );
            if let Err(e) = self.animator.animate(targets) {
                log::warn!("animation error: {e}");
            }
        }
        self.record_submitted(&submitted);
        for (_, layout) in layouts {
            self.registry
                .update_tiling_slots_from_layout(&layout.virtual_layout);
            self.registry.update_tiled_rects(&layout.actual_layout);
        }
    }

    /// Submit animation targets for `layout` WITHOUT syncing registry state or
    /// committing to [`ScrollingSpace`](crate::workspace::ScrollingSpace).
    ///
    /// The pure-animation core shared by [`animate_layout`] (which syncs the
    /// registry first) and [`animate_preview`] (which must not touch the
    /// registry, since the preview is non-committing and reversible).
    ///
    /// Honors the active-drag exclusion filter: while `drag_state` is `Some`,
    /// the dragged window and its border overlay are omitted from the target
    /// list so the animator does not fight the user's mouse. When `on_drag_end`
    /// calls [`animate_layout`], `drag_state` has already been `take()`n so the
    /// filter does not trigger — the dragged window animates to its final
    /// position as intended.
    fn submit_animation(&mut self, layout: &AppliedLayout, monitor_idx: usize) {
        let mut targets: Vec<WindowTarget> = Vec::new();
        let mut submitted: Vec<(isize, Rect)> = Vec::new();
        self.collect_targets(layout, monitor_idx, &mut targets, &mut submitted);
        self.record_submitted(&submitted);
        if targets.is_empty() {
            return;
        }
        log::debug!(
            "animate_layout: submitting {} targets to animator",
            targets.len()
        );
        if let Err(e) = self.animator.animate(targets) {
            log::warn!("animation error: {e}");
        }
    }

    /// The rect the last submit commanded for `hwnd`, if any — see
    /// [`submitted_rects`](FlowWM::submitted_rects). This is the pre-batch
    /// rect every target builder reads first; a committed registry rect or a
    /// caller's snapshot only fills in for a window never submitted.
    #[must_use]
    pub(super) fn last_submitted_rect(&self, hwnd: isize) -> Option<Rect> {
        self.submitted_rects.get(&hwnd).copied()
    }

    /// Record the visible rects a submit path just commanded (see
    /// [`submitted_rects`](FlowWM::submitted_rects)).
    pub(super) fn record_submitted(&mut self, submitted: &[(isize, Rect)]) {
        for &(hwnd, rect) in submitted {
            self.submitted_rects.insert(hwnd, rect);
        }
    }

    /// Drop `hwnd`'s last-submitted rect: the window left the layout
    /// (destroyed, minimized, hidden), so the next time it is seated nothing
    /// may assume it still sits where it was last sent.
    pub(super) fn forget_submitted(&mut self, hwnd: isize) {
        self.submitted_rects.remove(&hwnd);
    }

    /// Append the animator targets for `layout` (produced by `monitor_idx`'s
    /// scrolling space) to `targets`, teleporting the windows whose tween
    /// would cross a neighbouring monitor, and the visible rect commanded
    /// for each window to `submitted` (for
    /// [`record_submitted`](Self::record_submitted) once the batch is sent).
    pub(super) fn collect_targets(
        &self,
        layout: &AppliedLayout,
        monitor_idx: usize,
        targets: &mut Vec<WindowTarget>,
        submitted: &mut Vec<(isize, Rect)>,
    ) {
        if layout.actual_layout.entries.is_empty() {
            return;
        }
        let monitor_info = *self.monitors[monitor_idx].active_scrolling().monitor();

        // Safety net: during a tile drag, exclude the dragged window and its
        // border overlay from animation targets so the animator doesn't fight
        // the user's mouse. This covers code paths (e.g. window Created/
        // Destroyed hooks) that call animate_layout while a drag is active.
        // When on_drag_end calls animate_layout, drag_state has already been
        // take()n so the filter does not trigger — the dragged window animates
        // to its final position as intended.
        let excluded_id = self.drag_state.as_ref().map(|ds| ds.dragged_id());
        let excluded_border_hwnd = self.drag_state.as_ref().and_then(|ds| {
            self.registry
                .get_window(HWND(ds.dragged_hwnd() as *mut _))
                .and_then(|w| w.border.as_ref())
                .map(|b| b.hwnd())
        });

        // Effective border inset. Each window's rect shrinks by
        // (thickness - overlap) px per edge so the overlay ring (the outer
        // `thickness` px of the layout slot) overlaps the visible content by
        // `overlap` px — closing the DWM hairline. overlap=0 reproduces the
        // old full-gap behavior. See `docs/src/dev-guide/borders.md`.
        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;

        for entry in &layout.actual_layout.entries {
            // Skip the dragged window during an active drag.
            if Some(entry.window_id) == excluded_id {
                continue;
            }

            // Look up the window once; extract invisible bounds and the
            // border overlay HWND from the shared reference. Option<&Window>
            // is Copy (all &T are Copy), so both .map calls below take
            // independent copies without invalidating `window`.
            let window = self.registry.get_window(HWND(entry.window_id.0 as *mut _));
            let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
            let border_hwnd = window.and_then(|w| w.border.as_ref()).map(|b| b.hwnd());

            // Skip the dragged window's border overlay.
            if let Some(eb) = excluded_border_hwnd
                && border_hwnd == Some(eb)
            {
                continue;
            }

            // Translate the layout engine's visible rect into a Win32
            // window rect. This compensates for invisible borders so
            // that SetWindowPos places the window's visible content
            // exactly where the layout engine intended. Then shrink by
            // the configured border thickness to leave room for the
            // colored overlay ring.
            let window_rect = invisible_bounds
                .visible_to_window(entry.rect)
                .inset(border_thickness - border_overlap);

            // The last commanded rect first (a preview may have moved the
            // window without committing); the registry's tiled rect — still
            // the pre-mutation one here, animate_layouts_on syncs it after
            // submitting — only for a window never submitted.
            let prev_rect = self
                .last_submitted_rect(entry.window_id.0)
                .or_else(|| registry_prev_rect(window));
            push_targets(
                targets,
                submitted,
                &monitor_info,
                entry.window_id.0,
                prev_rect,
                entry.rect,
                window_rect,
                border_hwnd,
            );
        }
    }

    /// Pre-batch rects of every tracked window (tiled rect while tiled, float
    /// rect while floating), keyed by HWND value, for
    /// [`PrevRects::Snapshot`]. Capture it **before** syncing the registry to
    /// a new layout.
    pub(super) fn snapshot_prev_rects(&self) -> HashMap<isize, Rect> {
        self.registry
            .windows()
            .filter_map(|w| registry_prev_rect(Some(w)).map(|r| (w.hwnd.0 as isize, r)))
            .collect()
    }

    /// Where every tracked window **really** is right now, keyed by HWND
    /// value, for [`PrevRects::Snapshot`].
    ///
    /// Starts from [`snapshot_prev_rects`](Self::snapshot_prev_rects) and
    /// shifts every window seated in a parked workspace by that workspace's
    /// current parking offset ([`WorkspaceParking::offset`] against the
    /// monitor's active workspace), because the registry only keeps
    /// workspace-local rects. Capture it **before** changing any monitor's
    /// active workspace and before syncing the registry to a new layout.
    ///
    /// This is the prev-rect source for every submit that moves a parked
    /// workspace: without the shift, a workspace returning from behind a
    /// stacked neighbour would look like an on-screen → on-screen move and
    /// [`plan_move`] would tween it down through that neighbour.
    pub(super) fn snapshot_stacked_prev_rects(&self) -> HashMap<isize, Rect> {
        let mut rects = self.snapshot_prev_rects();
        for monitor in &self.monitors {
            let window_gap = monitor.active_scrolling().padding().window_gap;
            let parking = WorkspaceParking::new(
                monitor.active_scrolling().monitor(),
                monitor.screen_rect(),
                window_gap,
            );
            let active_id = monitor.active_workspace_id();
            for ws in monitor.workspaces() {
                let offset = parking.offset(ws.id, active_id);
                if offset == 0 {
                    continue;
                }
                let tiles = ws.scrolling.actual_layout().entries.iter();
                let floats = ws.floating.windows().iter();
                for entry in tiles.chain(floats) {
                    if let Some(r) = rects.get_mut(&entry.window_id.0) {
                        r.y += offset;
                    }
                }
            }
        }
        rects
    }

    /// Non-committing preview animation for a tile drag.
    ///
    /// During a tile drag, `on_drag_move` calls this to reflow the *other*
    /// tiled windows toward their prospective landing positions so the user can
    /// see where a release would drop the dragged window. It is
    /// **non-committing**: neither the registry nor
    /// [`ScrollingSpace`](crate::workspace::ScrollingSpace) is touched, so the
    /// next preview (or the final commit in
    /// [`on_drag_end`](super::drag::FlowWM::on_drag_end)) supersedes it. The
    /// dragged window is omitted from the target list by [`submit_animation`]'s
    /// active-drag exclusion filter, so it keeps following the mouse via the
    /// direct `border.set_geometry` calls in `on_drag_move`.
    /// (`docs/src/dev-guide/tile-drag.md`) `monitor_idx` is the monitor
    /// whose strip the preview was projected for.
    pub(super) fn animate_preview_on(&mut self, monitor_idx: usize, preview: &AppliedLayout) {
        self.submit_animation(preview, monitor_idx);
    }

    /// Submit a single combined animation batch spanning multiple workspaces.
    ///
    /// Each `(ActualLayout, y_offset)` pair describes one workspace's current
    /// on-screen layout plus the vertical parking offset to apply to every
    /// window in that workspace. All windows from all pairs are merged into a
    /// single [`Vec<WindowTarget>`] and submitted to the animator in one call.
    /// This is what makes a workspace switch animate as a single coordinated
    /// transition rather than N independent moves — the animator's worker
    /// thread builds tweens for every target simultaneously and drives them at
    /// a shared frame rate.
    ///
    /// # Where the `y_offset` is applied
    ///
    /// The offset is applied **here** in the daemon bridge, not in
    /// [`layout::projection`](crate::layout::projection). The projection layer
    /// stays pure and horizontal-only — it always produces workspace-local
    /// coordinates, and this method decides where each workspace sits
    /// vertically in the monitor stack. See
    /// [`workspace_y_offset`](crate::workspace::workspace_y_offset) for the
    /// offset computation.
    ///
    /// # When to use this vs [`animate_layout`](Self::animate_layout)
    ///
    /// - `animate_layout` — single-workspace mutation (scroll, focus, swap,
    ///   resize). One workspace, `y_offset = 0`.
    /// - `animate_workspaces` — workspace ops (switch, move-to-workspace)
    ///   where two workspaces animate in lockstep with different `y_offset`s.
    ///
    /// # Registry sync is the caller's responsibility
    ///
    /// Unlike [`animate_layout`](Self::animate_layout), this method does
    /// **not** call [`registry.update_tiling_slots_from_layout`] or
    /// [`registry.update_tiled_rects`] — those operate on a single virtual
    /// layout at a time, and the caller must invoke them per-workspace
    /// **after** this call (see [`PrevRects::Registry`]). This method only
    /// submits animation targets.
    ///
    /// The batches are taken to belong to the active monitor; use
    /// [`animate_workspaces_on`](Self::animate_workspaces_on) for another
    /// monitor or for a pre-captured set of previous rects.
    ///
    /// [`registry.update_tiling_slots_from_layout`]:
    ///     crate::registry::WindowRegistry::update_tiling_slots_from_layout
    /// [`registry.update_tiled_rects`]:
    ///     crate::registry::WindowRegistry::update_tiled_rects
    pub(super) fn animate_workspaces(&mut self, batches: &[(ActualLayout, i32)]) {
        self.animate_workspaces_on(self.active_monitor, batches, PrevRects::Registry);
    }

    /// [`animate_workspaces`](Self::animate_workspaces) for the workspaces of
    /// `monitor_idx`, reading each window's pre-batch rect from `prev` so a
    /// move that would cross a neighbouring monitor becomes an instant one
    /// ([`plan_move`]).
    pub(super) fn animate_workspaces_on(
        &mut self,
        monitor_idx: usize,
        batches: &[(ActualLayout, i32)],
        prev: PrevRects<'_>,
    ) {
        if batches.is_empty() {
            return;
        }

        let WorkspaceTargets {
            targets,
            has_float_target,
            submitted,
        } = self.workspace_targets(monitor_idx, batches, prev);
        self.record_submitted(&submitted);

        if targets.is_empty() {
            return;
        }

        // Suppress LOCATIONCHANGE capture for the animation's duration so the
        // animator's intermediate rects don't reach FloatingSpace. Only armed
        // when animation is enabled: an instant (0 ms) snap lands at the final
        // rect, so its LOCATIONCHANGE is already correct.
        if has_float_target && self.config.animation.enabled {
            self.arm_float_tracking_suppression();
        }

        log::debug!(
            "animate_workspaces: submitting {} targets across {} workspace batch(es)",
            targets.len(),
            batches.len()
        );

        if let Err(e) = self.animator.animate(targets) {
            log::warn!("animate_workspaces error: {e}");
        }
    }

    /// The animator targets for a workspace batch on `monitor_idx`, whether
    /// any of them is a tracked float window, and the visible rect commanded
    /// for each window.
    ///
    /// This is the pure half of
    /// [`animate_workspaces_on`](Self::animate_workspaces_on): every window of
    /// every `(layout, y_offset)` pair is shifted by its offset and run
    /// through [`push_targets`] with its pre-batch rect — the last submitted
    /// one, else the one `prev` supplies — so the neighbour-teleport
    /// decision is made here. Split out so tests can inspect the decisions
    /// without an animator; the caller records `submitted` once the batch
    /// is sent.
    pub(super) fn workspace_targets(
        &self,
        monitor_idx: usize,
        batches: &[(ActualLayout, i32)],
        prev: PrevRects<'_>,
    ) -> WorkspaceTargets {
        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;
        let monitor_info = *self.monitors[monitor_idx].active_scrolling().monitor();

        // Build the combined target list. Iterating with explicit loops
        // (rather than `flat_map`) lets us hold one `&self.registry` borrow
        // at a time without entangling the borrow checker across closures.
        let mut targets: Vec<WindowTarget> = Vec::new();
        let mut submitted: Vec<(isize, Rect)> = Vec::new();
        // Whether any target in this batch is a tracked float window. Used
        // after the loop to decide whether to suppress LOCATIONCHANGE capture
        // for the animation's duration.
        let mut has_float_target = false;
        for (layout, y_offset) in batches {
            for entry in &layout.entries {
                if !has_float_target && crate::registry::hooks::is_float_hwnd(entry.window_id.0) {
                    has_float_target = true;
                }
                let window = self.registry.get_window(HWND(entry.window_id.0 as *mut _));
                let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
                let border_hwnd = window.and_then(|w| w.border.as_ref()).map(|b| b.hwnd());

                // Translate visible-rect → window-rect, shrink by the border
                // inset (thickness - overlap) to make room for the ring, then apply the
                // vertical offset. Order doesn't matter mathematically (offset
                // is a pure y translation, invisible_bounds adjusts y by a
                // fixed `-top`, inset is uniform), but the chosen order keeps
                // the "workspace-local rect first, workspace stack second"
                // mental model clear.
                let shift = |r: Rect| Rect {
                    y: r.y + y_offset,
                    ..r
                };
                // A fullscreen window has no ring to make room for: it
                // covers the screen rect exactly.
                let inset = if self.is_fullscreen(entry.window_id.0) {
                    0
                } else {
                    border_thickness - border_overlap
                };
                let window_rect =
                    shift(invisible_bounds.visible_to_window(entry.rect).inset(inset));
                // The border overlay sits at the visible rect (same y_offset)
                // so it moves in lockstep with the window across the switch.
                let next = shift(entry.rect);
                let prev_rect = self
                    .last_submitted_rect(entry.window_id.0)
                    .or_else(|| prev.rect_for(entry.window_id.0, window));
                push_targets(
                    &mut targets,
                    &mut submitted,
                    &monitor_info,
                    entry.window_id.0,
                    prev_rect,
                    next,
                    window_rect,
                    border_hwnd,
                );
            }
        }
        WorkspaceTargets {
            targets,
            has_float_target,
            submitted,
        }
    }

    /// Silently teleport windows across multiple workspaces, bypassing the animator.
    ///
    /// Each `(ActualLayout, y_offset)` pair is rendered by calling
    /// [`set_window_rect`](crate::registry::win32::set_window_rect) directly
    /// for every window — an instant `SetWindowPos` with no tweening. This is
    /// the **bystander path** used during workspace switches to relocate
    /// non-participating workspaces whose parking side changed (e.g. ws 3-7
    /// when switching 2 → 8: they were below active 2 but must end up above
    /// active 8). They snap into place instantly so the user's attention
    /// stays on the two workspaces that are actually animating.
    ///
    /// # Why bypass the animator?
    ///
    /// The animator's `RetargetFromCurrent` interrupt policy would animate
    /// every retargeted window from its current position to the new target.
    /// For a 10-workspace switch that could mean eight bystander workspaces
    /// all visibly sliding across the screen at once. Teleporting them keeps
    /// the animation focused on the participant workspaces and keeps the
    /// workspace stacking invariant intact.
    ///
    /// # Registry sync is the caller's responsibility
    ///
    /// Like [`animate_workspaces`](Self::animate_workspaces), this method
    /// only submits positions to Win32. It does **not** update tiling slots
    /// or tiled rects in the registry — the caller handles that
    /// per-workspace as needed.
    ///
    /// # Error handling
    ///
    /// Per-window failures are logged at `warn` level but never propagated.
    /// A failed teleport leaves a window at its old position — a minor
    /// visual inconsistency on a non-visible workspace, not a crash — and
    /// is **not** recorded as the window's last submitted rect, so the next
    /// batch still moves it from where it really is.
    pub(super) fn teleport_workspaces(&mut self, batches: &[(ActualLayout, i32)]) {
        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;
        let mut submitted: Vec<(isize, Rect)> = Vec::new();
        for (layout, y_offset) in batches {
            for entry in &layout.entries {
                let window = self.registry.get_window(HWND(entry.window_id.0 as *mut _));
                let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();

                let window_rect = invisible_bounds
                    .visible_to_window(entry.rect)
                    .inset(border_thickness - border_overlap);
                let target_y = window_rect.y + y_offset;

                // Direct SetWindowPos — bypass the animator entirely.
                // Teleport is best-effort: a single failure must not abort
                // the rest of the batch, it only keeps the window out of the
                // submitted-rect record.
                let visible = Rect {
                    x: entry.rect.x,
                    y: entry.rect.y + y_offset,
                    width: entry.rect.width,
                    height: entry.rect.height,
                };
                if crate::registry::win32::set_window_rect(
                    entry.window_id.0,
                    window_rect.x,
                    target_y,
                    window_rect.width,
                    window_rect.height,
                ) {
                    submitted.push((entry.window_id.0, visible));
                }

                // Teleport the border overlay to match. `set_geometry` calls
                // `SetWindowPos`, which triggers `WM_SIZE` → repaint if the size
                // changed, keeping the bitmap in sync on instant repositioning.
                // Option<&Window> is Copy so re-using `window` here is sound.
                if let Some(border) = window.and_then(|w| w.border.as_ref()) {
                    border.set_geometry(visible);
                }
            }
        }
        self.record_submitted(&submitted);
    }

    /// Non-committing teleport preview for a resize drag.
    ///
    /// During a column drag-resize, [`on_resize_move`](super::drag::FlowWM::on_resize_move)
    /// calls this to relocate the *bystander* tiled windows to their boundary-move
    /// targets instantly (direct `SetWindowPos`, bypassing the animator) so the
    /// moving boundary stays fused to the cursor. It is **non-committing**:
    /// neither the registry nor
    /// [`ScrollingSpace`](crate::workspace::ScrollingSpace) is touched — the
    /// committed layout stays frozen for the drag, and the sole commit is
    /// [`on_resize_end`](super::drag::FlowWM::on_resize_end) on release.
    ///
    /// The dragged window is omitted from the target list by the active-drag
    /// exclusion filter (Win32 owns its geometry during the native move-size,
    /// so its width already tracks the cursor, overshoot and all). This mirrors
    /// [`teleport_workspaces`](Self::teleport_workspaces) but operates on a
    /// single active-workspace [`AppliedLayout`] and honors the drag-exclusion
    /// filter — the resize analog of [`animate_preview_on`](Self::animate_preview_on),
    /// except it bypasses the animator (Teleport, not animation) per ADR-0004.
    pub(super) fn teleport_preview(&mut self, preview: &AppliedLayout) {
        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;
        let mut submitted: Vec<(isize, Rect)> = Vec::new();

        // Active-drag exclusion filter — same intent as submit_animation's, so
        // the animator-free teleport also leaves the dragged window alone.
        let excluded_id = self.drag_state.as_ref().map(|ds| ds.dragged_id());
        let excluded_border_hwnd = self.drag_state.as_ref().and_then(|ds| {
            self.registry
                .get_window(HWND(ds.dragged_hwnd() as *mut _))
                .and_then(|w| w.border.as_ref())
                .map(|b| b.hwnd())
        });

        for entry in &preview.actual_layout.entries {
            if Some(entry.window_id) == excluded_id {
                continue;
            }
            let window = self.registry.get_window(HWND(entry.window_id.0 as *mut _));
            let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
            let border_hwnd = window.and_then(|w| w.border.as_ref()).map(|b| b.hwnd());
            if let Some(eb) = excluded_border_hwnd
                && border_hwnd == Some(eb)
            {
                continue;
            }

            let window_rect = invisible_bounds
                .visible_to_window(entry.rect)
                .inset(border_thickness - border_overlap);
            // Direct SetWindowPos — bypass the animator entirely so the boundary
            // follows the cursor with no tween lag. A successful move is the
            // window's new last-submitted rect (the preview never commits to
            // the registry, so this record is all the next batch has).
            if crate::registry::win32::set_window_rect(
                entry.window_id.0,
                window_rect.x,
                window_rect.y,
                window_rect.width,
                window_rect.height,
            ) {
                submitted.push((entry.window_id.0, entry.rect));
            }

            // Teleport the border overlay to match (option<&Window> is Copy).
            if let Some(border) = window.and_then(|w| w.border.as_ref()) {
                border.set_geometry(entry.rect);
            }
        }
        self.record_submitted(&submitted);
    }
}

/// What [`FlowWM::workspace_targets`] builds for one batch.
#[derive(Debug, Default)]
pub(super) struct WorkspaceTargets {
    /// The animator targets (windows and their border overlays).
    pub targets: Vec<WindowTarget>,
    /// Whether any target is a tracked float window (the caller suppresses
    /// float-location tracking for the animation's duration if so).
    pub has_float_target: bool,
    /// The visible rect commanded for each window, for
    /// [`FlowWM::record_submitted`].
    pub submitted: Vec<(isize, Rect)>,
}

/// Convert an [`AppliedLayout`] into animation targets and submit to an animator.
///
/// This is a standalone version of [`FlowWM::animate_layout`] that
/// takes `&mut WindowAnimator` directly instead of `&mut FlowWM`.
/// Used during construction when `FlowWM` doesn't exist yet
/// but the animator needs to snap windows to their initial positions.
///
/// # Visible-Rect to Window-Rect Translation
///
/// Like [`FlowWM::animate_layout`], this function translates each
/// entry's rect from the layout engine's visible-rect space to Win32 window-rect
/// space using the window's stored
/// [`InvisibleBounds`](crate::common::InvisibleBounds). This is necessary even
/// during the initial snap so that windows land at the correct position from
/// the very first frame.
///
/// # Border Thickness
///
/// `border_thickness` and `border_overlap` are passed explicitly because this
/// function runs before `FlowWM` exists — the caller reads them
/// from the config and forwards them. The effective per-edge shrink is
/// `(thickness - overlap)`, same as
/// [`FlowWM::animate_layout`].
///
/// `monitor` is the monitor the layout was projected for; it drives the
/// neighbour-teleport decision ([`plan_move`]) like every other submit path,
/// although at `Duration::ZERO` a tween never sweeps anyway.
pub(super) fn animate_layout_raw(
    animator: &mut WindowAnimator,
    layout: &AppliedLayout,
    monitor: &MonitorInfo,
    registry: &WindowRegistry,
    border_thickness: i32,
    border_overlap: i32,
) {
    if layout.actual_layout.entries.is_empty() {
        return;
    }

    let mut targets: Vec<WindowTarget> = Vec::new();
    // Nothing records these: the daemon's submitted-rect map does not exist
    // yet, and its readers fall back to the registry rect this snap sets.
    let mut submitted: Vec<(isize, Rect)> = Vec::new();
    for entry in &layout.actual_layout.entries {
        let window = registry.get_window(HWND(entry.window_id.0 as *mut _));
        let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
        // During initial construction borders are still None (they're
        // created afterwards in refresh_all_border_styles), so this yields
        // zero border targets on the first snap. Subsequent calls (if any)
        // would include them. See animate_layout.
        let border_hwnd = window.and_then(|w| w.border.as_ref()).map(|b| b.hwnd());

        let window_rect = invisible_bounds
            .visible_to_window(entry.rect)
            .inset(border_thickness - border_overlap);
        // Nothing has been laid out yet, so there is no previous rect.
        push_targets(
            &mut targets,
            &mut submitted,
            monitor,
            entry.window_id.0,
            None,
            entry.rect,
            window_rect,
            border_hwnd,
        );
    }

    if targets.is_empty() {
        return;
    }
    if let Err(e) = animator.animate(targets) {
        log::warn!("animation error (initial snap): {e}");
    }
}

// ---------------------------------------------------------------------------
// Neighbour-teleport rule
// ---------------------------------------------------------------------------

/// The off-virtual-screen zone a parked rect sits in.
///
/// Horizontal zones come from the layout projection
/// ([`park_left_x`](crate::layout::projection::park_left_x) /
/// [`park_right_x`](crate::layout::projection::park_right_x)); vertical ones
/// from workspace parking ([`workspace_y_offset`](crate::workspace::workspace_y_offset)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParkSide {
    /// Entirely left of the virtual screen.
    Left,
    /// Entirely right of the virtual screen.
    Right,
    /// Entirely above the virtual screen (vertical workspace parking).
    Above,
    /// Entirely below the virtual screen.
    Below,
}

/// The parking zones `rect` lies in, one per axis: `(horizontal, vertical)`.
///
/// Both are `None` when the rect touches the virtual screen (an on-screen
/// rect). A parked rect has at least one; a column parked left or right of
/// the virtual screen *inside an inactive workspace* has both, since the
/// workspace offset shifts it above or below as well.
fn park_sides(monitor: &MonitorInfo, rect: Rect) -> (Option<ParkSide>, Option<ParkSide>) {
    if !monitor.is_parked(rect) {
        return (None, None);
    }
    let vs = monitor.virtual_screen;
    let horizontal = if rect.right() <= vs.x {
        Some(ParkSide::Left)
    } else if rect.x >= vs.right() {
        Some(ParkSide::Right)
    } else {
        None
    };
    let vertical = if rect.bottom() <= vs.y {
        Some(ParkSide::Above)
    } else if rect.y >= vs.bottom() {
        Some(ParkSide::Below)
    } else {
        None
    };
    (horizontal, vertical)
}

/// Whether a window sliding between this monitor and the `side` parking
/// zone would pass over another monitor: the monitor's neighbour flag for
/// that side ([`MonitorInfo::neighbour_left`] / `neighbour_right` /
/// `neighbour_up` / `neighbour_down`). This is the one place the rule reads
/// the flags; every submit path goes through it via [`plan_move`].
fn side_blocked(monitor: &MonitorInfo, side: ParkSide) -> bool {
    match side {
        ParkSide::Left => monitor.neighbour_left,
        ParkSide::Right => monitor.neighbour_right,
        ParkSide::Above => monitor.neighbour_up,
        ParkSide::Below => monitor.neighbour_down,
    }
}

/// Whether `rect` is parked on **any** side that has a neighbour. A window
/// parked both horizontally (column) and vertically (workspace) is blocked
/// when either axis is: a tween to or from it would cross that neighbour.
fn parked_on_blocked_side(monitor: &MonitorInfo, rect: Rect) -> bool {
    let (horizontal, vertical) = park_sides(monitor, rect);
    horizontal.is_some_and(|s| side_blocked(monitor, s))
        || vertical.is_some_and(|s| side_blocked(monitor, s))
}

/// How the bridge moves one window from where it is to where the layout
/// puts it. Decided by [`plan_move`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Move {
    /// Ordinary tween through the animator.
    Tween,
    /// Instant move ([`WindowTarget::instant`]): the tween would sweep the
    /// window across a neighbouring monitor.
    Instant,
    /// No target at all: the window already sits in this blocked-side
    /// parking slot and an instant move put it there, so there is nothing
    /// to retarget.
    Stay,
}

/// Decide how to move a window from `prev` (its rect before this batch,
/// `None` when unknown) to `next` on `monitor`.
///
/// The teleport rule: a move is [`Move::Instant`] when **either** endpoint is
/// a parking slot on a side that has a neighbour (`prev` parked there and
/// sliding in over the neighbour, `next` parked there and sliding out over
/// it, or both parked on blocked sides and sliding from one across to the
/// other). Horizontal sides come from column parking, vertical ones from
/// workspace parking, and a rect parked on both axes counts as blocked when
/// **any** of its sides is. Parking on a free side keeps the usual slide,
/// and an on-screen to on-screen move is always a tween.
///
/// When `prev == Some(next)` and that slot is on a blocked side the window
/// is already there — its last move was instant, so no tween can be in
/// flight for it — and the answer is [`Move::Stay`], which saves a
/// `SetWindowPos` per parked window per layout. For that to hold, `prev`
/// must be the rect the window was **last sent to** by any path, committed
/// or not ([`FlowWM::last_submitted_rect`]), never merely the registry's
/// last committed rect: a drag preview can move a parked column on-screen
/// without committing, and the next preview must then move it back instead
/// of staying put. Free-side parked windows never get `Stay`: they slid
/// there, may still be mid-flight, and must be resubmitted so a superseding
/// batch does not strand them.
pub(super) fn plan_move(monitor: &MonitorInfo, prev: Option<Rect>, next: Rect) -> Move {
    let next_blocked = parked_on_blocked_side(monitor, next);
    let prev_blocked = prev.is_some_and(|p| parked_on_blocked_side(monitor, p));
    if next_blocked && prev == Some(next) {
        Move::Stay
    } else if next_blocked || prev_blocked {
        Move::Instant
    } else {
        Move::Tween
    }
}

/// Would tweening a window from `prev` to `next` sweep it across a monitor
/// that neighbours this one? `true` exactly when [`plan_move`] answers
/// [`Move::Instant`]. Kept as the boolean view of the rule for the tests;
/// production code reads the full [`Move`] decision.
#[cfg(test)]
fn tween_crosses_neighbour(monitor: &MonitorInfo, prev: Option<Rect>, next: Rect) -> bool {
    plan_move(monitor, prev, next) == Move::Instant
}

/// Append the animator target(s) for one window — the window itself and,
/// when it has one, its border overlay — to `targets`, choosing tween,
/// instant move or no target per [`plan_move`], and `(hwnd, next)` to
/// `submitted` so the caller can record where the window was sent
/// ([`FlowWM::record_submitted`]; a `Stay` is recorded too — the window is
/// there). Every submit path builds its targets through here so the
/// teleport rule is applied uniformly.
///
/// `next` is the layout's visible rect (any workspace `y_offset` already
/// applied): it drives the decision and seats the border overlay, which sits
/// at the visible rect with no invisible-bounds translation and no thickness
/// inset because the ring occupies the gap between `next` and the inset
/// `window_rect`. `window_rect` is the translated Win32 rect the window
/// itself is moved to. The border follows the window's decision so the
/// animator moves both in lockstep (its `SetWindowPos`-based backend treats
/// the overlay HWND like any other window).
///
/// Returns the decision so callers can log or count it.
#[allow(clippy::too_many_arguments)]
fn push_targets(
    targets: &mut Vec<WindowTarget>,
    submitted: &mut Vec<(isize, Rect)>,
    monitor: &MonitorInfo,
    hwnd: isize,
    prev: Option<Rect>,
    next: Rect,
    window_rect: Rect,
    border_hwnd: Option<isize>,
) -> Move {
    let decision = plan_move(monitor, prev, next);
    submitted.push((hwnd, next));
    log::debug!(
        "animate: hwnd={hwnd} {decision:?} ({},{},{},{}) [visible] → ({},{},{},{}) [window]",
        next.x,
        next.y,
        next.width,
        next.height,
        window_rect.x,
        window_rect.y,
        window_rect.width,
        window_rect.height,
    );
    let instant = match decision {
        Move::Stay => return decision,
        Move::Instant => true,
        Move::Tween => false,
    };
    targets.push(
        WindowTarget::new(
            WindowRef(hwnd),
            IVec2::new(window_rect.x, window_rect.y),
            IVec2::new(window_rect.width, window_rect.height),
        )
        .with_instant(instant),
    );
    if let Some(b_hwnd) = border_hwnd {
        targets.push(
            WindowTarget::new(
                WindowRef(b_hwnd),
                IVec2::new(next.x, next.y),
                IVec2::new(next.width, next.height),
            )
            .with_instant(instant),
        );
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::{
        FlowWM, Move, PrevRects, WindowTarget, WorkspaceParking, plan_move, push_targets,
        tween_crosses_neighbour,
    };
    use crate::common::{Rect, WindowId};
    use crate::layout::types::MonitorInfo;
    use windows::Win32::Foundation::HWND;

    fn primary_with_left_neighbour() -> MonitorInfo {
        let primary = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let left = Rect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let vs = Rect {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        MonitorInfo::in_desktop(primary, primary, vs, &[left])
    }

    fn col(x: i32) -> Rect {
        Rect {
            x,
            y: 16,
            width: 460,
            height: 1000,
        }
    }

    #[test]
    fn parking_toward_a_neighbour_teleports() {
        let m = primary_with_left_neighbour();
        // on-screen -> parked left (DISPLAY2 is there): teleport
        assert!(tween_crosses_neighbour(&m, Some(col(11)), col(-2401)));
        // unknown previous rect -> parked left: still teleport
        assert!(tween_crosses_neighbour(&m, None, col(-2401)));
    }

    #[test]
    fn arriving_from_a_neighbour_side_parking_slot_teleports() {
        let m = primary_with_left_neighbour();
        assert!(tween_crosses_neighbour(&m, Some(col(-2401)), col(11)));
    }

    #[test]
    fn free_edge_parking_keeps_the_slide_animation() {
        let m = primary_with_left_neighbour();
        // parked right (no neighbour there): slide out as usual
        assert!(!tween_crosses_neighbour(&m, Some(col(1439)), col(1936)));
        // ... and slide back in
        assert!(!tween_crosses_neighbour(&m, Some(col(1936)), col(1439)));
    }

    #[test]
    fn on_screen_moves_never_teleport() {
        let m = primary_with_left_neighbour();
        assert!(!tween_crosses_neighbour(&m, Some(col(11)), col(487)));
        assert!(!tween_crosses_neighbour(&m, None, col(487)));
    }

    #[test]
    fn single_monitor_never_teleports() {
        let m = MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        });
        assert!(!tween_crosses_neighbour(&m, Some(col(11)), col(-476)));
        assert!(!tween_crosses_neighbour(&m, Some(col(-476)), col(11)));
    }

    #[test]
    fn parked_left_to_parked_right_teleports_when_the_left_side_is_blocked() {
        // 6 columns, viewport at the end, focus col 0: col 5 goes from the
        // free right slot to the left slot behind DISPLAY2 (and vice versa).
        let m = primary_with_left_neighbour();
        let parked_left = col(-2401);
        let parked_right = col(1936);
        assert!(tween_crosses_neighbour(&m, Some(parked_left), parked_right));
        assert!(tween_crosses_neighbour(&m, Some(parked_right), parked_left));
        assert_eq!(
            plan_move(&m, Some(parked_left), parked_right),
            Move::Instant
        );
    }

    #[test]
    fn already_parked_on_a_blocked_side_stays_put() {
        let m = primary_with_left_neighbour();
        let parked_left = col(-2401);
        assert_eq!(plan_move(&m, Some(parked_left), parked_left), Move::Stay);
        assert!(!tween_crosses_neighbour(&m, Some(parked_left), parked_left));
        let mut targets = Vec::new();
        let mut submitted = Vec::new();
        let decision = push_targets(
            &mut targets,
            &mut submitted,
            &m,
            7,
            Some(parked_left),
            parked_left,
            parked_left.inset(2),
            Some(8),
        );
        assert_eq!(decision, Move::Stay);
        assert!(targets.is_empty(), "no target is emitted for a Stay");
        assert_eq!(submitted, vec![(7, parked_left)]);
    }

    #[test]
    fn already_parked_on_a_free_side_is_still_resubmitted() {
        // The slide to a free-side slot may be mid-flight; dropping the
        // target would strand it when the new batch supersedes the old.
        let m = primary_with_left_neighbour();
        let parked_right = col(1936);
        assert_eq!(plan_move(&m, Some(parked_right), parked_right), Move::Tween);
    }

    #[test]
    fn push_targets_marks_window_and_border_instant_together() {
        let m = primary_with_left_neighbour();
        let mut targets = Vec::new();
        let mut submitted = Vec::new();
        let next = col(-2401);
        let decision = push_targets(
            &mut targets,
            &mut submitted,
            &m,
            7,
            Some(col(11)),
            next,
            next.inset(2),
            Some(8),
        );
        assert_eq!(decision, Move::Instant);
        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(|t| t.instant));
        assert_eq!(targets[0].window_ref.0, 7);
        assert_eq!(targets[0].as_rect().x, next.inset(2).x);
        assert_eq!(targets[1].window_ref.0, 8);
        assert_eq!(targets[1].as_rect().x, next.x);

        let mut targets = Vec::new();
        let decision = push_targets(
            &mut targets,
            &mut submitted,
            &m,
            7,
            Some(col(11)),
            col(487),
            col(487),
            None,
        );
        assert_eq!(decision, Move::Tween);
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].instant);
    }

    #[test]
    fn vertical_workspace_parking_on_a_free_side_keeps_the_slide() {
        // A workspace parked above/below the virtual screen is off-screen,
        // but with no monitor stacked there it slides like before.
        let m = primary_with_left_neighbour();
        assert!(!m.neighbour_up && !m.neighbour_down);
        let above = Rect {
            x: 11,
            y: -1080 + 16,
            width: 460,
            height: 1000,
        };
        assert_eq!(plan_move(&m, Some(col(11)), above), Move::Tween);
        assert_eq!(plan_move(&m, Some(above), col(11)), Move::Tween);
        // ... but a column parked left of the virtual screen AND shifted
        // vertically is still behind the left neighbour: instant.
        let parked_left_shifted = Rect {
            y: -1080 + 16,
            ..col(-2401)
        };
        assert_eq!(
            plan_move(&m, Some(col(-2401)), parked_left_shifted),
            Move::Instant
        );
    }

    /// Laptop panel at the origin with a 2560x1440 display stacked above it.
    /// Up is blocked, down is free.
    fn laptop_under_external() -> MonitorInfo {
        let laptop = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
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
        MonitorInfo::in_desktop(laptop, laptop, vs, &[external])
    }

    /// The far "above" slot on `laptop_under_external`: a 1000px-tall window
    /// at y=16 on the laptop, shifted by the blocked-side workspace offset
    /// (-1440 - 16 - 1080).
    fn far_above(x: i32) -> Rect {
        Rect {
            y: 16 - 1440 - 16 - 1080,
            ..col(x)
        }
    }

    #[test]
    fn workspace_parking_toward_a_stacked_neighbour_teleports() {
        let m = laptop_under_external();
        assert!(m.neighbour_up && !m.neighbour_down);
        // on-screen -> far above (behind the external): instant
        assert_eq!(plan_move(&m, Some(col(11)), far_above(11)), Move::Instant);
        // unknown previous rect: still instant
        assert_eq!(plan_move(&m, None, far_above(11)), Move::Instant);
        // arriving from behind the external: instant
        assert_eq!(plan_move(&m, Some(far_above(11)), col(11)), Move::Instant);
        // already there: Stay, no target emitted
        assert_eq!(
            plan_move(&m, Some(far_above(11)), far_above(11)),
            Move::Stay
        );
    }

    #[test]
    fn returning_from_the_far_slot_behind_a_stacked_neighbour_is_instant() {
        // VM regression: DISPLAY2 above DISPLAY1, switch 2 -> 1 on DISPLAY1.
        // The incoming window sits exactly at the far "above" slot (its
        // bottom PARK_MARGIN above the virtual screen) and must not tween
        // down through DISPLAY2 on its way back on-screen.
        let m = laptop_under_external();
        let height = 1000;
        let parked = Rect {
            x: 11,
            y: m.virtual_screen.y - crate::layout::projection::PARK_MARGIN - height,
            width: 460,
            height,
        };
        let on_screen = col(11);
        assert_eq!(plan_move(&m, Some(parked), on_screen), Move::Instant);
        let mut targets = Vec::new();
        let mut submitted = Vec::new();
        let decision = push_targets(
            &mut targets,
            &mut submitted,
            &m,
            7,
            Some(parked),
            on_screen,
            on_screen.inset(2),
            Some(8),
        );
        assert_eq!(decision, Move::Instant);
        assert!(targets.iter().all(|t| t.instant));
    }

    #[test]
    fn workspace_parking_toward_the_free_side_still_slides() {
        let m = laptop_under_external();
        // The free-side slot: one monitor height (+gap) below the laptop,
        // which is also beyond the virtual screen's bottom (2520).
        let below = Rect {
            y: 16 + 1080 + 4,
            ..col(11)
        };
        assert_eq!(plan_move(&m, Some(col(11)), below), Move::Tween);
        assert_eq!(plan_move(&m, Some(below), col(11)), Move::Tween);
        // A free-side parked window is always resubmitted.
        assert_eq!(plan_move(&m, Some(below), below), Move::Tween);
    }

    #[test]
    fn window_parked_on_both_axes_is_instant_when_either_side_is_blocked() {
        let m = laptop_under_external();
        // Column parked right of the virtual screen (free side) inside a
        // workspace parked above (blocked side): the vertical axis blocks.
        let right_and_above = far_above(2560 + 16);
        assert_eq!(
            plan_move(&m, Some(col(2560 + 16)), right_and_above),
            Move::Instant
        );
        assert_eq!(
            plan_move(&m, Some(right_and_above), col(2560 + 16)),
            Move::Instant
        );
        // Free on both axes (parked right, workspace below): slide.
        let right_and_below = Rect {
            y: 16 + 1080 + 4,
            ..col(2560 + 16)
        };
        assert_eq!(
            plan_move(&m, Some(col(2560 + 16)), right_and_below),
            Move::Tween
        );
        // The mirror case: a left-blocked column inside a workspace parked
        // on a free vertical side is still instant (horizontal axis blocks).
        let m = primary_with_left_neighbour();
        let left_and_below = Rect {
            y: 16 + 1080 + 4,
            ..col(-2401)
        };
        assert_eq!(plan_move(&m, Some(col(11)), left_and_below), Move::Instant);
    }

    /// Fake HWND values for the stacked-pair daemon fixture.
    const WS1_HWND: isize = 0x20001;
    const WS2_HWND: isize = 0x20002;

    /// A daemon on the laptop of `laptop_under_external` with two
    /// workspaces of one tile each and workspace 2 active, so workspace 1 is
    /// parked above — behind the external. The registry holds each tile's
    /// workspace-local rect, as it does in the daemon.
    fn stacked_flow_with_ws1_parked_above() -> FlowWM {
        use crate::config::types::WindowRulesConfig;
        use crate::layout::types::Padding;
        use crate::registry::WindowRegistry;
        use crate::registry::win32::WindowInfo;
        use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};
        use windows::Win32::Foundation::HWND;

        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [WS1_HWND, WS2_HWND] {
            registry.register_window_from_info(&WindowInfo {
                hwnd: HWND(hwnd as *mut _),
                title: "TestWindow".into(),
                class: "TestClass".into(),
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                },
                exe: "test.exe".into(),
                process_path: "C:\\test.exe".into(),
                is_visible: true,
                is_maximized: false,
                is_fullscreen: false,
            });
        }
        let info = laptop_under_external();
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let mut workspaces = Vec::new();
        for (id, hwnd) in [(1, WS1_HWND), (2, WS2_HWND)] {
            let mut scrolling = ScrollingSpace::new(info, 960, 320, 100, 100, padding, 4);
            scrolling.add_window(WindowId(hwnd));
            registry.update_tiling_slots_from_layout(scrolling.virtual_layout());
            registry.update_tiled_rects(scrolling.actual_layout());
            workspaces.push(Workspace::new(WorkspaceId(id), scrolling));
        }
        let laptop = info.work_area;
        let monitor = Monitor::new(laptop, laptop, workspaces, 1);
        FlowWM::new_for_test(registry, vec![monitor])
    }

    #[test]
    fn stacked_snapshot_reports_parked_workspace_windows_at_their_far_slot() {
        use crate::workspace::WorkspaceId;
        let flow = stacked_flow_with_ws1_parked_above();
        let monitor = &flow.monitors[0];
        let parking = WorkspaceParking::new(
            monitor.active_scrolling().monitor(),
            monitor.screen_rect(),
            4,
        );
        let far_above = parking.offset(WorkspaceId(1), WorkspaceId(2));
        assert!(far_above < -1440, "ws 1 parks beyond the external");
        let local1 = flow
            .registry
            .get_window(HWND(WS1_HWND as *mut _))
            .and_then(|w| w.tiled_rect);
        let local2 = flow
            .registry
            .get_window(HWND(WS2_HWND as *mut _))
            .and_then(|w| w.tiled_rect);
        let (Some(local1), Some(local2)) = (local1, local2) else {
            panic!("both tiles have registry rects");
        };
        // The plain registry snapshot is workspace-local: both tiles on-screen.
        let plain = flow.snapshot_prev_rects();
        assert_eq!(plain.get(&WS1_HWND), Some(&local1));
        // The stacked snapshot shifts the parked workspace's tile up to where
        // it really is, and leaves the active workspace's tile alone.
        let stacked = flow.snapshot_stacked_prev_rects();
        assert_eq!(
            stacked.get(&WS1_HWND),
            Some(&Rect {
                y: local1.y + far_above,
                ..local1
            })
        );
        assert_eq!(stacked.get(&WS2_HWND), Some(&local2));
    }

    #[test]
    fn switching_back_from_behind_the_external_targets_the_incoming_window_instantly() {
        use crate::workspace::WorkspaceId;
        let flow = stacked_flow_with_ws1_parked_above();
        let monitor = &flow.monitors[0];
        let parking = WorkspaceParking::new(
            monitor.active_scrolling().monitor(),
            monitor.screen_rect(),
            4,
        );
        let (Some(ws1), Some(ws2)) = (
            monitor.workspace(WorkspaceId(1)),
            monitor.workspace(WorkspaceId(2)),
        ) else {
            panic!("fixture has workspaces 1 and 2");
        };
        // The batch a 2 -> 1 switch builds: ws 1 comes on-screen (offset 0),
        // ws 2 parks below on the free side (+height+gap).
        let batches = [
            (ws1.scrolling.actual_layout().clone(), 0),
            (
                ws2.scrolling.actual_layout().clone(),
                parking.offset(WorkspaceId(2), WorkspaceId(1)),
            ),
        ];
        let find = |targets: &[WindowTarget], hwnd: isize| {
            targets
                .iter()
                .find(|t| t.window_ref.0 == hwnd)
                .map(|t| t.instant)
        };

        // With the stacked snapshot (what the switch uses) the incoming
        // window is instant and the outgoing one still slides.
        let snap = flow.snapshot_stacked_prev_rects();
        let targets = flow
            .workspace_targets(0, &batches, PrevRects::Snapshot(&snap))
            .targets;
        assert_eq!(
            find(&targets, WS1_HWND),
            Some(true),
            "incoming window must not tween down through the external"
        );
        assert_eq!(
            find(&targets, WS2_HWND),
            Some(false),
            "outgoing window slides to the free side below"
        );

        // The workspace-local registry rects would have hidden the parked
        // position (on-screen -> on-screen reads as a tween): this is the
        // VM failure and why the switch must not use `PrevRects::Registry`.
        let targets = flow
            .workspace_targets(0, &batches, PrevRects::Registry)
            .targets;
        assert_eq!(find(&targets, WS1_HWND), Some(false));
    }

    #[test]
    fn switch_workspace_layout_on_a_stacked_pair_flips_the_active_workspace() {
        // End to end through the dispatch path with the mock animator: the
        // switch must build its batches, teleport nothing (both workspaces
        // are participants) and leave ws 1 active.
        use crate::workspace::WorkspaceId;
        let mut flow = stacked_flow_with_ws1_parked_above();
        assert!(flow.switch_workspace_layout(WorkspaceId(1), WorkspaceId(2)));
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(1));
    }

    /// A daemon on `primary_with_left_neighbour` whose strip holds three
    /// 960-wide columns with the viewport at the end, so column 0 (`P`) is
    /// parked behind the left neighbour and the registry records it there.
    fn flow_with_a_column_parked_behind_the_left_neighbour() -> (FlowWM, isize) {
        use crate::config::types::WindowRulesConfig;
        use crate::layout::types::Padding;
        use crate::registry::WindowRegistry;
        use crate::registry::win32::WindowInfo;
        use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};
        use windows::Win32::Foundation::HWND;

        const P: isize = 0x30001;
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [P, 0x30002, 0x30003] {
            registry.register_window_from_info(&WindowInfo {
                hwnd: HWND(hwnd as *mut _),
                title: "TestWindow".into(),
                class: "TestClass".into(),
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                },
                exe: "test.exe".into(),
                process_path: "C:\test.exe".into(),
                is_visible: true,
                is_maximized: false,
                is_fullscreen: false,
            });
        }
        let info = primary_with_left_neighbour();
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let mut scrolling = ScrollingSpace::new(info, 960, 320, 100, 100, padding, 4);
        for hwnd in [P, 0x30002, 0x30003] {
            // insert_window reveals each new column, scrolling the viewport.
            scrolling.insert_window(WindowId(hwnd));
        }
        registry.update_tiling_slots_from_layout(scrolling.virtual_layout());
        registry.update_tiled_rects(scrolling.actual_layout());
        let parked = scrolling
            .actual_layout()
            .find(WindowId(P))
            .expect("P projected")
            .rect;
        assert!(info.is_parked(parked), "P must start parked: {parked:?}");
        assert!(parked.right() <= info.virtual_screen.x, "parked left");
        let screen = info.work_area;
        let monitor = Monitor::new(
            screen,
            screen,
            vec![Workspace::new(WorkspaceId(1), scrolling)],
            0,
        );
        (FlowWM::new_for_test(registry, vec![monitor]), P)
    }

    /// The decision `collect_targets` would make for `hwnd` under `layout`,
    /// and whether a target was emitted for it.
    fn decision_for(
        flow: &FlowWM,
        layout: &crate::layout::types::AppliedLayout,
        hwnd: isize,
    ) -> (Option<bool>, Rect) {
        let mut targets = Vec::new();
        let mut submitted = Vec::new();
        flow.collect_targets(layout, 0, &mut targets, &mut submitted);
        let instant = targets
            .iter()
            .find(|t| t.window_ref.0 == hwnd)
            .map(|t| t.instant);
        let next = submitted
            .iter()
            .find(|(h, _)| *h == hwnd)
            .map(|(_, r)| *r)
            .expect("every window in the layout is recorded");
        (instant, next)
    }

    #[test]
    fn preview_sequence_moves_a_parked_column_instantly_each_time_and_never_stays_stale() {
        // Drag previews never commit to the registry. A parked column that
        // one preview brings on-screen must be sent back instantly by the
        // next preview (not told to Stay because the registry still says
        // "parked"), and a third preview that leaves it parked is a Stay.
        let (mut flow, p) = flow_with_a_column_parked_behind_the_left_neighbour();
        let committed = {
            let space = flow.monitors[0].active_scrolling();
            crate::layout::types::AppliedLayout {
                virtual_layout: space.virtual_layout().clone(),
                actual_layout: space.actual_layout().clone(),
            }
        };
        let parked_rect = committed
            .actual_layout
            .find(WindowId(p))
            .expect("P in the layout")
            .rect;
        // A preview with the viewport back at the start: P comes on-screen.
        let on_screen_preview = {
            let mut vl = committed.virtual_layout.clone();
            vl.viewport_offset = 0;
            let monitor = *flow.monitors[0].active_scrolling().monitor();
            let padding = flow.monitors[0].active_scrolling().padding();
            let actual = crate::layout::projection::project(&vl, &monitor, &padding, 0);
            crate::layout::types::AppliedLayout {
                virtual_layout: vl,
                actual_layout: actual,
            }
        };
        let on_screen_rect = on_screen_preview
            .actual_layout
            .find(WindowId(p))
            .expect("P in the preview")
            .rect;
        assert!(on_screen_rect.overlaps(flow.monitors[0].work_area()));

        // 1. Parked (registry) -> on-screen preview: instant, from behind
        //    the neighbour.
        assert_eq!(flow.last_submitted_rect(p), None);
        let (instant, next) = decision_for(&flow, &on_screen_preview, p);
        assert_eq!(instant, Some(true));
        assert_eq!(next, on_screen_rect);
        flow.animate_preview_on(0, &on_screen_preview);
        assert_eq!(flow.last_submitted_rect(p), Some(on_screen_rect));
        // The registry still holds the committed (parked) rect: the preview
        // never commits.
        assert_eq!(
            flow.registry
                .get_window(HWND(p as *mut _))
                .and_then(|w| w.tiled_rect),
            Some(parked_rect)
        );

        // 2. On-screen (last submitted) -> parked preview: instant again,
        //    not a Stay — the window is not where the registry says.
        let (instant, next) = decision_for(&flow, &committed, p);
        assert_eq!(
            instant,
            Some(true),
            "must be sent back behind the neighbour"
        );
        assert_eq!(next, parked_rect);
        flow.animate_preview_on(0, &committed);
        assert_eq!(flow.last_submitted_rect(p), Some(parked_rect));

        // 3. Parked (last submitted) -> parked preview: already there, Stay.
        let (instant, _) = decision_for(&flow, &committed, p);
        assert_eq!(instant, None, "a Stay emits no target");

        // Leaving the layout forgets the record; the registry rect is the
        // fallback again.
        flow.forget_submitted(p);
        assert_eq!(flow.last_submitted_rect(p), None);
        let (instant, _) = decision_for(&flow, &committed, p);
        assert_eq!(instant, None, "registry says parked: Stay");
    }

    #[test]
    fn stacked_pair_with_free_sides_never_teleports_horizontally() {
        // Regression: a vertical neighbour must not make horizontal column
        // parking instant — the laptop's left and right sides are free.
        let m = laptop_under_external();
        assert!(!m.neighbour_left && !m.neighbour_right);
        assert_eq!(plan_move(&m, Some(col(11)), col(-476)), Move::Tween);
        assert_eq!(plan_move(&m, Some(col(11)), col(2560 + 16)), Move::Tween);
    }
}
