//! Window drag lifecycle: a 5-state machine
//! `DragMode ::= Idle | Classifying | Translate | Resize | Float`.
//!
//! When a tiled window is grabbed (`MoveSizeStart`), the drag enters the
//! provisional [`DragMode::Classifying`] state. Classification happens on the
//! first `LOCATIONCHANGE` by rect-diff ([`classify_drag`]): width/height
//! changed → [`DragMode::Resize`] (and which horizontal edge moved identifies
//! the grip); position-only → [`DragMode::Translate`] (the existing title-bar
//! reorder); a click with no movement stays `Classifying` and is a no-op on
//! release. `Classifying` also suppresses geometry snap-back from the moment
//! it is entered, because the dragged tile's `LOCATIONCHANGE` is routed here
//! (not to the float-sync path) for the whole drag.
//!
//! `Translate` keeps the existing reorder behavior unchanged: a non-committing
//! preview during the drag ([`FlowWM::on_translate_move`]) and the sole
//! window-placement commit on release ([`FlowWM::on_translate_end`]).
//!
//! `Resize` (tickets #9 + #10: horizontal column, vertical row, and corner-
//! compose edge/corner resize) is a sibling state. During the drag it
//! teleports the *other* windows to their boundary-move targets (direct
//! `SetWindowPos`, bypassing the animator) so the moving boundary stays fused
//! to the cursor; the committed `ScrollingSpace` layout stays **frozen**
//! (non-committing), and the sole commit is on release, which runs the
//! animator once. The viewport never scrolls mid-grab, and the resized column
//! is brought into view on release via [`ensure_column_visible`]. A corner
//! grip composes the horizontal and vertical boundary-moves in one gesture
//! (they touch disjoint fields, so they commute).
//!
//! A floating window enters [`DragMode::Float`] on its `MoveSizeStart` and
//! holds `drag_state` until `MoveSizeEnd`. There is nothing to classify or
//! commit: its `LOCATIONCHANGE` events are routed here (by `dragged_hwnd`,
//! like a tile's) and each one stores the live rect through the same
//! `store_float_rect` the passive float-sync path uses. Occupying the one
//! `drag_state` field is the point — the animator, the busy gate and the
//! keymap sessions all ask that field, so a float drag needs no flag of its
//! own (ADR-0016; the same shape as upstream's "floats enter DragMode").
//!
//! Because resize and float drags live in the same `drag_state` field, the
//! existing IPC `Busy` guard (layout-mutating commands rejected while
//! `drag_state` is `Some`) covers them automatically.
//!
//! The three entry points ([`FlowWM::on_drag_start`],
//! [`FlowWM::on_drag_move`], [`FlowWM::on_drag_end`]) are called from the
//! daemon's event loop; the hook callback remains stateless — it only
//! signals via [`set_dragged_hwnd`](crate::registry::hooks::set_dragged_hwnd)
//! / [`clear_dragged_hwnd`](crate::registry::hooks::clear_dragged_hwnd)
//! from the main thread. A keymap-driven session (`flow dispatch drag
//! begin|end`, `daemon::drag_session`) enters the same state machine.
//!
//! The Win32 reads the handlers need (window rect, cursor) go through
//! [`DragProbe`] so the `_with` variants run against a scripted desktop in
//! the unit tests.
//!
//! (`docs/src/dev-guide/tile-drag.md`, `docs/adr/0004-tile-resize-contract.md`,
//! `docs/adr/0016-layer-focus-switch-and-one-drag-state.md`)

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;

use crate::borders::{BorderState, style_for_state};
use crate::common::{Direction, Rect, WindowId};
use crate::layout::mutations::ensure_column_visible;
use crate::layout::preview::{
    DropZone, ZoneParams, preview_insert, preview_move, resolve_drop_zone,
};
use crate::layout::projection;
use crate::layout::resize::{
    DragKind, ResizeEdge, VerticalEdge, classify_drag, resize_column_boundary_move,
    resize_row_boundary_move,
};
use crate::layout::types::AppliedLayout;
use crate::registry::hooks::{clear_dragged_hwnd, set_dragged_hwnd};
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32 as registry_win32;

use super::borders::float_border_rect;
use super::edge_scroll::{EdgeScrollAction, EdgeScrollScheduler, EdgeScrollTimings};
use super::types::FlowWM;

/// The Win32 reads the drag and float-sync handlers make, behind a trait so
/// the unit tests can script a desktop (`docs/src/dev-guide/tile-drag.md`,
/// *Testing*).
pub(super) trait DragProbe {
    /// `GetWindowRect`: the full window rect, or `None` when it cannot be read.
    fn window_rect(&self, hwnd: isize) -> Option<Rect>;
    /// `GetCursorPos` in screen pixels.
    fn cursor_pos(&self) -> Option<(i32, i32)>;
    /// `IsIconic`.
    fn is_iconic(&self, hwnd: isize) -> bool;
}

/// The live desktop.
pub(super) struct Win32Drag;

impl DragProbe for Win32Drag {
    fn window_rect(&self, hwnd: isize) -> Option<Rect> {
        registry_win32::get_window_rect(HWND(hwnd as *mut _))
            .map_err(|e| log::debug!("drag: GetWindowRect failed for {hwnd}: {e}"))
            .ok()
    }
    fn cursor_pos(&self) -> Option<(i32, i32)> {
        registry_win32::get_cursor_pos()
            .map_err(|e| log::debug!("drag: GetCursorPos failed: {e}"))
            .ok()
    }
    fn is_iconic(&self, hwnd: isize) -> bool {
        registry_win32::is_iconic(HWND(hwnd as *mut _))
    }
}

/// The 5-state drag state machine, or `None` when idle.
///
/// `Idle` is represented as `None` on [`FlowWM::drag_state`], so the existing
/// `is_some()` / `take()` API (and the IPC `Busy` guard that keys off it) is
/// unchanged — resize and float drags live in this same field and are
/// covered automatically.
pub(super) enum DragMode {
    /// Provisional state entered on `MoveSizeStart` for a tile. Suppresses
    /// geometry snap-back and classifies on the first `LOCATIONCHANGE` by
    /// rect-diff into [`Translate`](DragMode::Translate) or
    /// [`Resize`](DragMode::Resize).
    Classifying(ClassifyingDrag),
    /// Title-bar reorder drag — the existing translate behavior. Unchanged by
    /// ticket #9 (it was previously the only drag state).
    Translate(DragState),
    /// Horizontal edge/corner column resize — ticket #9.
    Resize(ResizeDrag),
    /// A floating window being moved or resized, natively or by a keymap
    /// session. Entered directly (no classification), held for the whole
    /// gesture, and ended with no layout commit: every `LOCATIONCHANGE`
    /// stored the live rect already (ADR-0016).
    Float(FloatDrag),
}

impl DragMode {
    /// The layout-engine ID of the dragged window (valid for every variant).
    #[must_use]
    pub(super) fn dragged_id(&self) -> WindowId {
        match self {
            DragMode::Classifying(c) => c.dragged_id,
            DragMode::Translate(t) => t.dragged_id,
            DragMode::Resize(r) => r.dragged_id,
            DragMode::Float(f) => f.dragged_id,
        }
    }

    /// The raw HWND value of the dragged window (valid for every variant).
    #[must_use]
    pub(super) fn dragged_hwnd(&self) -> isize {
        match self {
            DragMode::Classifying(c) => c.dragged_hwnd,
            DragMode::Translate(t) => t.dragged_hwnd,
            DragMode::Resize(r) => r.dragged_hwnd,
            DragMode::Float(f) => f.dragged_hwnd,
        }
    }

    /// The armed edge-scroll repeat deadline, or `None`. Only
    /// [`Translate`](DragMode::Translate) arms edge-scroll; the other variants
    /// never scroll mid-grab.
    #[must_use]
    pub(super) fn edge_scroll_deadline(&self) -> Option<Instant> {
        match self {
            DragMode::Translate(t) => t.edge_scroll_deadline,
            DragMode::Classifying(_) | DragMode::Resize(_) | DragMode::Float(_) => None,
        }
    }
}

