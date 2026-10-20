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

use super::types::FlowWM;

/// Where the bridge reads each window's **pre-batch** rect from when it
/// decides whether a tween would cross a neighbouring monitor
/// ([`plan_move`]).
///
/// The decision compares where the window is now with where the layout puts
/// it, so the "now" rect must predate the mutation being animated. Callers
/// that sync the registry *after* submitting use [`Registry`](Self::Registry);
/// callers that have already synced it (the loadout apply path) capture the
/// rects first with [`FlowWM::snapshot_prev_rects`] and pass
/// [`Snapshot`](Self::Snapshot).
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
        for (idx, layout) in layouts {
            self.collect_targets(layout, *idx, &mut targets);
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
        self.collect_targets(layout, monitor_idx, &mut targets);
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

    /// Append the animator targets for `layout` (produced by `monitor_idx`'s
    /// scrolling space) to `targets`, teleporting the windows whose tween
    /// would cross a neighbouring monitor.
    fn collect_targets(
        &self,
        layout: &AppliedLayout,
        monitor_idx: usize,
        targets: &mut Vec<WindowTarget>,
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

            // The registry's tiled rect is still the pre-mutation one here:
            // animate_layouts_on syncs the registry after submitting.
            let prev_rect = registry_prev_rect(window);
            push_targets(
                targets,
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
    /// (`docs/src/dev-guide/tile-drag.md`)
    pub(super) fn animate_preview(&mut self, preview: &AppliedLayout) {
        let idx = self.active_monitor;
        self.submit_animation(preview, idx);
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

        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;
        let monitor_info = *self.monitors[monitor_idx].active_scrolling().monitor();

        // Build the combined target list. Iterating with explicit loops
        // (rather than `flat_map`) lets us hold one `&self.registry` borrow
        // at a time without entangling the borrow checker across closures.
        let mut targets: Vec<WindowTarget> = Vec::new();
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
                let window_rect = shift(
                    invisible_bounds
                        .visible_to_window(entry.rect)
                        .inset(border_thickness - border_overlap),
                );
                // The border overlay sits at the visible rect (same y_offset)
                // so it moves in lockstep with the window across the switch.
                let next = shift(entry.rect);
                let prev_rect = prev.rect_for(entry.window_id.0, window);
                push_targets(
                    &mut targets,
                    &monitor_info,
                    entry.window_id.0,
                    prev_rect,
                    next,
                    window_rect,
                    border_hwnd,
                );
            }
        }

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
    /// visual inconsistency on a non-visible workspace, not a crash.
    pub(super) fn teleport_workspaces(&self, batches: &[(ActualLayout, i32)]) {
        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;
        for (layout, y_offset) in batches {
            for entry in &layout.entries {
                let window = self.registry.get_window(HWND(entry.window_id.0 as *mut _));
                let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();

                let window_rect = invisible_bounds
                    .visible_to_window(entry.rect)
                    .inset(border_thickness - border_overlap);
                let target_y = window_rect.y + y_offset;

                // Direct SetWindowPos — bypass the animator entirely.
                // Per-window result ignored: teleport is best-effort, and a
                // single failure must not abort the rest of the batch.
                let _ = crate::registry::win32::set_window_rect(
                    entry.window_id.0,
                    window_rect.x,
                    target_y,
                    window_rect.width,
                    window_rect.height,
                );

                // Teleport the border overlay to match. `set_geometry` calls
                // `SetWindowPos`, which triggers `WM_SIZE` → repaint if the size
                // changed, keeping the bitmap in sync on instant repositioning.
                // Option<&Window> is Copy so re-using `window` here is sound.
                if let Some(border) = window.and_then(|w| w.border.as_ref()) {
                    border.set_geometry(Rect {
                        x: entry.rect.x,
                        y: entry.rect.y + y_offset,
                        width: entry.rect.width,
                        height: entry.rect.height,
                    });
                }
            }
        }
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
    /// filter — the resize analog of [`animate_preview`](Self::animate_preview),
    /// except it bypasses the animator (Teleport, not animation) per ADR-0004.
    pub(super) fn teleport_preview(&self, preview: &AppliedLayout) {
        let border_thickness = self.config.borders.thickness as i32;
        let border_overlap = self.config.borders.overlap as i32;

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
            // follows the cursor with no tween lag.
            let _ = crate::registry::win32::set_window_rect(
                entry.window_id.0,
                window_rect.x,
                window_rect.y,
                window_rect.width,
                window_rect.height,
            );

            // Teleport the border overlay to match (option<&Window> is Copy).
            if let Some(border) = window.and_then(|w| w.border.as_ref()) {
                border.set_geometry(entry.rect);
            }
        }
    }
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

/// Which parking zone `rect` lies in, or `None` when it touches the virtual
/// screen (an on-screen rect).
fn park_side(monitor: &MonitorInfo, rect: Rect) -> Option<ParkSide> {
    if !monitor.is_parked(rect) {
        return None;
    }
    let vs = monitor.virtual_screen;
    Some(if rect.right() <= vs.x {
        ParkSide::Left
    } else if rect.x >= vs.right() {
        ParkSide::Right
    } else if rect.bottom() <= vs.y {
        ParkSide::Above
    } else {
        ParkSide::Below
    })
}

/// Whether a window sliding between this monitor and the `side` parking
/// zone would pass over another monitor.
///
/// Only horizontal neighbours are modelled today
/// ([`MonitorInfo::neighbour_left`] / [`MonitorInfo::neighbour_right`]).
/// Vertical workspace parking still uses the monitor's own height and does
/// not know about stacked monitors; once `MonitorInfo` grows
/// `neighbour_up` / `neighbour_down`, wire them in here and every submit
/// path picks the rule up.
fn side_blocked(monitor: &MonitorInfo, side: ParkSide) -> bool {
    match side {
        ParkSide::Left => monitor.neighbour_left,
        ParkSide::Right => monitor.neighbour_right,
        ParkSide::Above | ParkSide::Below => false,
    }
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
/// other). Parking on a free side keeps the usual slide, and an on-screen
/// to on-screen move is always a tween.
///
/// When `prev == Some(next)` and that slot is on a blocked side the window
/// is already there — its last move was instant, so no tween can be in
/// flight for it — and the answer is [`Move::Stay`], which saves a
/// `SetWindowPos` per parked window per layout. Free-side parked windows
/// never get `Stay`: they slid there, may still be mid-flight, and must be
/// resubmitted so a superseding batch does not strand them.
pub(super) fn plan_move(monitor: &MonitorInfo, prev: Option<Rect>, next: Rect) -> Move {
    let next_blocked = park_side(monitor, next).is_some_and(|s| side_blocked(monitor, s));
    let prev_blocked = prev
        .and_then(|p| park_side(monitor, p))
        .is_some_and(|s| side_blocked(monitor, s));
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
/// instant move or no target per [`plan_move`]. Every submit path builds its
/// targets through here so the teleport rule is applied uniformly.
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
fn push_targets(
    targets: &mut Vec<WindowTarget>,
    monitor: &MonitorInfo,
    hwnd: isize,
    prev: Option<Rect>,
    next: Rect,
    window_rect: Rect,
    border_hwnd: Option<isize>,
) -> Move {
    let decision = plan_move(monitor, prev, next);
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
    use super::{Move, plan_move, push_targets, tween_crosses_neighbour};
    use crate::common::Rect;
    use crate::layout::types::MonitorInfo;

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
        let decision = push_targets(
            &mut targets,
            &m,
            7,
            Some(parked_left),
            parked_left,
            parked_left.inset(2),
            Some(8),
        );
        assert_eq!(decision, Move::Stay);
        assert!(targets.is_empty(), "no target is emitted for a Stay");
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
        let next = col(-2401);
        let decision = push_targets(
            &mut targets,
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
        let decision = push_targets(&mut targets, &m, 7, Some(col(11)), col(487), col(487), None);
        assert_eq!(decision, Move::Tween);
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].instant);
    }

    #[test]
    fn vertical_workspace_parking_is_not_a_blocked_side() {
        // A workspace parked above/below the virtual screen is off-screen but
        // no vertical neighbour is modelled, so it keeps the slide.
        let m = primary_with_left_neighbour();
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
}