/// Identity of the float being dragged (the `Float` variant). No start
/// rect (nothing is classified) and no grip (nothing is committed): the
/// id and HWND are all the routing and the animator's exclusion need.
pub(super) struct FloatDrag {
    /// The layout-engine ID of the dragged float.
    pub(super) dragged_id: WindowId,
    /// The raw HWND value (for the `DRAGGED_HWND` global and event routing).
    pub(super) dragged_hwnd: isize,
}

/// Provisional `Classifying` payload: identity + the start rect.
pub(super) struct ClassifyingDrag {
    /// The layout-engine ID of the dragged window.
    pub(super) dragged_id: WindowId,
    /// The raw HWND value (for `GetWindowRect`, the `DRAGGED_HWND` global).
    pub(super) dragged_hwnd: isize,
    /// The dragged window's screen rect captured at `MoveSizeStart`, the
    /// reference for the rect-diff classifier on the first `LOCATIONCHANGE`.
    pub(super) start_rect: Rect,
}

/// State held while the user is reordering a tiled window by its title bar
/// (the `Translate` variant). Unchanged by ticket #9.
///
/// Only placement-essential fields are kept: the dragged window's identity, its
/// HWND (for `GetWindowRect` and the `DRAGGED_HWND` global), and the drop zone
/// currently under the cursor (`None` until the first move, and for the
/// empty-workspace degenerate where [`resolve_drop_zone`] returns `None`).
/// There is no dwell timer. The preview is gated on zone change (see
/// [`should_submit_preview`]); the commit happens once, on release.
pub(super) struct DragState {
    /// The layout-engine ID of the dragged window.
    pub(super) dragged_id: WindowId,
    /// The raw HWND value (for `GetWindowRect`, the `DRAGGED_HWND` global).
    pub(super) dragged_hwnd: isize,
    /// Drop zone currently under the cursor (`None` until the first move, and
    /// while the cursor is outside every work area, where
    /// [`resolve_drop_zone`] returns `None`). Taken from the last move event,
    /// never from the cursor at release: a keymap session's final move can
    /// still be queued when `drag end` arrives.
    pub(super) current_zone: Option<DropZone>,
    /// The monitor whose strip `current_zone` was resolved against: the one
    /// under the cursor at the last move. Another monitor than the dragged
    /// window's own makes the drop a cross-screen move (ADR-0018). `None`
    /// exactly when `current_zone` is.
    pub(super) zone_monitor: Option<usize>,
    /// Monitors other than the source whose strip got a preview or a scroll
    /// during this drag. Every later submission re-settles them in the same
    /// animator batch (a new batch supersedes an in-flight one).
    pub(super) touched: Vec<usize>,
    /// The edge-scroll auto-repeat state machine. Drives the
    /// immediate-on-entry scroll plus the timer-based repeat cadence; see
    /// [`edge_scroll`] and [`EdgeScrollScheduler`].
    pub(super) edge_scroll: EdgeScrollScheduler,
    /// The already-clamped effective timings, computed once at drag start from
    /// [`DragConfig`](crate::config::DragConfig) so the scheduler consumes them
    /// with no per-event clamp math.
    pub(super) edge_scroll_timings: EdgeScrollTimings,
    /// The deadline at which the armed auto-repeat timer fires, or `None` when
    /// no repeat is armed. The main loop folds this into its wait-timeout
    /// `min`-reduce and runs [`FlowWM::maybe_fire_edge_scroll`] when it arrives.
    /// Set/cleared by the [`EdgeScrollAction::Arm`] / [`EdgeScrollAction::Cancel`]
    /// actions the scheduler emits.
    pub(super) edge_scroll_deadline: Option<Instant>,
}

/// State held during an edge/corner resize (the `Resize` variant). Tickets
/// #9 (horizontal) and #10 (vertical + corner compose).
///
/// The grip edges and the resized column/row indices are captured at
/// classification time and frozen for the drag's duration. The committed
/// layout is also frozen (the drag is non-committing); each move teleports the
/// bystander windows to their boundary-move targets and the sole commit is on
/// release. A corner grip carries a non-`None` edge on both axes; the daemon
/// composes them by applying the horizontal and vertical boundary-moves
/// independently (they touch disjoint fields — column widths vs row heights —
/// so they commute).
pub(super) struct ResizeDrag {
    /// The layout-engine ID of the dragged (resized) window.
    pub(super) dragged_id: WindowId,
    /// The raw HWND value (for `GetWindowRect`, the `DRAGGED_HWND` global).
    pub(super) dragged_hwnd: isize,
    /// Which horizontal edge the user grabbed (`None` for a vertical-only
    /// resize). The opposite edge anchors.
    pub(super) grip_h: ResizeEdge,
    /// Which vertical edge the user grabbed (`None` for a horizontal-only
    /// resize). The opposite edge anchors.
    pub(super) grip_v: VerticalEdge,
    /// The index of the resized column, captured at classification time. The
    /// column is found via [`WindowId`] so this index stays valid even though
    /// the committed layout is frozen.
    pub(super) col: usize,
    /// The index of the dragged window's row within `col`, captured at
    /// classification time. Frozen for the drag's duration.
    pub(super) row: usize,
}

// ---------------------------------------------------------------------------
// Handler methods on FlowWM
//
// Called from `process_hook_events` in `run.rs` on MoveSizeStart/MoveSizeEnd
// and LocationChange events during a tile drag.

impl FlowWM {
    /// Begin a window drag: a tile classifies, a float just follows.
    ///
    /// Called on `MoveSizeStart` for any tracked window (and by a keymap
    /// session's `drag begin`). An active tile enters the
    /// [`DragMode::Classifying`] provisional state; an active float enters
    /// [`DragMode::Float`] directly. Anything else (minimized, hidden,
    /// ignored, untracked) sets no state.
    ///
    /// Entering `Classifying` immediately routes the dragged tile's subsequent
    /// `LOCATIONCHANGE` events into the drag path (not the float-sync path),
    /// which is what suppresses geometry snap-back from the moment the grab
    /// starts. The first `LOCATIONCHANGE` promotes the state to `Translate` or
    /// `Resize` via [`classify_drag`] (see [`Self::classify_and_promote`]). A
    /// click with no movement stays `Classifying` and is a no-op on release.
    ///
    /// A float's `LOCATIONCHANGE` events are routed here too and stored live
    /// ([`FlowWM::store_float_rect`]); holding `drag_state` keeps the
    /// animator off the window and the busy gate up for the gesture.
    pub(super) fn on_drag_start(&mut self, hwnd: isize) {
        self.on_drag_start_with(&Win32Drag, hwnd);
    }

    /// [`on_drag_start`](Self::on_drag_start) against `probe`.
    pub(super) fn on_drag_start_with<P: DragProbe>(&mut self, probe: &P, hwnd: isize) {
        // Tiling off: a drag is a plain Windows drag, nothing to classify
        // or preview (and `drag_state` stays `None`, so edge-scroll and the
        // dragged-window routing stay inert too).
        if self.tiling_paused {
            return;
        }
        // Late chrome (ADR-0028): the drag converts rects with these borders.
        let _ = self.remeasure_frame_with(&super::frame_bounds::Win32Frame, hwnd, "drag start");
        let hwnd_handle = HWND(hwnd as *mut _);
        let Some(window) = self.registry.get_window(hwnd_handle) else {
            return;
        };
        match window.state {
            WindowState::Tiling(TilingState::Active { .. }) => {}
            WindowState::Floating(FloatingState::Active { .. }) => {
                self.drag_state = Some(DragMode::Float(FloatDrag {
                    dragged_id: WindowId(hwnd),
                    dragged_hwnd: hwnd,
                }));
                set_dragged_hwnd(hwnd);
                log::debug!("drag start: hwnd={hwnd} (float)");
                return;
            }
            _ => return,
        }

        // Capture the start rect for the rect-diff classifier. Falls back to a
        // zero rect on failure (classify_drag will then see the first move as a
        // pure translate/resize from origin — a degenerate but safe behavior;
        // GetWindowRect on a live window does not fail in practice).
        let start_rect = probe.window_rect(hwnd).unwrap_or(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });

        self.drag_state = Some(DragMode::Classifying(ClassifyingDrag {
            dragged_id: WindowId(hwnd),
            dragged_hwnd: hwnd,
            start_rect,
        }));

        set_dragged_hwnd(hwnd);

        // Recolor the border to focused to give visual feedback.
        let focused_style = style_for_state(&self.config.borders, BorderState::Focused);
        if let Some(win) = self.registry.get_window_mut(hwnd_handle)
            && let Some(border) = win.border.as_ref()
        {
            border.set_style(focused_style);
        }

        log::debug!("drag start: hwnd={hwnd} (classifying)");
    }

    /// Dispatch a `LOCATIONCHANGE` for the dragged window to the active drag
    /// variant.
    ///
    /// Called on each `LOCATIONCHANGE` for the dragged window while
    /// `self.drag_state` is `Some`. The variant decides the routing:
    /// - [`DragMode::Classifying`] → [`Self::classify_and_promote`] (then fall
    ///   through to the just-promoted variant's move handler).
    /// - [`DragMode::Translate`] → [`Self::on_translate_move`] (the existing
    ///   title-bar reorder preview).
    /// - [`DragMode::Resize`] → [`Self::on_resize_move`] (the boundary-move
    ///   teleport preview).
    /// - [`DragMode::Float`] → [`Self::store_float_rect`] (the live rect, the
    ///   ring re-seated; no preview, no commit).
    pub(super) fn on_drag_move(&mut self, hwnd: isize) {
        self.on_drag_move_with(&Win32Drag, hwnd);
    }

    /// [`on_drag_move`](Self::on_drag_move) against `probe`.
    pub(super) fn on_drag_move_with<P: DragProbe>(&mut self, probe: &P, hwnd: isize) {
        // First, classify if we are still in the provisional state. Promotion
        // may install a Translate or Resize state; we then dispatch the move to
        // whichever variant is active afterwards. A fullscreen column is
        // never promoted (ADR-0021): a native drag of its caption stays
        // Classifying, shows nothing, and snaps back on release.
        if matches!(self.drag_state.as_ref(), Some(DragMode::Classifying(_)))
            && !self.is_fullscreen(hwnd)
        {
            self.classify_and_promote(probe, hwnd);
        }
        match self.drag_state.as_ref() {
            Some(DragMode::Translate(_)) => self.on_translate_move(probe, hwnd),
            Some(DragMode::Resize(_)) => self.on_resize_move(probe, hwnd),
            Some(DragMode::Float(_)) => self.store_float_rect_with(probe, hwnd),
            // Still Classifying (no movement yet), or None — nothing to do.
            _ => {}
        }
    }

    /// Classify the gesture on the first `LOCATIONCHANGE` and promote the
    /// [`DragMode::Classifying`] state into `Translate` or `Resize`.
    ///
    /// Reads the current window rect, compares it to the captured start rect
    /// via [`classify_drag`], and installs the matching variant's state:
    /// - [`DragKind::None`] → no movement yet; stay `Classifying` (a click that
    ///   has not moved). The next `LOCATIONCHANGE` retries.
    /// - [`DragKind::Translate`] → install `Translate(DragState)` with the
    ///   edge-scroll timings computed on demand, so the existing reorder path
    ///   works unchanged.
    /// - [`DragKind::Resize`] with at least one real edge (a `Left`/`Right`
    ///   horizontal edge, a `Top`/`Bottom` vertical edge, or both — a corner)
    ///   → install `Resize(ResizeDrag)` capturing the grip(s) and the resized
    ///   column + row indices. A `Resize` whose edges are both `None` (a
    ///   degenerate size-only change with no identified grip) stays
    ///   `Classifying`: the native geometry is shown mid-drag and the layout
    ///   snaps back on release (a no-op commit).
    fn classify_and_promote<P: DragProbe>(&mut self, probe: &P, hwnd: isize) {
        let classifying = match self.drag_state.take() {
            Some(DragMode::Classifying(c)) => c,
            // Not classifying — restore and return (caller should not have
            // invoked this, but be defensive).
            other => {
                self.drag_state = other;
                return;
            }
        };
        let Some(current_rect) = probe.window_rect(hwnd) else {
            // Put the Classifying state back so the next move retries.
            self.drag_state = Some(DragMode::Classifying(classifying));
            return;
        };
        match classify_drag(classifying.start_rect, current_rect) {
            DragKind::None => {
                // No movement yet — keep waiting for the first real move.
                self.drag_state = Some(DragMode::Classifying(classifying));
            }
            DragKind::Translate => {
                let edge_scroll_timings = self.compute_edge_scroll_timings();
                self.drag_state = Some(DragMode::Translate(DragState {
                    dragged_id: classifying.dragged_id,
                    dragged_hwnd: classifying.dragged_hwnd,
                    current_zone: None,
                    zone_monitor: None,
                    touched: Vec::new(),
                    edge_scroll: EdgeScrollScheduler::new(),
                    edge_scroll_timings,
                    edge_scroll_deadline: None,
                }));
                log::debug!("drag classify: hwnd={hwnd} → translate");
            }
            DragKind::Resize {
                horizontal,
                vertical,
            } => {
                // Promote when the user grabbed a real edge on at least one
                // axis (a Left/Right horizontal edge, a Top/Bottom vertical
                // edge, or both — a corner). When neither edge moved (both
                // `None`) the size-change was degenerate; stay Classifying so
                // the release snaps back.
                let has_h = matches!(horizontal, ResizeEdge::Left | ResizeEdge::Right);
                let has_v = matches!(vertical, VerticalEdge::Top | VerticalEdge::Bottom);
                if !has_h && !has_v {
                    self.drag_state = Some(DragMode::Classifying(classifying));
                    return;
                }
                // Resolve the resized column AND row index from the committed
                // layout once; they are frozen for the drag's duration.
                let mi = self.monitor_index_for_window(classifying.dragged_id);
                let pos = self.monitors[mi]
                    .active_scrolling()
                    .virtual_layout()
                    .find_window(classifying.dragged_id);
                match pos {
                    Some((col, row)) => {
                        self.drag_state = Some(DragMode::Resize(ResizeDrag {
                            dragged_id: classifying.dragged_id,
                            dragged_hwnd: classifying.dragged_hwnd,
                            grip_h: horizontal,
                            grip_v: vertical,
                            col,
                            row,
                        }));
                        log::debug!(
                            "drag classify: hwnd={hwnd} → resize grip_h={horizontal:?} grip_v={vertical:?} col={col} row={row}"
                        );
                    }
                    None => {
                        // Window not in the committed layout (stale focus /
                        // destroyed mid-classify). Restore Classifying so the
                        // drag still completes on MoveSizeEnd and snaps back,
                        // rather than leaking the DRAGGED_HWND global.
                        log::debug!(
                            "drag classify: hwnd={hwnd} not in layout; staying Classifying"
                        );
                        self.drag_state = Some(DragMode::Classifying(classifying));
                    }
                }
            }
        }
    }

    /// Compute the clamped effective edge-scroll timings for a translate drag.
    fn compute_edge_scroll_timings(&self) -> EdgeScrollTimings {
        let drag_cfg = &self.config.drag;
        let repeat_ms = drag_cfg.effective_repeat_interval_ms(&self.config.animation);
        let initial_ms = drag_cfg.effective_initial_delay_ms(repeat_ms);
        EdgeScrollTimings {
            initial_delay: Duration::from_millis(u64::from(initial_ms)),
            repeat_interval: Duration::from_millis(u64::from(repeat_ms)),
        }
    }

    /// The `Resize` variant's move handler: teleport bystander windows to their
    /// boundary-move targets.
    ///
    /// Win32 owns the dragged window's geometry during a native move-size, so
    /// its width already tracks the cursor 1:1 (overshoot and all). This reads
    /// the resulting width and applies [`resize_column_boundary_move`] to the
    /// **frozen committed** virtual layout (the drag is non-committing), then
    /// teleports the *other* windows directly (`SetWindowPos`, bypassing the
    /// animator) so the moving boundary stays fused to the cursor. The dragged
    /// window is excluded from the teleport — Win32 is already placing it. The
    /// viewport never scrolls mid-grab.
    fn on_resize_move<P: DragProbe>(&mut self, probe: &P, hwnd: isize) {
        let (dragged_hwnd, grip_h, grip_v, col, row) = match self.drag_state.as_ref() {
            Some(DragMode::Resize(r)) => (r.dragged_hwnd, r.grip_h, r.grip_v, r.col, r.row),
            _ => return,
        };
        let hwnd_handle = HWND(dragged_hwnd as *mut _);

        // The dragged window's native geometry tracks the cursor (overshoot and
        // all). Translate the window rect to a *visible* rect before feeding
        // the layout: column widths / row heights are visible quantities, and
        // feeding the window-rect dimensions would grow them by the per-edge
        // invisible borders on every drag.
        let (visible_rect, border) = match probe.window_rect(dragged_hwnd) {
            Some(r) => {
                let window = self.registry.get_window(hwnd_handle);
                let invisible_bounds = window.map(|w| w.invisible_bounds).unwrap_or_default();
                let visible_rect = invisible_bounds.window_to_visible(r);
                // Clone the Border handle out of the borrow before any later
                // `&mut self` (teleport / layout snapshot) so the NLL borrow of
                // the registry closes here. `None` if this Tile has no overlay.
                let border = window.and_then(|w| w.border.clone());
                (visible_rect, border)
            }
            None => {
                log::debug!("resize move: no window rect for {hwnd}");
                return;
            }
        };

        // Border follows the window (direct set_geometry, not the animator).
        // The resize Tile's Border is repositioned on every move so the ring
        // stays glued to the window edge — including when the Tile Overshoots
        // past a Clamp. Like every other border path, the ring wraps the window
        // with a `(thickness − overlap)` gap plus `overlap` px over the content
        // edge: the resize window has no reserved gap of its own (Win32 owns its
        // geometry, so it is never inset like a bystander), so the overlay
        // outsets by `thickness − overlap` via `float_border_rect` into the
        // inter-tile gap the neighbor already reserves — the same geometry the
        // translate-drag and bystander paths use. Without the outset the ring
        // would sit fully over the content, and the native move-size (which
        // raises the sized window above its overlay) would bury it under the
        // opaque content, making the border vanish mid-resize. Placed before the
        // "no bystander change" early-return below so the Border still tracks
        // when the resize is fully Clamped (bystanders stationary, only this
        // Tile Overshoots). The active-drag exclusion in the animator/teleport
        // path is unchanged: this set_geometry is the sole mover of the resizing
        // Tile's Border.
        if let Some(border) = border.as_ref() {
            border.set_geometry(float_border_rect(
                visible_rect,
                self.config.borders.thickness,
                self.config.borders.overlap,
            ));
        }

        // Snapshot the frozen committed layout + config + monitor of the
        // strip that holds the dragged window (not blindly the active one).
        let mi = self.drag_monitor_index();
        let (vl, config, monitor) = {
            let space = self.monitors[mi].active_scrolling();
            (
                space.virtual_layout().clone(),
                *space.config(),
                *space.monitor(),
            )
        };

        // Compose the two axes. A corner grip applies both boundary-moves;
        // they touch disjoint fields (column widths vs row heights) so they
        // commute. Either may independently return None (clamped to no change),
        // so the composition falls back to the current layout at each step.
        let preview_vl = vl.clone();
        let preview_vl = match grip_h {
            ResizeEdge::Left | ResizeEdge::Right => {
                match resize_column_boundary_move(
                    &preview_vl,
                    col,
                    grip_h,
                    visible_rect.width,
                    &config,
                ) {
                    Some(v) => v,
                    None => preview_vl,
                }
            }
            ResizeEdge::None => preview_vl,
        };
        let preview_vl = match grip_v {
            VerticalEdge::Top | VerticalEdge::Bottom => {
                match resize_row_boundary_move(
                    &preview_vl,
                    col,
                    row,
                    grip_v,
                    visible_rect.height,
                    &config,
                ) {
                    Some(v) => v,
                    None => preview_vl,
                }
            }
            VerticalEdge::None => preview_vl,
        };

        // If neither axis produced a change, bystanders are already at their
        // targets — nothing to teleport.
        if preview_vl == vl {
            return;
        }

        // Project with the FROZEN viewport (boundary-move preserves
        // viewport_offset) and teleport bystanders. The dragged window is
        // excluded by the active-drag filter inside teleport_preview.
        let actual = projection::project_with(
            &preview_vl,
            &monitor,
            &config.padding,
            config.visible_rows,
            config.layout_mode,
        );
        let applied = AppliedLayout {
            virtual_layout: preview_vl,
            actual_layout: actual,
        };
        self.teleport_preview(&applied);
    }

    /// The `Translate` variant's move handler — the existing title-bar reorder
    /// preview logic, unchanged by ticket #9.
    ///
    /// Border follows the window, the drop zone is resolved, and the other
    /// windows are previewed (non-committing) or the viewport scrolls
    /// (committed live). See the historical `on_drag_move` doc for the full
    /// flow; only the dispatch shell changed — this body is identical to the
    /// pre-resize translate-drag handler.
    fn on_translate_move<P: DragProbe>(&mut self, probe: &P, hwnd: isize) {
        let Some(drag) = self.drag_state.as_ref() else {
            return;
        };
        let dragged_id = drag.dragged_id();
        let hwnd_handle = HWND(hwnd as *mut _);

        // 1. Border follows the window (direct set_geometry, not the animator).
        let Some(window_rect) = probe.window_rect(hwnd) else {
            return;
        };
        {
            let Some(window) = self.registry.get_window(hwnd_handle) else {
                return;
            };
            let visible_rect = window.invisible_bounds.window_to_visible(window_rect);
            let border_rect = float_border_rect(
                visible_rect,
                self.config.borders.thickness,
                self.config.borders.overlap,
            );
            if let Some(border) = window.border.as_ref() {
                border.set_geometry(border_rect);
            }
        }

        // 2. Cursor position.
        let Some((cx, cy)) = probe.cursor_pos() else {
            return;
        };

        // 3. The strip under the cursor: the dragged window's own, or
        //    another monitor's (a cross-screen drag, ADR-0018). The zone is
        //    resolved against THAT monitor's work area; a cursor on no
        //    monitor (a gap, off the virtual screen) or outside the work
        //    area (a taskbar) has no zone.
        let src = self.drag_monitor_index();
        let cursor_mon = self.monitor_index_for_rect_opt(Rect {
            x: cx,
            y: cy,
            width: 1,
            height: 1,
        });
        let new_zone = cursor_mon.and_then(|m| {
            let (applied, _, monitor) = self.strip_snapshot(m);
            resolve_drop_zone(&applied, &monitor, cx, cy, &self.zone_params(m))
        });
        let new_mon = new_zone.and(cursor_mon);

        // 4. Update the tracked zone (used by on_drag_end), then act. We capture
        //    the previous move's zone BEFORE overwriting it. Two independent
        //    consumers:
        //    - Edge-scroll transition detection (below) keys off the band change.
        //    - The preview gate keys off the zone change. The animator's
        //      `RetargetFromCurrent` policy samples each window's current
        //      interpolated position as the new `from` on every submit, so
        //      resubmitting an identical (stable) target while windows are
        //      mid-flight resets the batch's progress to zero and the reflow
        //      never completes under continuous cursor movement. Because a scroll
        //      band is a distinct zone value, passing through it changes the zone
        //      and naturally invalidates this gate, so the next preview after a
        //      scroll always fires. The monitor is part of the key: column 0 of
        //      one strip is not column 0 of another.
        let (prev_zone, prev_mon) = match self.drag_state.as_mut() {
            Some(DragMode::Translate(t)) => {
                let prev = (t.current_zone, t.zone_monitor);
                t.current_zone = new_zone;
                t.zone_monitor = new_mon;
                if let Some(m) = new_mon
                    && m != src
                    && !t.touched.contains(&m)
                {
                    t.touched.push(m);
                }
                prev
            }
            _ => return,
        };

        // Edge-scroll transition detection — replaces the old per-move scroll.
        // Only band *changes* act here: entering fires the one immediate scroll
        // and arms the scheduler; leaving cancels. The repeat itself is
        // timer-driven (maybe_fire_edge_scroll), so a cursor held still in the
        // band keeps scrolling — impossible when scroll rode the move stream.
        // A band flip (Left↔Right, or the same band on another strip or
        // column) is a leave-then-enter; the new entry arms the new band.
        let prev_band = scroll_band_key(prev_mon, prev_zone);
        let new_band = scroll_band_key(new_mon, new_zone);
        let changed = should_submit_preview((prev_mon, prev_zone), (new_mon, new_zone));
        if new_band != prev_band {
            // Leaving any armed band cancels the timer.
            if prev_band.is_some() {
                let action = match self.drag_state.as_mut() {
                    Some(DragMode::Translate(t)) => t.edge_scroll.on_leave(),
                    _ => EdgeScrollAction::Cancel,
                };
                self.apply_edge_scroll_action(action);
            }
            // Entering a new band fires the immediate defining scroll, then arms
            // the first-gap timer on success (or stays idle at the content edge).
            if let Some(dir) = scroll_direction(new_zone) {
                // on_enter returns Scroll(dir) — a *request* the scheduler makes,
                // not a scroll it performs. The caller (this method) performs the
                // real scroll and feeds whether it moved back through
                // on_scroll_outcome, per the scheduler's caller protocol. So the
                // returned action is intentionally discarded here. The borrow is
                // scoped so `scroll_once_and_rearm` (which needs `&mut self`) can
                // run afterwards.
                {
                    if let Some(DragMode::Translate(t)) = self.drag_state.as_mut() {
                        let _ = t.edge_scroll.on_enter(dir);
                    }
                }
                self.scroll_once_and_rearm(dir);
                if let Some(zone) = new_zone {
                    log::debug!(
                        "drag move: hwnd={hwnd} zone={zone:?} monitor={new_mon:?} cursor=({cx},{cy})"
                    );
                }
                return;
            }
        }

        // NON-COMMITTING preview, gated on zone change: resubmitting an
        // identical target mid-flight resets the animator (see
        // `should_submit_preview`). The strip under a placement zone shows
        // the prospective layout (the committed one when the move is a
        // no-op); every other strip the drag touched shows its committed
        // layout, all in one batch. A scroll zone previews nothing (the
        // scroll committed above, or at the content edge nothing moves).
        if !changed || new_zone.is_some_and(|z| !z.is_placement()) {
            if let Some(zone) = new_zone {
                log::debug!(
                    "drag move: hwnd={hwnd} zone={zone:?} monitor={new_mon:?} cursor=({cx},{cy})"
                );
            }
            return;
        }
        let preview = match (new_mon, new_zone) {
            (Some(m), Some(zone)) => {
                let (applied, config, monitor) = self.strip_snapshot(m);
                let p = if m == src {
                    preview_move(&applied.virtual_layout, dragged_id, zone, &config, &monitor)
                } else {
                    preview_insert(&applied.virtual_layout, dragged_id, zone, &config, &monitor)
                };
                let p = p.map(|p| self.card_camera(m, p, dragged_id, zone));
                Some((m, p.unwrap_or(applied)))
            }
            _ => None,
        };
        let mut batch = self.drag_settle_layouts(preview.as_ref().map(|(m, _)| *m));
        batch.extend(preview);
        let refs: Vec<(usize, &AppliedLayout)> = batch.iter().map(|(m, l)| (*m, l)).collect();
        self.animate_previews_on(&refs);

        if let Some(zone) = new_zone {
            log::debug!(
                "drag move: hwnd={hwnd} zone={zone:?} monitor={new_mon:?} cursor=({cx},{cy})"
            );
        }
    }

    /// A book-mode card drop's camera (ADR-0022): for a
    /// [`DropZone::Card`] the preview `p` of `mi`'s strip is re-aimed so
    /// `window` is that pile's new top
    /// ([`ScrollingSpace::edge_layout`](crate::workspace::ScrollingSpace::edge_layout))
    /// and re-projected; every other zone's preview is returned as it is.
    fn card_camera(
        &self,
        mi: usize,
        p: AppliedLayout,
        window: crate::common::WindowId,
        zone: DropZone,
    ) -> AppliedLayout {
        let Some(side) = zone.card_side() else {
            return p;
        };
        let space = self.monitors[mi].active_scrolling();
        let virtual_layout = space.edge_layout(p.virtual_layout, window, side);
        AppliedLayout {
            actual_layout: space.project_layout(&virtual_layout),
            virtual_layout,
        }
    }

    /// The committed layout, mutation config and monitor of `mi`'s active
    /// strip: the read-only snapshot zone resolution and previews work on.
    fn strip_snapshot(
        &self,
        mi: usize,
    ) -> (
        AppliedLayout,
        crate::layout::mutations::MutationConfig,
        crate::layout::types::MonitorInfo,
    ) {
        let space = self.monitors[mi].active_scrolling();
        (
            AppliedLayout {
                virtual_layout: space.virtual_layout().clone(),
                actual_layout: space.actual_layout().clone(),
            },
            *space.config(),
            *space.monitor(),
        )
    }

    /// The zone-map knobs for `mi`'s strip: `[drag]` plus its `visible_rows`.
    fn zone_params(&self, mi: usize) -> ZoneParams {
        let d = &self.config.drag;
        ZoneParams {
            edge_scroll_px: d.edge_scroll_width,
            edge_scroll_ratio: d.edge_scroll_ratio,
            col_edge_ratio: d.col_edge_ratio,
            col_edge_max_px: d.col_edge_max_px,
            visible_rows: self.monitors[mi].active_scrolling().config().visible_rows,
        }
    }

    /// The committed layouts of the strips a translate drag has to keep
    /// settled — its own and every other one it touched — except `except`
    /// (the strip getting a new preview or scroll in the same batch). Empty
    /// for a drag that never left its own strip and is previewing there.
    fn drag_settle_layouts(&self, except: Option<usize>) -> Vec<(usize, AppliedLayout)> {
        let Some(DragMode::Translate(t)) = self.drag_state.as_ref() else {
            return Vec::new();
        };
        let src = self.monitor_index_for_window(t.dragged_id);
        let mut mons = vec![src];
        mons.extend(t.touched.iter().copied().filter(|&m| m != src));
        mons.into_iter()
            .filter(|&m| Some(m) != except && m < self.monitors.len())
            .map(|m| (m, self.strip_snapshot(m).0))
            .collect()
    }

    /// End the tile drag — dispatches to the active variant's sole commit
    /// point.
    ///
    /// Called on `MoveSizeEnd`. Takes `drag_state` (so [`animate_layout`] /
    /// [`teleport_preview`] include the dragged window again) and routes the
    /// payload to the variant's end handler:
    /// - [`DragMode::Translate`] → [`Self::on_translate_end`] (the existing
    ///   reorder commit — the only window-placement commit in that drag).
    /// - [`DragMode::Resize`] → [`Self::on_resize_end`] (commits the final
    ///   boundary-move layout and runs the animator once; the dragged window
    ///   snaps from its native overshoot to its clamped tile).
    /// - [`DragMode::Classifying`] → [`Self::on_classifying_end`] (a click with
    ///   no movement — the gesture never produced a classifiable rect-diff, so
    ///   it is still `Classifying` at release. Re-commits the unchanged layout
    ///   so any stray native motion snaps back; the re-commit is defensive, not
    ///   a real change). A genuine vertical, horizontal, or corner grip is
    ///   promoted out of `Classifying` on the first location-change (see
    ///   [`Self::classify_and_promote`]), so it never reaches this arm.
    ///
    /// Taking `drag_state` drops the translate edge-scroll scheduler and its
    /// armed deadline (translate variant), tearing down the auto-repeat timer —
    /// no leftover scroll continues after release.
    pub(super) fn on_drag_end(&mut self, hwnd: isize) {
        self.on_drag_end_with(&Win32Drag, hwnd);
    }

    /// [`on_drag_end`](Self::on_drag_end) against `probe`.
    pub(super) fn on_drag_end_with<P: DragProbe>(&mut self, probe: &P, hwnd: isize) {
        // Dragging a maximized window by its title bar un-maximizes it and
        // ends in MOVESIZEEND, sometimes without a STATECHANGE: an
        // OS-ignored window gets the recovery here (cheap gate first).
        if self.registry.is_os_ignored(hwnd) {
            self.on_window_state_change(hwnd);
        }
        let Some(mut drag) = self.drag_state.take() else {
            return;
        };
        let dragged_hwnd = drag.dragged_hwnd();
        // Always clear the dragged-HWND global so LOCATIONCHANGE routing returns
        // to the float-sync path, and (for Translate) tear down the scheduler.
        if let DragMode::Translate(t) = &mut drag {
            let _ = t.edge_scroll.on_drag_end();
        }
        clear_dragged_hwnd();

        // Destroyed-mid-drag guard: the window is already gone from the layout
        // and registry (on_window_destroyed handled it). Nothing to commit.
        if self
            .registry
            .get_window(HWND(dragged_hwnd as *mut _))
            .is_none()
        {
            log::debug!("drag end: window {dragged_hwnd} destroyed, skipping commit");
            return;
        }

        match drag {
            DragMode::Translate(t) => self.on_translate_end(t),
            DragMode::Resize(r) => self.on_resize_end(probe, r),
            DragMode::Classifying(c) => self.on_classifying_end(c),
            DragMode::Float(f) => self.on_float_end(probe, f),
        }
    }

    /// The `Float` variant's release: no layout to commit. The rect is
    /// stored once more because the last move's `LOCATIONCHANGE` can still be
    /// in the hook queue when a keymap session's `drag end` is read from the
    /// pipe; the write is idempotent, and it re-seats the ring.
    fn on_float_end<P: DragProbe>(&mut self, probe: &P, drag: FloatDrag) {
        self.store_float_rect_with(probe, drag.dragged_hwnd);
        self.refresh_border_for(drag.dragged_hwnd);
        log::debug!("drag end (float): hwnd={}", drag.dragged_hwnd);
    }

    /// The `Translate` variant's release commit — the only window-placement
    /// commit of a title-bar (or keymap) move drag.
    ///
    /// The drop is decided by the last move's zone and monitor
    /// ([`DragState::current_zone`] / [`DragState::zone_monitor`]). A
    /// placement zone on another monitor's strip moves the window there
    /// ([`Self::drop_on_other_monitor`]); a placement zone on its own strip
    /// reorders it; anything else (a scroll band, no zone, a full column)
    /// snaps it back into its tile. The dropped window becomes its strip's
    /// focus so its row is revealed in a stack with parked rows.
    fn on_translate_end(&mut self, drag: DragState) {
        let id = drag.dragged_id;
        let src = self.monitor_index_for_window(id);
        if let (Some(target), Some(zone)) = (drag.zone_monitor, drag.current_zone)
            && target != src
            && target < self.monitors.len()
            && zone.is_placement()
            && self.drop_on_other_monitor(&drag, src, target, zone)
        {
            log::debug!(
                "drag end (translate): hwnd={} moved to monitor {target} at {zone:?}",
                drag.dragged_hwnd
            );
            return;
        }

        let (applied, config, monitor) = self.strip_snapshot(src);
        let vl = applied.virtual_layout;
        let zone = match (drag.zone_monitor, drag.current_zone) {
            (Some(m), Some(zone)) if m == src && zone.is_placement() => Some(zone),
            _ => None,
        };
        let moved = zone.and_then(|z| {
            preview_move(&vl, id, z, &config, &monitor).map(|p| self.card_camera(src, p, id, z))
        });
        let placed = moved.is_some();
        let target_vl = moved.map_or(vl, |p| p.virtual_layout);

        // Ensure the dragged window's column is on-screen, then commit + animate.
        // drag_state is already take()n, so animate_layout INCLUDES the dragged
        // window — it visibly snaps from its mouse position into its tile.
        let target_vl = match target_vl.find_window(id) {
            Some((col, _)) => ensure_column_visible(&target_vl, col, &config),
            None => target_vl,
        };
        let strip = self.monitors[src].active_scrolling_mut();
        if placed {
            // The drop reveals its row (a stack with parked rows scrolls
            // the others off), so the strip's focus follows it.
            strip.set_focus(id);
        }
        let committed = strip.commit_layout(target_vl);
        let others = self.settle_touched(&drag, &[src]);
        let mut batch: Vec<(usize, &AppliedLayout)> = vec![(src, &committed)];
        batch.extend(others.iter().map(|(m, l)| (*m, l)));
        self.animate_layouts_on(&batch);

        // Re-resolve border style + position for the (unchanged) window state.
        self.refresh_border_for(drag.dragged_hwnd);

        log::debug!(
            "drag end (translate): hwnd={} zone={zone:?} placed={placed}",
            drag.dragged_hwnd
        );
    }

    /// Commit a cross-screen drop: `drag`'s window leaves `src`'s strip and
    /// is inserted into `target`'s at `zone`, which was resolved against
    /// that strip. Returns `false`, changing nothing, when the target
    /// refuses the zone (a full column), so the caller snaps it back.
    fn drop_on_other_monitor(
        &mut self,
        drag: &DragState,
        src: usize,
        target: usize,
        zone: DropZone,
    ) -> bool {
        let id = drag.dragged_id;
        let (applied, config, monitor) = self.strip_snapshot(target);
        let Some(p) = preview_insert(&applied.virtual_layout, id, zone, &config, &monitor) else {
            return false;
        };
        let p = self.card_camera(target, p, id, zone);
        let from = self.monitors[src].active_scrolling_mut().remove_window(id);
        let new_vl = match p.virtual_layout.find_window(id) {
            Some((col, _)) => ensure_column_visible(&p.virtual_layout, col, &config),
            None => p.virtual_layout,
        };
        let strip = self.monitors[target].active_scrolling_mut();
        strip.set_focus(id);
        let dst = strip.commit_layout(new_vl);
        let others = self.settle_touched(drag, &[src, target]);
        self.finish_tiled_monitor_move(id, (src, &from), (target, &dst), &others);
        true
    }

    /// The committed layouts of the strips `drag` touched other than the
    /// ones in `skip`, for the release batch: a strip that showed a preview
    /// but did not receive the drop settles back in the same submission.
    fn settle_touched(&self, drag: &DragState, skip: &[usize]) -> Vec<(usize, AppliedLayout)> {
        drag.touched
            .iter()
            .copied()
            .filter(|m| !skip.contains(m) && *m < self.monitors.len())
            .map(|m| (m, self.strip_snapshot(m).0))
            .collect()
    }

    /// The `Resize` variant's release commit — the sole commit of an edge/corner
    /// drag-resize.
    ///
    /// Reads the dragged window's final native geometry (the cursor position at
    /// release), applies the horizontal boundary-move ([`resize_column_boundary_move`])
    /// and the vertical boundary-move ([`resize_row_boundary_move`]) to the
    /// frozen committed layout, brings the resized column into view via
    /// [`ensure_column_visible`], then commits + animates once. `drag_state` is
    /// already `take()`n, so [`animate_layout`] includes the dragged window —
    /// it visibly snaps from its native overshoot rect into its clamped tile
    /// (the elastic-pin snap-back).
    fn on_resize_end<P: DragProbe>(&mut self, probe: &P, drag: ResizeDrag) {
        let mi = self.monitor_index_for_window(drag.dragged_id);
        let (vl, config) = {
            let space = self.monitors[mi].active_scrolling();
            (space.virtual_layout().clone(), *space.config())
        };

        // Read the final native geometry (the cursor position at release) and
        // translate it to a *visible* rect — column widths and row heights are
        // visible quantities, so feeding the window-rect dimensions would grow
        // them by the per-edge invisible borders on every drag.
        let hwnd_handle = HWND(drag.dragged_hwnd as *mut _);
        let (fallback_width, fallback_height) = vl
            .columns
            .get(drag.col)
            .and_then(|c| c.rows.get(drag.row))
            .map(|r| (vl.columns[drag.col].width_px, r.height))
            .unwrap_or((config.column_width as i32, config.available_height()));
        let visible_rect = match probe.window_rect(drag.dragged_hwnd) {
            Some(r) => {
                let invisible_bounds = self
                    .registry
                    .get_window(hwnd_handle)
                    .map(|w| w.invisible_bounds)
                    .unwrap_or_default();
                invisible_bounds.window_to_visible(r)
            }
            None => {
                log::debug!(
                    "resize end: no window rect for {}; committing layout as-is",
                    drag.dragged_hwnd
                );
                // Fall back to the column/row's current geometry so the commit
                // is a no-op snap-back rather than a silent drop.
                Rect {
                    x: 0,
                    y: 0,
                    width: fallback_width,
                    height: fallback_height,
                }
            }
        };

        // Compose the two axes (corner grip = both; edge grip = one). Each
        // boundary-move clamps to its axis's [min, max] (the elastic ceiling);
        // if an axis reports no change it falls back to the layout as-is.
        let target_vl = vl.clone();
        let target_vl = match drag.grip_h {
            ResizeEdge::Left | ResizeEdge::Right => resize_column_boundary_move(
                &target_vl,
                drag.col,
                drag.grip_h,
                visible_rect.width,
                &config,
            )
            .unwrap_or(target_vl),
            ResizeEdge::None => target_vl,
        };
        let target_vl = match drag.grip_v {
            VerticalEdge::Top | VerticalEdge::Bottom => resize_row_boundary_move(
                &target_vl,
                drag.col,
                drag.row,
                drag.grip_v,
                visible_rect.height,
                &config,
            )
            .unwrap_or(target_vl),
            VerticalEdge::None => target_vl,
        };

        // Bring the resized column into view on release (the viewport never
        // scrolled mid-grab). drag_state is take()n, so animate_layout includes
        // the dragged window — it snaps from its overshoot to its clamped tile.
        let target_vl = ensure_column_visible(&target_vl, drag.col, &config);
        let applied = self.monitors[mi]
            .active_scrolling_mut()
            .commit_layout(target_vl);
        self.animate_layout_on(mi, &applied);

        self.refresh_border_for(drag.dragged_hwnd);

        log::debug!(
            "drag end (resize): hwnd={} grip_h={:?} grip_v={:?} col={} row={}",
            drag.dragged_hwnd,
            drag.grip_h,
            drag.grip_v,
            drag.col,
            drag.row,
        );
    }

    /// The `Classifying` variant's release — a no-op commit that snaps any
    /// stray native motion back to the unchanged committed layout.
    ///
    /// Reached when the gesture never produced a classifiable movement (a pure
    /// click). The committed layout was never mutated mid-drag, so re-committing
    /// it re-projects and animates the dragged window back to its tile. The
    /// re-commit + animate is a defensive snap-back of an unchanged layout
    /// (story #4 — "nothing changes"); it produces no visible move.
    fn on_classifying_end(&mut self, drag: ClassifyingDrag) {
        let mi = self.monitor_index_for_window(drag.dragged_id);
        let vl = self.monitors[mi]
            .active_scrolling()
            .virtual_layout()
            .clone();
        let applied = self.monitors[mi].active_scrolling_mut().commit_layout(vl);
        self.animate_layout_on(mi, &applied);
        self.refresh_border_for(drag.dragged_hwnd);
        log::debug!("drag end (classifying/no-op): hwnd={}", drag.dragged_hwnd);
    }

    /// Perform one column scroll in `direction` and report whether the viewport
    /// moved.
    ///
    /// The viewport commits LIVE during a drag (a real view change the user
    /// expects to persist). `scroll_left` / `scroll_right` return `None` at the
    /// content edge, which the scheduler reads as "stop repeating". The dragged
    /// window is excluded by `animate_layout`'s filter; the others animate to
    /// their scrolled slots. Returns whether a scroll happened (the boolean the
    /// scheduler's outcome feedback consumes).
    ///
    /// The scroll acts on the strip the band was resolved on (the one under
    /// the cursor, which a cross-screen drag makes another monitor's), and a
    /// row band (`Up` / `Down`) on the column it names. The other strips the
    /// drag touched are re-settled in the same animator batch.
    fn perform_edge_scroll(&mut self, direction: Direction) -> bool {
        let (mi, zone) = match self.drag_state.as_ref() {
            Some(DragMode::Translate(t)) => (
                t.zone_monitor.unwrap_or_else(|| self.drag_monitor_index()),
                t.current_zone,
            ),
            _ => return false,
        };
        if mi >= self.monitors.len() {
            return false;
        }
        let strip = self.monitors[mi].active_scrolling_mut();
        let scrolled = match (direction, zone) {
            (Direction::Left, _) => strip.scroll_left(),
            (Direction::Right, _) => strip.scroll_right(),
            (Direction::Up, Some(DropZone::ScrollUp { col })) => {
                strip.scroll_column_rows(col, false)
            }
            (Direction::Down, Some(DropZone::ScrollDown { col })) => {
                strip.scroll_column_rows(col, true)
            }
            (Direction::Up | Direction::Down, _) => None,
        };
        let others = self.drag_settle_layouts(Some(mi));
        let mut batch: Vec<(usize, &AppliedLayout)> = others.iter().map(|(m, l)| (*m, l)).collect();
        if let Some(applied) = scrolled.as_ref() {
            batch.push((mi, applied));
        }
        if !batch.is_empty() {
            self.animate_layouts_on(&batch);
        }
        scrolled.is_some()
    }

    /// Index of the monitor whose strip holds the window being dragged, or
    /// the active monitor when no drag is in progress. Every drag handler
    /// reads and commits through this strip rather than the active monitor's:
    /// a drag can start on a background monitor, and the active monitor can
    /// change under a drag.
    fn drag_monitor_index(&self) -> usize {
        match self.drag_state.as_ref() {
            Some(d) => self.monitor_index_for_window(d.dragged_id()),
            None => self.active_monitor,
        }
    }

    /// Apply a scheduler-emitted [`EdgeScrollAction`] to the drag's armed
    /// deadline.
    ///
    /// `Arm(deadline)` stores the next-fire deadline (the main loop waits on
    /// it); `Cancel` clears it. `Scroll` is never returned by `on_scroll_outcome`
    /// — only by `on_enter` / `on_timer_fired`, whose callers perform the scroll
    /// directly and feed the outcome back, so a `Scroll` arriving here is a
    /// defensive no-op (leave the deadline untouched).
    fn apply_edge_scroll_action(&mut self, action: EdgeScrollAction) {
        let Some(drag) = self.drag_state.as_mut() else {
            return;
        };
        let DragMode::Translate(t) = drag else {
            // Edge-scroll only arms during a Translate drag.
            return;
        };
        match action {
            EdgeScrollAction::Arm(deadline) => t.edge_scroll_deadline = Some(deadline),
            EdgeScrollAction::Cancel => t.edge_scroll_deadline = None,
            EdgeScrollAction::Scroll(_) => {}
        }
    }

    /// Perform one scroll in `direction`, then feed its outcome to the scheduler
    /// and (re)arm or cancel the repeat timer per the result.
    ///
    /// Shared tail of the entry scroll ([`Self::on_drag_move`]) and each
    /// timer-fired repeat ([`Self::maybe_fire_edge_scroll`]): once the scheduler
    /// has requested a scroll via `on_enter` / `on_timer_fired`, the caller
    /// performs the real column scroll, reports whether the viewport moved back
    /// through `on_scroll_outcome`, and applies the emitted `Arm` / `Cancel` to
    /// the deadline.
    fn scroll_once_and_rearm(&mut self, direction: Direction) {
        let scrolled = self.perform_edge_scroll(direction);
        let (timings, action) = match self.drag_state.as_mut() {
            Some(DragMode::Translate(t)) => {
                let timings = t.edge_scroll_timings;
                let action = t
                    .edge_scroll
                    .on_scroll_outcome(scrolled, Instant::now(), &timings);
                (timings, action)
            }
            // Edge-scroll only arms during a Translate drag; a spurious timer
            // fire in another variant is a no-op.
            _ => return,
        };
        let _ = timings;
        self.apply_edge_scroll_action(action);
    }

    /// Fire one auto-repeat edge scroll if the armed timer's deadline arrived.
    ///
    /// A twin of [`maybe_resume_float_tracking`](super::FlowWM::maybe_resume_float_tracking):
    /// called at the top of the main loop (and inside the IPC inner loop), it
    /// drives the steady repeat cadence while the cursor stays in a band —
    /// including when the cursor is held perfectly still, which the move-driven
    /// design could not do. The timer fires in the scheduler's last-known
    /// direction (no fresh cursor read: the band is screen-edge-based, so
    /// scrolling the viewport does not move it). On each fire it performs one
    /// column scroll and re-arms or cancels per the outcome (the content edge
    /// stops it). No-op when no drag is active, no timer is armed, or the
    /// deadline has not yet arrived.
    pub(super) fn maybe_fire_edge_scroll(&mut self) {
        let deadline = match self.drag_state.as_ref() {
            Some(DragMode::Translate(t)) => t.edge_scroll_deadline,
            // Edge-scroll only arms during a Translate drag.
            _ => return,
        };
        let Some(deadline) = deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        // Deadline due: the scheduler emits a Scroll in its stored direction.
        // A spurious fire from Idle (no action armed) is a no-op.
        let dir = match self.drag_state.as_mut() {
            Some(DragMode::Translate(t)) => t.edge_scroll.on_timer_fired(),
            _ => return,
        };
        let dir = match dir {
            Some(EdgeScrollAction::Scroll(dir)) => dir,
            _ => return,
        };
        self.scroll_once_and_rearm(dir);
    }
}

// NOTE: the Win32 side of the drag lifecycle is covered through `DragProbe`
// by the session tests in `daemon::drag_session` (begin → moves → end on a
// tile and on a float); the pure pieces by the resolve_drop_zone tests in
// layout::preview and the should_submit_preview unit tests below.

/// Decide whether a new drop zone warrants a fresh preview animation during a
/// tile drag.
///
/// The preview must be gated on zone change. The animator's default
/// `RetargetFromCurrent` policy does NOT no-op on identical targets while a
/// window is mid-flight: it samples the interpolated position as the new `from`
/// and starts a fresh batch, resetting progress to zero. Resubmitting the same
/// stable target on every `LOCATIONCHANGE` (which fires per pixel of mouse
/// movement) therefore prevents the reflow from ever completing, producing the
/// slow/janky preview symptom.
///
/// Only `Row` / `Column` consult this — scroll commits live every move and
/// `None` previews nothing. Because a scroll band is a distinct zone value,
/// transitioning through it changes the zone, so the next preview after a
/// scroll always fires without a special reset.
#[must_use]
fn should_submit_preview(
    prev: (Option<usize>, Option<DropZone>),
    new: (Option<usize>, Option<DropZone>),
) -> bool {
    prev != new
}

/// The edge-scroll band direction a drop zone sits in, or `None` off-band.
///
/// Drives the transition detector in [`FlowWM::on_drag_move`]: a change in this
/// value is the only thing that acts on the scroll — entering a band fires the
/// immediate scroll, leaving cancels. `Row` / `Column` / `None` are all "off"
/// (no band), so the detector only distinguishes Left / Right / off.
#[must_use]
fn scroll_direction(zone: Option<DropZone>) -> Option<Direction> {
    match zone {
        Some(DropZone::ScrollLeft) => Some(Direction::Left),
        Some(DropZone::ScrollRight) => Some(Direction::Right),
        Some(DropZone::ScrollUp { .. }) => Some(Direction::Up),
        Some(DropZone::ScrollDown { .. }) => Some(Direction::Down),
        _ => None,
    }
}

/// Identity of the scroll band a zone sits in — the strip, the zone (which
/// names the column for a row band) — or `None` off-band. A change in this
/// value is a band leave/enter for the transition detector.
#[must_use]
fn scroll_band_key(
    monitor: Option<usize>,
    zone: Option<DropZone>,
) -> Option<(Option<usize>, DropZone)> {
    let zone = zone?;
    scroll_direction(Some(zone)).map(|_| (monitor, zone))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate on one strip (monitor 0).
    fn sp(prev: Option<DropZone>, new: Option<DropZone>) -> bool {
        should_submit_preview((Some(0), prev), (Some(0), new))
    }

    #[test]
    fn the_same_zone_on_another_monitor_submits() {
        // A cross-screen drag: column 0 of monitor 1 is not column 0 of
        // monitor 0.
        let z = Some(DropZone::Column { col: 0 });
        assert!(should_submit_preview((Some(0), z), (Some(1), z)));
        assert!(!should_submit_preview((Some(1), z), (Some(1), z)));
    }

    #[test]
    fn row_bands_map_to_vertical_directions_and_key_on_the_column() {
        assert_eq!(
            scroll_direction(Some(DropZone::ScrollUp { col: 1 })),
            Some(Direction::Up)
        );
        assert_eq!(
            scroll_direction(Some(DropZone::ScrollDown { col: 1 })),
            Some(Direction::Down)
        );
        assert_ne!(
            scroll_band_key(Some(0), Some(DropZone::ScrollDown { col: 1 })),
            scroll_band_key(Some(0), Some(DropZone::ScrollDown { col: 2 })),
            "the same band on another column is a new band"
        );
        assert_ne!(
            scroll_band_key(Some(0), Some(DropZone::ScrollRight)),
            scroll_band_key(Some(1), Some(DropZone::ScrollRight)),
            "the same band on another strip is a new band"
        );
        assert_eq!(
            scroll_band_key(Some(0), Some(DropZone::Row { col: 1, row: 0 })),
            None
        );
    }

    #[test]
    fn same_zone_is_skipped() {
        // The bug case: cursor holds still inside one zone; resubmitting would
        // reset mid-flight windows and stall the reflow.
        let z = Some(DropZone::Column { col: 2 });
        assert!(!sp(z, z));
    }

    #[test]
    fn different_column_zone_submits() {
        assert!(sp(
            Some(DropZone::Column { col: 2 }),
            Some(DropZone::Column { col: 3 }),
        ));
    }

    #[test]
    fn row_vs_column_zone_submits() {
        assert!(sp(
            Some(DropZone::Column { col: 2 }),
            Some(DropZone::Row { col: 2, row: 0 }),
        ));
    }

    #[test]
    fn first_preview_after_idle_submits() {
        assert!(sp(None, Some(DropZone::Column { col: 1 })));
    }

    #[test]
    fn scroll_band_naturally_invalidates_gate() {
        // Cursor was in a Column zone, passed through ScrollLeft, returned to
        // the same Column zone — the gate must fire (the committed layout
        // scrolled, so the preview target moved).
        let col = Some(DropZone::Column { col: 2 });
        assert!(sp(col, Some(DropZone::ScrollLeft)));
        assert!(sp(Some(DropZone::ScrollLeft), col));
    }

    #[test]
    fn leaving_to_none_and_back_submits() {
        let col = Some(DropZone::Column { col: 2 });
        assert!(sp(col, None));
        assert!(sp(None, col));
    }

    #[test]
    fn scroll_direction_maps_bands() {
        assert_eq!(
            scroll_direction(Some(DropZone::ScrollLeft)),
            Some(Direction::Left)
        );
        assert_eq!(
            scroll_direction(Some(DropZone::ScrollRight)),
            Some(Direction::Right)
        );
    }

    #[test]
    fn scroll_direction_off_band_is_none() {
        // Row / Column / None are all off-band (no scroll).
        assert_eq!(scroll_direction(Some(DropZone::Column { col: 2 })), None);
        assert_eq!(
            scroll_direction(Some(DropZone::Row { col: 2, row: 0 })),
            None
        );
        assert_eq!(scroll_direction(None), None);
    }
}
