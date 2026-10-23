//! Scrolling space — the infinite-horizontal-canvas tiling engine.
//!
//! [`ScrollingSpace`] is the main entry point for layout operations. It owns the
//! current [`VirtualLayout`], tracks focus and monocle state, and produces
//! [`AppliedLayout`] results for each mutation.
//!
//! # Where this lives
//!
//! A [`ScrollingSpace`] is the tiling half of a [`Workspace`](super::Workspace)
//! (the other half being a [`FloatingSpace`](super::FloatingSpace)). Many
//! workspaces can share a single monitor, but only the active one is visible;
//! the rest are packed "above" and "below" — the same packing idea the scrolling
//! space uses horizontally between columns, now applied vertically between
//! workspaces. This module is **pure layout math**: it never touches Win32 and
//! knows nothing of HWNDs, only [`WindowId`]s.
//!
//! # The mutation pipeline
//!
//! Every public method runs the same pipeline internally:
//! `mutations::*` (pure) → `apply_mutation` → `projection::project` →
//! [`AppliedLayout`]. The animation layer then diffs that against real on-screen
//! positions to decide what to animate (see [`AppliedLayout`]).
//!
//! # What ScrollingSpace does not own
//!
//! The scrolling space is **pure layout logic**. It does not own window
//! metadata or tile/float/ignore state (that's
//! [`WindowRegistry`](crate::registry::WindowRegistry)), animation timing (that's
//! the [`animation`](crate::animation) layer), or config loading (that's
//! [`config`](crate::config)). It only owns columns, widths, focus, and the
//! viewport offset.
//!
//! See the developer guide's *Mutation Pipeline* chapter
//! (`docs/src/dev-guide/layout/pipeline.md`) for the full flow with a worked example.

use crate::common::{Direction, WindowId};
use crate::layout::mutations::{self, MutationConfig};
use crate::layout::projection;
use crate::layout::types::{ActualLayout, AppliedLayout, MonitorInfo, Padding, VirtualLayout};

/// The scrolling space — owns virtual layout state, tracks focus, and produces applied layouts.
///
/// Single entry point for all tiling operations. Each public method applies a
/// mutation (scroll, focus, swap, resize, toggle monocle, add/remove window),
/// runs the mutation pipeline, and returns an [`AppliedLayout`].
///
/// # Focus tracking
///
/// Focus is stored as `Option<WindowId>` — a stable window identifier, not a
/// slot index — so swapping columns needs no focus fixup and focus follows the
/// window when the layout changes. Removing the focused window falls back to
/// the first available window.
///
/// # Example
///
/// ```rust
/// use flow_wm::workspace::ScrollingSpace;
/// use flow_wm::layout::types::{MonitorInfo, Padding};
/// use flow_wm::common::{Rect, WindowId, Direction};
///
/// let monitor = MonitorInfo::single(Rect { x: 0, y: 0, width: 1920, height: 1080 });
/// let mut engine = ScrollingSpace::new(monitor, 960, 320, 100, 100, Padding { window_gap: 4, up: 0, down: 0 }, 4);
///
/// // Add windows (each becomes a new column, auto-focused)
/// engine.add_window(WindowId(1));
/// engine.add_window(WindowId(2));
/// engine.add_window(WindowId(3));
///
/// // Focus is on WindowId(3) (last added). Focus left to WindowId(2).
/// let (focused, _viewport_diff) = engine.focus(WindowId(3), Direction::Left).unwrap();
/// assert_eq!(focused, WindowId(2));
/// ```
pub struct ScrollingSpace {
    /// Current virtual layout (the infinite horizontal canvas).
    virtual_layout: VirtualLayout,
    /// Per-space history cursor: the most recently interacted-with window in this space.
    ///
    /// This is NOT OS-level focus (owned by the registry via `set_focused`).
    /// See (`docs/src/dev-guide/workspace.md`).
    last_focused_window: Option<WindowId>,
    /// Last projected actual layout (cached for the `actual_layout()` accessor).
    actual: ActualLayout,
    /// The monitor being managed.
    monitor: MonitorInfo,
    /// Configuration derived from `FlowConfig`.
    config: MutationConfig,
    /// Saved column width before monocle mode (per column index), in pixels.
    monocle_saved_width: Option<(usize, i32)>,
    /// The width pattern of the monitor's current preset, one pixel width
    /// per term. With two or more terms the terms are the screen's **slots**
    /// and every column is **pinned** to its slot on every mutation
    /// ([`pin_to_slots`](Self::pin_to_slots)); with fewer the strip behaves
    /// as before (`config.column_width` is the base width and columns
    /// resize freely). See `docs/adr/0017-width-patterns-are-screen-slots.md`.
    width_pattern: Vec<u32>,
    /// Which term the first slot shows: slot `k` (0 at the anchor column)
    /// has term `(k + slot_shift) mod n`. Chosen on every pattern apply so
    /// the focused column gets the first term, kept while the pattern stays,
    /// 0 without one.
    slot_shift: usize,
}

/// Derive the cached [`MutationConfig`] — including the slot-ladder bounds
/// `max_n` and `abs_max_width` — from the monitor geometry and layout params.
///
/// Shared by [`ScrollingSpace::new`] and [`ScrollingSpace::reconfigure`] so the
/// ladder derivation has a single source. See
/// (`docs/src/dev-guide/config-and-persistence.md`).
fn build_mutation_config(
    monitor: &MonitorInfo,
    column_width: u32,
    min_column_width_px: u32,
    min_window_height_px: u32,
    min_row_height_px: u32,
    padding: Padding,
    columns_per_screen: u32,
) -> MutationConfig {
    let gap = padding.window_gap;
    let cw = column_width as i32;
    let monitor_w = monitor.work_area.width;
    let monitor_h = monitor.work_area.height;
    // abs_max_width = monitor_width − 2*gap (monocle width / ladder top).
    let abs_max_width = monitor_w - 2 * gap;
    // column_shift = column_width + window_gap — one slot step.
    let shift = cw + gap;
    // max_n: full slot steps between base width and abs_max_width.
    let max_n: u32 = if shift > 0 && abs_max_width > cw {
        ((abs_max_width - cw) / shift) as u32
    } else {
        0
    };

    MutationConfig {
        monitor_width: monitor_w,
        monitor_height: monitor_h,
        min_window_height_px,
        min_row_height_px,
        column_width,
        min_column_width_px,
        max_n,
        abs_max_width,
        padding,
        columns_per_screen,
        // The row limits are not geometry: `new` starts without them and
        // `reconfigure` / `set_monitor_info` carry the current ones over.
        visible_rows: 0,
        max_rows: 0,
    }
}

impl ScrollingSpace {
    /// Create a new scrolling space for the given monitor.
    ///
    /// `column_width` is the base column width in pixels (the `n = 0` rung of
    /// the expand/shrink slot ladder). `min_column_width_px` is the lower
    /// bound for free-form drag-resize. The slot-ladder derived values
    /// (`max_n`, `abs_max_width`) are derived (see `build_mutation_config`) from `column_width`, the
    /// monitor width, and `window_gap`, then stored in the space's
    /// [`MutationConfig`] — every mutation reuses the same ladder.
    ///
    /// # Ladder derivation
    ///
    /// - `abs_max_width = monitor_width − 2*window_gap` (the monocle width and
    ///   the two-step top of the expand ladder).
    /// - `column_shift = column_width + window_gap` (one slot step).
    /// - `max_n = floor((abs_max_width − column_width) / column_shift)` — how
    ///   many full slot steps fit between the base width and `abs_max_width`.
    ///   Clamped to `≥ 0` for degenerate configs (column wider than monitor).
    #[must_use]
    pub fn new(
        monitor: MonitorInfo,
        column_width: u32,
        min_column_width_px: u32,
        min_window_height_px: u32,
        min_row_height_px: u32,
        padding: Padding,
        columns_per_screen: u32,
    ) -> Self {
        let virtual_layout = VirtualLayout::new();
        let config = build_mutation_config(
            &monitor,
            column_width,
            min_column_width_px,
            min_window_height_px,
            min_row_height_px,
            padding,
            columns_per_screen,
        );
        let actual = projection::project(&virtual_layout, &monitor, &config.padding, 0);

        Self {
            virtual_layout,
            last_focused_window: None,
            actual,
            monitor,
            config,
            monocle_saved_width: None,
            width_pattern: Vec::new(),
            slot_shift: 0,
        }
    }

    /// The strip with its width pattern set — the construction-time form of
    /// [`set_width_pattern`](Self::set_width_pattern).
    #[must_use]
    pub fn with_width_pattern(mut self, pattern: &[u32]) -> Self {
        let _ = self.set_width_pattern(pattern);
        self
    }

    /// The current width pattern (empty without presets, one entry for a
    /// plain fraction, the period for a pattern).
    #[must_use]
    pub fn width_pattern(&self) -> &[u32] {
        &self.width_pattern
    }

    /// Whether a pattern of two or more terms pins every column's width to
    /// its screen slot (expand / shrink / set-width / monocle / drag-resize
    /// are then refused or undone on the next mutation).
    #[must_use]
    pub fn widths_pinned(&self) -> bool {
        self.width_pattern.len() > 1
    }

    /// The slot order of the current pattern: slot `k` shows term
    /// `(k + slot_shift) mod n`. 0 without a pattern.
    #[must_use]
    pub fn slot_shift(&self) -> usize {
        self.slot_shift
    }

    /// The anchor column of the current layout — the column at the
    /// viewport's left edge, where the run of slots starts — while a pattern
    /// pins the strip. `None` without a pattern or without columns.
    #[must_use]
    pub fn slot_anchor(&self) -> Option<usize> {
        if !self.widths_pinned() {
            return None;
        }
        self.anchor_of(&self.virtual_layout)
    }

    /// Install `pattern` (pixel widths per term, already floored by the
    /// caller) as this strip's width pattern: the first term becomes the
    /// base column width (the ladder's `n = 0` rung and the width new
    /// columns are created at) and, for two or more terms, every column is
    /// pinned to its slot with the slot order the strip already had (a
    /// reload or hotplug carry-over does not re-choose it). Existing widths
    /// are left alone for a single term (the reload policy); an empty
    /// pattern clears pinning and keeps the base width. Returns the
    /// re-projected layout.
    pub fn set_width_pattern(&mut self, pattern: &[u32]) -> ActualLayout {
        self.width_pattern = pattern.to_vec();
        self.slot_shift = match pattern.len() {
            0 | 1 => 0,
            n => self.slot_shift % n,
        };
        if let Some(&first) = pattern.first() {
            let _ = self.set_base_column_width(first);
        }
        if self.widths_pinned() {
            self.monocle_saved_width = None;
            let layout = self.virtual_layout.clone();
            // `apply_mutation` pins the slots and keeps the focus in view.
            let _ = self.apply_mutation(layout);
        }
        self.actual.clone()
    }

    /// `flow dispatch width <preset>` for a strip: install `pattern` and set
    /// every column to it — a single term goes through
    /// [`apply_column_width_to_all`](Self::apply_column_width_to_all); a
    /// pattern fills the screen's slots in place from the column at the left
    /// edge, with the slot order chosen so the **focused column takes the
    /// first term**. Returns the applied layout for the animator.
    pub fn apply_width_pattern(&mut self, pattern: &[u32]) -> AppliedLayout {
        self.width_pattern = pattern.to_vec();
        self.slot_shift = 0;
        match pattern {
            [] => AppliedLayout {
                actual_layout: self.actual.clone(),
                virtual_layout: self.virtual_layout.clone(),
            },
            [single] => self.apply_column_width_to_all(*single),
            [first, ..] => {
                // `refit_to_config` pins with shift 0 and seats the camera on
                // the anchor; the shift is then read off the focus's distance
                // from that anchor and the strip pinned once more.
                let _ = self.set_base_column_width(*first);
                self.monocle_saved_width = None;
                let mut new_layout = self.virtual_layout.clone();
                let anchor = self.pin_to_slots(&mut new_layout, None);
                let focus = self
                    .last_focused_window
                    .and_then(|w| new_layout.find_window(w).map(|(c, _)| c));
                if let (Some(anchor), Some(focus)) = (anchor, focus) {
                    let n = pattern.len() as i64;
                    self.slot_shift = (anchor as i64 - focus as i64).rem_euclid(n) as usize;
                }
                self.apply_mutation(new_layout)
            }
        }
    }

    /// Place the camera after every column's width changed. The column that
    /// stood at the viewport's left edge in `old` stays there, so the view
    /// still begins on a column boundary: a raw offset kept across a re-flow
    /// starts mid-column, and a column cut by the left edge is parked when a
    /// neighbour monitor lies beyond it, which leaves a bare gap (the dev
    /// guide's *Projection*, the overhang rule). Then the end-of-canvas clamp
    /// and the focus reveal as before.
    fn reanchor_after_reflow(&self, old: &VirtualLayout, new_layout: &mut VirtualLayout) {
        if new_layout.columns.is_empty() {
            return;
        }
        let gap = self.config.padding.window_gap;
        let last = new_layout.columns.len() - 1;
        let anchor = (0..old.columns.len())
            .find(|&i| column_left(old, i, gap) >= old.viewport_offset)
            .unwrap_or(last)
            .min(last);
        let canvas = projection::canvas_width(new_layout, gap);
        let max_offset = (canvas - self.config.monitor_width).max(0);
        new_layout.viewport_offset = (column_left(new_layout, anchor, gap) - gap)
            .max(0)
            .min(max_offset);
        let focus_col = self.focus_col_index().min(last);
        *new_layout = mutations::ensure_column_visible(new_layout, focus_col, &self.config);
    }

    /// The anchor of `layout`: the first column whose left edge is at or
    /// past the viewport's left edge under the widths it carries, capped so
    /// a whole run of `n` slots still fits before the strip's end. `None`
    /// without columns.
    fn anchor_of(&self, layout: &VirtualLayout) -> Option<usize> {
        let len = layout.columns.len();
        if len == 0 {
            return None;
        }
        let gap = self.config.padding.window_gap;
        let first_full = (0..len)
            .find(|&i| column_left(layout, i, gap) >= layout.viewport_offset)
            .unwrap_or(len - 1);
        Some(first_full.min(len.saturating_sub(self.width_pattern.len())))
    }

    /// Pin `layout` to the screen slots (ADR-0017): the anchor column
    /// ([`anchor_of`](Self::anchor_of)) starts the run, column `i` takes term
    /// `(i − anchor + slot_shift) mod n`, and the camera starts on the
    /// anchor, so the view always begins on a column boundary and the run
    /// fills the screen. If that leaves `keep` (else the strip's focus) past
    /// the right edge — terms adding up to less than the screen let more
    /// than `n` columns fit before the re-flow — the run is moved so that
    /// column ends it. A no-op without a pattern; returns the anchor used.
    fn pin_to_slots(&self, layout: &mut VirtualLayout, keep: Option<WindowId>) -> Option<usize> {
        if !self.widths_pinned() {
            return None;
        }
        let mut anchor = self.anchor_of(layout)?;
        self.pin_run(layout, anchor);
        let gap = self.config.padding.window_gap;
        let keep = keep
            .or(self.last_focused_window)
            .and_then(|w| layout.find_window(w).map(|(c, _)| c));
        if let Some(col) = keep {
            let right = column_left(layout, col, gap) + layout.columns[col].width_px;
            if right > layout.viewport_offset + self.config.monitor_width {
                anchor = (col + 1).saturating_sub(self.width_pattern.len());
                self.pin_run(layout, anchor);
            }
        }
        Some(anchor)
    }

    /// Widths and camera for the run of slots starting at `anchor`.
    fn pin_run(&self, layout: &mut VirtualLayout, anchor: usize) {
        let gap = self.config.padding.window_gap;
        let n = self.width_pattern.len() as i64;
        for (i, col) in layout.columns.iter_mut().enumerate() {
            let term = (i as i64 - anchor as i64 + self.slot_shift as i64).rem_euclid(n) as usize;
            col.width_px = (self.width_pattern[term] as i32)
                .min(self.config.abs_max_width)
                .max(1);
        }
        layout.viewport_offset = (column_left(layout, anchor, gap) - gap).max(0);
    }

    /// Re-derive this space's cached geometry from new layout params without
    /// disturbing window placement.
    ///
    /// Recomputes [`MutationConfig`] (including the `max_n`/`abs_max_width`
    /// ladder bounds) and re-projects the pixel layout from the **existing**
    /// [`VirtualLayout`] — columns, window order, focus, viewport offset, and
    /// monocle state are all preserved. Used by config hot-reload; see
    /// (`docs/src/dev-guide/config-and-persistence.md`).
    ///
    /// Column widths and row heights are refitted to the new bounds like
    /// [`set_monitor_info`](Self::set_monitor_info) does — a larger
    /// `window_gap` shrinks every column's row budget and the ladder top
    /// (`abs_max_width`), and the rows would otherwise spill past the work
    /// area.
    ///
    /// Returns the freshly projected [`ActualLayout`] so the caller can animate
    /// windows into their new positions (e.g. after a `window_gap` change).
    pub fn reconfigure(
        &mut self,
        column_width: u32,
        min_column_width_px: u32,
        min_window_height_px: u32,
        min_row_height_px: u32,
        padding: Padding,
        columns_per_screen: u32,
    ) -> ActualLayout {
        let old = self.config;
        self.config = build_mutation_config(
            &self.monitor,
            column_width,
            min_column_width_px,
            min_window_height_px,
            min_row_height_px,
            padding,
            columns_per_screen,
        );
        self.config.visible_rows = old.visible_rows;
        self.config.max_rows = old.max_rows;
        self.refit_to_config(&old);
        self.reproject();
        self.actual.clone()
    }

    /// The strip with `[columns] visible_rows` / `max_rows` set to
    /// `visible_rows` / `max_rows` (`0` = unlimited) — the construction-time
    /// form of [`set_row_limits`](Self::set_row_limits).
    #[must_use]
    pub fn with_row_limits(mut self, visible_rows: u32, max_rows: u32) -> Self {
        let _ = self.set_row_limits(visible_rows, max_rows);
        self
    }

    /// Change the row viewport size (`visible_rows`, `0` = every row) and the
    /// per-column cap (`max_rows`, `0` = unlimited) — the live-reload path
    /// for the two `[columns]` keys. A changed `visible_rows` re-shares every
    /// column's height over its new slot count and keeps the focused row
    /// visible; a changed `max_rows` only governs future inserts (a column
    /// already over the cap keeps its rows until windows leave it). Returns
    /// the re-projected layout for the animator.
    pub fn set_row_limits(&mut self, visible_rows: u32, max_rows: u32) -> ActualLayout {
        let old = self.config;
        self.config.visible_rows = visible_rows;
        self.config.max_rows = max_rows;
        if old.visible_rows != visible_rows {
            let mut layout = self.virtual_layout.clone();
            for col in &mut layout.columns {
                let heights = mutations::column_heights(col.rows.len(), &self.config);
                for (row, h) in col.rows.iter_mut().zip(heights) {
                    row.height = h;
                }
            }
            self.virtual_layout = layout;
        }
        self.virtual_layout = mutations::normalize_row_viewports(
            &self.virtual_layout,
            self.last_focused_window,
            &self.config,
        );
        self.reproject();
        self.actual.clone()
    }

    /// Re-project the current virtual layout into `self.actual`.
    fn reproject(&mut self) {
        self.actual = projection::project(
            &self.virtual_layout,
            &self.monitor,
            &self.config.padding,
            self.config.visible_rows,
        );
    }

    /// Bring the existing layout within the bounds of a freshly built
    /// [`MutationConfig`] (`old` is the one it replaced):
    ///
    /// - every column wider than the new `abs_max_width` is clamped to it,
    ///   so a monitor that narrowed never projects a column onto a
    ///   neighbour (the overhang rule would park such a column, hiding it);
    /// - when the per-column row budget changed — the available height
    ///   *or* the window gap, which `available − (n + 1) · gap` depends on
    ///   — every column's rows are rescaled in proportion
    ///   ([`mutations::refit_heights`]).
    fn refit_to_config(&mut self, old: &MutationConfig) {
        let max = self.config.abs_max_width;
        if self.virtual_layout.columns.iter().any(|c| c.width_px > max) {
            for col in &mut self.virtual_layout.columns {
                col.width_px = col.width_px.min(max);
            }
        }
        if self.widths_pinned() {
            let mut layout = self.virtual_layout.clone();
            let _ = self.pin_to_slots(&mut layout, None);
            self.virtual_layout = layout;
        }
        if self.config.available_height() != old.available_height()
            || self.config.padding.window_gap != old.padding.window_gap
        {
            self.virtual_layout = mutations::refit_heights(&self.virtual_layout, &self.config);
        }
    }

    /// Change only the base column width (the `n = 0` rung of the
    /// expand/shrink ladder and the width new columns are created at),
    /// keeping every other parameter and every existing column's width.
    /// [`reconfigure`](Self::reconfigure) with the current parameters.
    pub fn set_base_column_width(&mut self, column_width: u32) -> ActualLayout {
        let c = self.config;
        self.reconfigure(
            column_width,
            c.min_column_width_px,
            c.min_window_height_px,
            c.min_row_height_px,
            c.padding,
            c.columns_per_screen,
        )
    }

    /// Switch this strip to a new base column width **and** set every
    /// column to it — `flow dispatch width <preset>`. The width is clamped
    /// to `abs_max_width` (never wider than the work area); the caller
    /// applies the `min_column_width_px` floor when it derives the preset.
    /// Any monocle state is forgotten (the saved width belongs to the old
    /// ladder) and the focused column is kept in view. Returns the
    /// re-projected layout for the animator.
    pub fn apply_column_width_to_all(&mut self, column_width: u32) -> AppliedLayout {
        let old = self.virtual_layout.clone();
        let _ = self.set_base_column_width(column_width);
        let width = (column_width as i32).min(self.config.abs_max_width).max(1);
        let mut new_layout = self.virtual_layout.clone();
        for col in &mut new_layout.columns {
            col.width_px = width;
        }
        self.monocle_saved_width = None;
        self.reanchor_after_reflow(&old, &mut new_layout);
        self.apply_mutation(new_layout)
    }

    /// Replace the space's [`MonitorInfo`] (work area, virtual screen,
    /// neighbour flags) after a display-topology change and re-derive the
    /// ladder and projection from it with the current layout parameters,
    /// keeping the existing [`VirtualLayout`] — columns, order, focus and
    /// viewport offset survive a display being attached, detached or
    /// resized. The hotplug reconcile (`daemon::hotplug`) follows this with
    /// [`reconfigure`](Self::reconfigure) when the parameters themselves
    /// depend on the monitor (an auto-derived `column_width`).
    ///
    /// Row heights are the layout's source of truth (projection consumes
    /// them verbatim), so when the work area's height changed every column's
    /// rows are rescaled into the new budget in proportion
    /// ([`mutations::refit_heights`]). When it *narrowed*, every column
    /// wider than the new `abs_max_width` is clamped to it: a column wider
    /// than the work area would be parked by the overhang rule whenever a
    /// neighbour sits on that side and the user would never see it again.
    /// A change that touches neither leaves the layout alone.
    ///
    /// Returns the freshly projected [`ActualLayout`] for the new geometry.
    pub fn set_monitor_info(&mut self, monitor: MonitorInfo) -> ActualLayout {
        let old = self.config;
        self.monitor = monitor;
        self.config = build_mutation_config(
            &self.monitor,
            old.column_width,
            old.min_column_width_px,
            old.min_window_height_px,
            old.min_row_height_px,
            old.padding,
            old.columns_per_screen,
        );
        self.config.visible_rows = old.visible_rows;
        self.config.max_rows = old.max_rows;
        self.refit_to_config(&old);
        self.reproject();
        self.actual.clone()
    }

    /// Get a reference to the current virtual layout.
    #[must_use]
    pub fn virtual_layout(&self) -> &VirtualLayout {
        &self.virtual_layout
    }

    /// Get a reference to the current actual layout (projected screen coords).
    ///
    /// This is the last layout produced by [`apply_mutation`](Self::apply_mutation).
    /// It contains the current pixel-accurate rectangles for every window.
    #[must_use]
    pub fn actual_layout(&self) -> &ActualLayout {
        &self.actual
    }

    /// Get the per-space last-focused window (history cursor, NOT OS focus).
    ///
    /// See (`docs/src/dev-guide/workspace.md`).
    #[must_use]
    pub fn last_focused_window(&self) -> Option<WindowId> {
        self.last_focused_window
    }

    /// Get a reference to the monitor info.
    #[must_use]
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// Get the configured [`Padding`].
    ///
    /// The workspace-op dispatch handlers use the cached `window_gap` to
    /// compute the vertical parking unit (`monitor_height + window_gap`) for
    /// [`workspace_y_offset`](crate::workspace::workspace_y_offset). All
    /// workspaces on a given monitor share the same padding — the daemon
    /// derives a single [`Padding`](crate::layout::types::Padding) at
    /// construction and threads it into every workspace's
    /// [`ScrollingSpace`].
    #[must_use]
    pub fn padding(&self) -> Padding {
        self.config.padding
    }

    /// Get the configured `columns_per_screen` threshold.
    ///
    /// Used by dispatch hooks (e.g. auto-center after a workspace move) to
    /// decide whether the destination grid is sparse enough to recenter.
    /// All workspaces on a monitor share this value — see [`Self::padding`]
    /// for the sharing rationale.
    #[must_use]
    pub fn columns_per_screen(&self) -> u32 {
        self.config.columns_per_screen
    }

    /// Access the mutation config for dispatch-level operations.
    ///
    /// Needed by daemon dispatch for `canvas_width` and `ensure_column_visible`
    /// calls that operate on the post-insert layout.
    #[must_use]
    pub(crate) fn config(&self) -> &MutationConfig {
        &self.config
    }

    /// Apply a mutation and compute the resulting [`AppliedLayout`].
    ///
    /// This is the core pipeline that every mutation flows through:
    /// 1. Receive new [`VirtualLayout`] from the mutation function.
    /// 2. [`projection::project`] → camera shift + parking → new [`ActualLayout`].
    /// 3. Update internal state and return the new layout.
    ///
    /// This method returns the full [`ActualLayout`] and lets the animation
    /// layer decide which windows to animate by comparing each target rect
    /// against the window's real on-screen position.
    ///
    /// Before projecting, every column's row viewport is clamped and the
    /// focused window's column is scrolled so the focused row is visible
    /// ([`mutations::normalize_row_viewports`]): whatever the mutation did
    /// — focus arriving from another column, a merge, a removal, a restore
    /// — the focused window is never left parked inside its own column.
    fn apply_mutation(&mut self, new_layout: VirtualLayout) -> AppliedLayout {
        self.apply_mutation_for(new_layout, self.last_focused_window)
    }

    /// [`apply_mutation`](Self::apply_mutation) keeping `window` (the
    /// caller-resolved target of a swap/merge/promote, normally the
    /// OS-foreground tile) visible in its column instead of the strip's
    /// focus cursor.
    fn apply_mutation_for(
        &mut self,
        mut new_layout: VirtualLayout,
        window: Option<WindowId>,
    ) -> AppliedLayout {
        // A pattern decides widths by screen slot, so a camera move or a
        // structural change (insert, remove, swap) re-pins the run from the
        // column now at the left edge here, and a resize that slipped
        // through is undone on the spot.
        let _ = self.pin_to_slots(&mut new_layout, window);
        let new_layout = mutations::normalize_row_viewports(&new_layout, window, &self.config);
        let new_actual = projection::project(
            &new_layout,
            &self.monitor,
            &self.config.padding,
            self.config.visible_rows,
        );

        let result = AppliedLayout {
            actual_layout: new_actual.clone(),
            virtual_layout: new_layout,
        };

        self.virtual_layout = result.virtual_layout.clone();
        self.actual = new_actual;
        result
    }

    // -----------------------------------------------------------------------
    // Scroll operations
    // -----------------------------------------------------------------------

    /// Scroll the viewport left by one column step.
    pub fn scroll_left(&mut self) -> Option<AppliedLayout> {
        let new_layout = mutations::scroll_left(&self.virtual_layout, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Scroll the viewport right by one column step.
    pub fn scroll_right(&mut self) -> Option<AppliedLayout> {
        let new_layout = mutations::scroll_right(&self.virtual_layout, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Focus operations
    // -----------------------------------------------------------------------

    /// Move focus from `source` in the given direction, shifting the camera if
    /// the target is off-screen.
    ///
    /// `source` is the window to move focus *from*; `last_focused_window` is
    /// updated to the destination window. The caller resolves `source` (e.g.
    /// from OS-foreground or per-space focus tracking).
    ///
    /// # Returns
    ///
    /// A tuple of `(focused_window_id, optional_layout_diff)`:
    /// - `focused_window_id` — the newly focused window.
    /// - `Option<AppliedLayout>` — `Some` if the viewport scrolled (animation needed),
    ///   `None` if the target was already visible (no position changes).
    ///
    /// The caller **must** pass the `AppliedLayout` to the animation system when present.
    /// Failure to do so causes window positions to desync from the layout state,
    /// producing incorrect diffs on subsequent mutations.
    pub fn focus(
        &mut self,
        source: WindowId,
        direction: Direction,
    ) -> Option<(WindowId, Option<AppliedLayout>)> {
        let result = mutations::focus(&self.virtual_layout, source, direction, &self.config)?;
        self.last_focused_window = Some(result.focused);

        // The camera moved (left/right) or a stacked column's row viewport
        // shifted (up/down): either way windows move and the layout must
        // be animated.
        let diff = if result.new_layout != self.virtual_layout {
            Some(self.apply_mutation(result.new_layout))
        } else {
            None
        };

        Some((result.focused, diff))
    }

    /// Set the focused window explicitly by [`WindowId`].
    ///
    /// No validation is performed — the caller should ensure the window exists
    /// in the layout. This does not produce a [`AppliedLayout`]; it only updates
    /// internal focus state.
    pub fn set_focus(&mut self, window: WindowId) {
        self.last_focused_window = Some(window);
    }

    // -----------------------------------------------------------------------
    // Swap operations
    // -----------------------------------------------------------------------

    /// Swap the target window's **column** with an adjacent column (Left/Right).
    ///
    /// Columns only have horizontal neighbours, so vertical directions
    /// (`Up` / `Down`) always return `None`. Use [`swap_window`](Self::swap_window)
    /// for per-window swaps in all four directions.
    ///
    /// Focus requires no fixup — it follows the window by [`WindowId`].
    /// `target` is the caller-resolved window (typically the OS-foreground tiled window).
    pub fn swap_column(&mut self, target: WindowId, direction: Direction) -> Option<AppliedLayout> {
        let new_layout =
            mutations::swap_column(&self.virtual_layout, target, direction, &self.config)?;
        Some(self.apply_mutation_for(new_layout, Some(target)))
    }

    /// Swap the target **window** with an adjacent individual window.
    ///
    /// Unlike [`swap_column`](Self::swap_column) which moves entire columns,
    /// this swaps two specific window IDs regardless of which columns they
    /// belong to. For Left/Right, the target window is exchanged with the
    /// nearest window in the adjacent column. For Up/Down, it behaves like
    /// a row swap within the same column.
    ///
    /// Focus requires no fixup — it follows the window by [`WindowId`].
    /// `target` is the caller-resolved window (typically the OS-foreground tiled window).
    pub fn swap_window(&mut self, target: WindowId, direction: Direction) -> Option<AppliedLayout> {
        let new_layout =
            mutations::swap_window(&self.virtual_layout, target, direction, &self.config)?;
        Some(self.apply_mutation_for(new_layout, Some(target)))
    }

    // -----------------------------------------------------------------------
    // Merge / promote operations
    // -----------------------------------------------------------------------

    /// Merge the target window into the adjacent column (Left/Right).
    ///
    /// The target window is removed from its current column and appended as
    /// a new bottom row of the neighbour column. Both columns' row heights are
    /// redistributed to equal shares via [`mutations::distribute_heights`].
    ///
    /// Returns `None` when there is no neighbour in the requested direction,
    /// when the target window is unknown, or when the merge would violate
    /// `min_window_height_px` in the destination column.
    ///
    /// Focus requires no fixup — it follows the window by [`WindowId`].
    /// `target` is the caller-resolved window (typically the OS-foreground tiled window).
    pub fn merge_column(
        &mut self,
        target: WindowId,
        direction: Direction,
    ) -> Option<AppliedLayout> {
        let new_layout =
            mutations::merge_column(&self.virtual_layout, target, direction, &self.config)?;
        Some(self.apply_mutation_for(new_layout, Some(target)))
    }

    /// Promote the target window out of its column into a new standalone column.
    ///
    /// A new single-row column is inserted immediately to the Left or Right of
    /// the source column, and the target window is moved into it. The source
    /// column's remaining rows are redistributed to equal heights.
    ///
    /// Returns `None` when the target window is already alone in its column
    /// (no-op per design), when the direction is vertical, or when the target
    /// window is unknown.
    ///
    /// Focus requires no fixup — it follows the window by [`WindowId`].
    /// `target` is the caller-resolved window (typically the OS-foreground tiled window).
    pub fn promote_window(
        &mut self,
        target: WindowId,
        direction: Direction,
    ) -> Option<AppliedLayout> {
        let new_layout =
            mutations::promote_window(&self.virtual_layout, target, direction, &self.config)?;
        Some(self.apply_mutation_for(new_layout, Some(target)))
    }

    // -----------------------------------------------------------------------
    // Resize operations
    // -----------------------------------------------------------------------

    /// Expand the target column by one `column_shift` (= `column_width + window_gap`).
    ///
    /// No direction is needed — the resize is independent (no neighbor compensation).
    /// `target` is the caller-resolved window (typically the OS-foreground tiled window).
    pub fn expand_column(&mut self, target: WindowId) -> Option<AppliedLayout> {
        let new_layout = mutations::expand_column(&self.virtual_layout, target, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Shrink the target column by one `column_shift` (= `column_width + window_gap`).
    ///
    /// No direction is needed — the resize is independent (no neighbor compensation).
    /// `target` is the caller-resolved window (typically the OS-foreground tiled window).
    pub fn shrink_column(&mut self, target: WindowId) -> Option<AppliedLayout> {
        let new_layout = mutations::shrink_column(&self.virtual_layout, target, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Set the target column width to an explicit pixel value.
    ///
    /// `target` is the caller-resolved window (typically the OS-foreground tiled window).
    pub fn set_column_width(&mut self, target: WindowId, target_px: i32) -> Option<AppliedLayout> {
        let new_layout =
            mutations::set_column_width(&self.virtual_layout, target, target_px, &self.config)?;
        Some(self.apply_mutation(new_layout))
    }

    /// Return the inclusive `[min, max]` bounds (in pixels) that an explicit
    /// column width must satisfy.
    ///
    /// This is the free-form range accepted by [`set_column_width`](Self::set_column_width)
    /// and the drag-resize primitive. IPC handlers use it to validate incoming
    /// width values *before* dispatching, so an out-of-range request gets a
    /// precise error message rather than being silently rejected by the space.
    #[must_use]
    pub fn column_width_bounds(&self) -> (i32, i32) {
        (
            self.config.min_column_width_px as i32,
            self.config.abs_max_width,
        )
    }

    // -----------------------------------------------------------------------
    // Monocle toggle
    // -----------------------------------------------------------------------

    /// Toggle monocle mode for the target window.
    ///
    /// `target` is the caller-resolved window (typically the OS-foreground tiled window).
    pub fn toggle_monocle(&mut self, target: WindowId) -> Option<AppliedLayout> {
        // Resolve the target column up front, before any mutation runs. If the
        // target window isn't in the layout there's nothing to toggle, and
        // returning early here is safe because nothing has been computed yet.
        let focused_col = self.virtual_layout.find_window(target).map(|(c, _)| c)?;
        let saved = self
            .monocle_saved_width
            .and_then(|(col, w)| if col == focused_col { Some(w) } else { None });
        let (new_layout, new_saved) =
            mutations::toggle_monocle(&self.virtual_layout, target, saved, &self.config)?;

        // `focused_col` is still valid: `mutations::toggle_monocle` returns a
        // brand-new layout and does not mutate `self.virtual_layout`, and it
        // only succeeded because it located `focused`. Update the cache from
        // the already-resolved `focused_col` instead of re-looking-up with a
        // fragile `?` that could swallow the just-computed mutation.
        self.monocle_saved_width = new_saved.map(|w| (focused_col, w));

        Some(self.apply_mutation(new_layout))
    }

    // -----------------------------------------------------------------------
    // Window lifecycle
    // -----------------------------------------------------------------------

    /// Add a window as a new column appended to the right.
    ///
    /// A window that is already in the strip is **not** added again: the
    /// current layout is returned unchanged (every seating path — the
    /// restore and show hooks, the hotplug migration, the loadout leftovers
    /// — goes through one of the insert methods, and none may create a
    /// second column for a window that already has one).
    pub fn add_window(&mut self, window: WindowId) -> AppliedLayout {
        if let Some(seated) = self.already_seated(window) {
            return seated;
        }
        let new_layout = mutations::add_window(&self.virtual_layout, window, &self.config);
        self.last_focused_window = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Insert `window` as a new column at the **left end** of the strip and
    /// focus it. The counterpart of [`add_window`](Self::add_window) (which
    /// appends at the right end); used when a window arrives from the
    /// monitor to the left so it lands at the near edge. A window already in
    /// the strip is left where it is.
    pub fn prepend_window(&mut self, window: WindowId) -> AppliedLayout {
        self.insert_window_at(0, window)
    }

    /// Insert `window` as a new single-row column at column index `col_idx`
    /// (clamped to the column count, so `usize::MAX` appends), focus it and
    /// bring it into view. This is how the monitor-hotplug reconcile puts a
    /// restored column back into its old slot. A window already in the strip
    /// is left where it is and the current layout is returned.
    pub fn insert_window_at(&mut self, col_idx: usize, window: WindowId) -> AppliedLayout {
        if let Some(seated) = self.already_seated(window) {
            return seated;
        }
        let new_layout =
            mutations::insert_window_at(&self.virtual_layout, col_idx, window, &self.config);
        self.last_focused_window = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Put `window` back into column index `col_idx` (clamped, so
    /// `usize::MAX` appends) **silently**: no focus change and no viewport
    /// scroll, unlike [`insert_window_at`](Self::insert_window_at). This is
    /// how the monitor-hotplug reconcile re-seats a column Windows minimized
    /// with a display change — the user's view and focus cursor stay as they
    /// were. A window already in the strip is left where it is.
    pub fn seat_window_at(&mut self, col_idx: usize, window: WindowId) -> AppliedLayout {
        if let Some(seated) = self.already_seated(window) {
            return seated;
        }
        let new_layout =
            mutations::seat_window_at(&self.virtual_layout, col_idx, window, &self.config);
        self.apply_mutation(new_layout)
    }

    /// The current layout when `window` already has a seat in this strip
    /// (the membership guard shared by every insert method), else `None`.
    fn already_seated(&self, window: WindowId) -> Option<AppliedLayout> {
        self.virtual_layout.find_window(window)?;
        Some(AppliedLayout {
            virtual_layout: self.virtual_layout.clone(),
            actual_layout: self.actual.clone(),
        })
    }

    /// Insert a new window as a column immediately after the focused window.
    ///
    /// This is the **window-creation placement strategy**: when a new tiling
    /// window is detected, it is placed right next to the currently focused
    /// window rather than at the far right end of the layout. All columns to
    /// the right of the focused column shift rightward by one `column_shift`
    /// on the canvas. Focus then moves to the new window, and the viewport
    /// adjusts ([`mutations::ensure_column_visible`]) to keep it on screen.
    ///
    /// This runs the standard mutation pipeline (mutate → project),
    /// so the returned [`AppliedLayout`] is ready to pass straight to the
    /// animation system.
    ///
    /// # Edge cases
    ///
    /// - **No focused window** (first window in an empty layout): behaves like
    ///   [`add_window`](Self::add_window) — the new window becomes the sole
    ///   column.
    /// - **Stale focus**: if the focused window ID is no longer in the layout,
    ///   the new column is appended at the end.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use flow_wm::workspace::ScrollingSpace;
    /// # use flow_wm::layout::types::{MonitorInfo, Padding};
    /// # use flow_wm::common::WindowId;
    /// # let monitor = MonitorInfo::single(flow_wm::common::Rect { x: 0, y: 0, width: 1920, height: 1080 });
    /// # let mut engine = ScrollingSpace::new(monitor, 960, 320, 100, 100, Padding { window_gap: 4, up: 0, down: 0 }, 4);
    /// engine.add_window(WindowId(1)); // col 0, focused
    /// engine.add_window(WindowId(2)); // col 1, focused
    /// engine.set_focus(WindowId(1));  // focus back to col 0
    ///
    /// engine.insert_window(WindowId(3)); // inserted at col 1, between 1 and 2
    /// assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
    /// ```
    pub fn insert_window(&mut self, window: WindowId) -> AppliedLayout {
        if let Some(seated) = self.already_seated(window) {
            return seated;
        }
        let focused = self.last_focused_window;
        let new_layout = mutations::insert_window_after_focused(
            &self.virtual_layout,
            focused,
            window,
            &self.config,
        );
        self.last_focused_window = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Put `window` back into the seat it left — the fullscreen restore
    /// primitive. `alone` says it had column `col` to itself: then a new
    /// single-row column is inserted at `col` (clamped, so a vanished
    /// column index appends). Otherwise it becomes row `row` (clamped) of
    /// column `col` when that column still exists and has room for one
    /// more row above `min_window_height_px`, else a new column at `col`.
    /// `width_px` is the new column's width (`None` = the base
    /// `column_width`), clamped to the strip's bounds. The window becomes
    /// the strip's focus cursor and is brought into view. A window already
    /// seated is left where it is.
    pub fn restore_window_at(
        &mut self,
        col: usize,
        row: usize,
        alone: bool,
        window: WindowId,
        width_px: Option<i32>,
    ) -> AppliedLayout {
        if let Some(seated) = self.already_seated(window) {
            return seated;
        }
        let mut new_layout = self.virtual_layout.clone();
        let width = width_px.unwrap_or(self.config.column_width as i32).clamp(
            (self.config.min_column_width_px as i32).min(self.config.abs_max_width),
            self.config.abs_max_width,
        );
        // Back into its old column when that column still exists, is not
        // full (`max_rows`) and has room above `min_window_height_px`; a
        // full column is followed instead (the window becomes a new column
        // right after it), never exceeded.
        let existing = (!alone && col < new_layout.columns.len()).then(|| {
            let n = new_layout.columns[col].rows.len();
            (self.config.column_full(n), self.config.rows_fit(n + 1))
        });
        let seat_col = match existing {
            Some((false, true)) => {
                new_layout =
                    mutations::add_window_to_column(&new_layout, col, window, &self.config);
                let rows = &mut new_layout.columns[col].rows;
                let last = rows.len() - 1;
                let at = row.min(last);
                let moved = rows.remove(last);
                rows.insert(at, moved);
                col
            }
            Some((true, _)) => {
                let idx = (col + 1).min(new_layout.columns.len());
                new_layout = mutations::seat_window_at(&new_layout, idx, window, &self.config);
                new_layout.columns[idx].width_px = width;
                idx
            }
            _ => {
                let idx = col.min(new_layout.columns.len());
                new_layout = mutations::seat_window_at(&new_layout, idx, window, &self.config);
                new_layout.columns[idx].width_px = width;
                idx
            }
        };
        new_layout = mutations::ensure_column_visible(&new_layout, seat_col, &self.config);
        self.last_focused_window = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Add a window as a new row in the focused column.
    pub fn add_window_to_focused_column(&mut self, window: WindowId) -> Option<AppliedLayout> {
        let focused = self.last_focused_window?;
        let (col, _) = self.virtual_layout.find_window(focused)?;
        let new_layout =
            mutations::add_window_to_column(&self.virtual_layout, col, window, &self.config);
        self.last_focused_window = Some(window);
        Some(self.apply_mutation(new_layout))
    }

    /// Remove a window from the layout and update focus.
    ///
    /// This implements the **window-removal pipeline**:
    ///
    /// 1. **Locate** the window's `(col, row)` in the virtual layout (before
    ///    removal).
    /// 2. **Focus resolution** — if the removed window was the focused window,
    ///    pick a successor via [`mutations::next_available_window`] (same-column
    ///    sibling preferred, then left column, then right). Otherwise focus is
    ///    unchanged.
    /// 3. **Remove** the window from the virtual layout. Because projection
    ///    computes canvas positions cumulatively (a running `canvas_x`
    ///    accumulator), deleting a column naturally shifts every window to its
    ///    right leftward by one slot width — no explicit viewport adjustment is
    ///    needed. This is the "natural compression" property.
    /// 4. **Ensure visibility** — if there is a focused window after removal,
    ///    scroll the viewport so its column is on-screen
    ///    ([`mutations::ensure_column_visible`]).
    /// 5. **Apply** — project the new layout to produce the [`AppliedLayout`]
    ///    (canvas positions for every window), which is passed directly to the
    ///    animation system.
    ///
    /// # Focus fallback
    ///
    /// When the focused window is removed, focus moves to a **sibling in the
    /// same column** first — the window directly below (which slides into the
    /// closed slot), or the one above if it was the bottom of the stack. Only
    /// when the column had no other rows does focus move to the **left**
    /// adjacent column's closest-row window, then the **right**. If the removed
    /// window was the only one in the layout, focus becomes `None`.
    ///
    /// # Arguments
    ///
    /// * `window` — the [`WindowId`] to remove.
    ///
    /// # Returns
    ///
    /// An [`AppliedLayout`] describing the new layout and all window positions,
    /// to be passed to the animation system.
    pub fn remove_window(&mut self, window: WindowId) -> AppliedLayout {
        // 1. Find the window's position before removal.
        let pos = self.virtual_layout.find_window(window);
        let was_focused = self.last_focused_window == Some(window);

        // 2. Resolve the new focus if the removed window was focused.
        //    `next_available_window` operates on the pre-removal layout, so it
        //    must run before step 3.
        let new_focused = if was_focused {
            pos.and_then(|(col, row)| {
                mutations::next_available_window(&self.virtual_layout, col, row)
            })
        } else {
            self.last_focused_window
        };

        // 3. Remove the window from the virtual layout.
        //    Natural compression: deleting a column shifts right-side windows
        //    left by their slot width during projection — no explicit shift.
        let mut new_layout = mutations::remove_window(&self.virtual_layout, window, &self.config);

        // Commit the new focus.
        self.last_focused_window = new_focused;

        // 4. Ensure the focused window's column is visible (if any focus remains).
        if let Some(focused) = new_focused
            && let Some((col, _)) = new_layout.find_window(focused)
        {
            new_layout = mutations::ensure_column_visible(&new_layout, col, &self.config);
        }

        // 5. Project + diff → animation instructions.
        self.apply_mutation(new_layout)
    }

    /// Initialize the layout with multiple windows in one operation.
    ///
    /// Creates one column per window. When `widths` is `Some`, each width is
    /// quantized to the nearest slot-ladder rung; when `None`, every column
    /// gets the base [`column_width`](MutationConfig::column_width). Produces a
    /// single [`AppliedLayout`] covering all windows. See
    /// (`docs/src/dev-guide/layout/mutations.md`).
    ///
    /// Focus is set to the window at `focus_col_idx`, falling back to the
    /// last window.
    pub fn initialize_windows(
        &mut self,
        ids: Vec<WindowId>,
        focus_col_idx: Option<usize>,
        widths: Option<&[u32]>,
    ) -> AppliedLayout {
        let new_layout = mutations::initialize_windows(&ids, &self.config, focus_col_idx, widths);
        // Focus the window at the focus column (the user's foreground window),
        // falling back to the last window when no focus column was specified.
        self.last_focused_window = focus_col_idx
            .and_then(|idx| ids.get(idx).copied())
            .or_else(|| ids.last().copied());
        self.apply_mutation(new_layout)
    }

    /// Swap in an entire layout wholesale — the loadout restore primitive.
    ///
    /// Builds a new [`VirtualLayout`] from `columns` (each `(width_px, Vec<WindowId>)`
    /// is one column; rows share equal heights) and `viewport_offset` via the pure
    /// [`mutations::set_layout`], updates `last_focused_window` to `focus`, then runs
    /// the standard [`apply_mutation`](Self::apply_mutation) pipeline so the returned
    /// [`AppliedLayout`] is ready for the animation system. Used by the loadout load
    /// path to rearrange windows into a previously-saved arrangement.
    ///
    /// Unlike [`initialize_windows`](Self::initialize_windows) (one column per window),
    /// this can express multi-row merged columns.
    #[must_use]
    pub fn set_layout(
        &mut self,
        columns: Vec<(u32, Vec<WindowId>)>,
        focus: Option<WindowId>,
        viewport_offset: i32,
    ) -> AppliedLayout {
        let new_layout = mutations::set_layout(&self.config, columns, viewport_offset);
        self.last_focused_window = focus;
        self.apply_mutation(new_layout)
    }

    // -----------------------------------------------------------------------
    // Viewport center operations (prefix-sum, variable-width aware)
    // -----------------------------------------------------------------------

    /// Center the viewport so the target window's column lands at the monitor midpoint.
    ///
    /// Uses the actual prefix-sum canvas position (variable-width aware), not
    /// the slot grid. Always computes `canvas_x(focused) − (monitor_width −
    /// focused_width) / 2`, even when all columns fit — this is the explicit
    /// center command behavior. Returns `None` when the layout is empty or
    /// `target` is not found. `target` is the caller-resolved window (typically
    /// the OS-foreground tiled window). See
    /// (`docs/src/dev-guide/layout/mutations.md`).
    pub fn center_focused_column(&mut self, target: WindowId) -> Option<AppliedLayout> {
        if self.virtual_layout.columns.is_empty() {
            return None;
        }
        let focus_col = self.virtual_layout.find_window(target).map(|(c, _)| c)?;
        debug_assert!(
            focus_col < self.virtual_layout.columns.len(),
            "focus_col {focus_col} out of range"
        );
        let mut new_layout = self.virtual_layout.clone();
        new_layout.viewport_offset =
            mutations::center_viewport_on_focused(&self.virtual_layout, focus_col, &self.config);
        Some(self.apply_mutation(new_layout))
    }

    /// Center the viewport so the entire canvas is centered in the monitor.
    ///
    /// `(canvas_width − monitor_width) / 2` — may be negative when the canvas
    /// is narrower than the monitor. Used by init and MoveWindowToWorkspace
    /// fit cases. Returns `None` when the layout is empty. See
    /// (`docs/src/dev-guide/layout/mutations.md`).
    pub fn center_canvas(&mut self) -> Option<AppliedLayout> {
        if self.virtual_layout.columns.is_empty() {
            return None;
        }
        let mut new_layout = self.virtual_layout.clone();
        new_layout.viewport_offset =
            mutations::center_viewport_canvas(&self.virtual_layout, &self.config);
        Some(self.apply_mutation(new_layout))
    }

    /// Insert a new window as a column after the focused window, preserving
    /// the moved window's width.
    ///
    /// Like [`insert_window`](Self::insert_window) but the new column is
    /// created at `width_px` (quantized to the nearest slot-ladder rung)
    /// instead of the base [`column_width`](MutationConfig::column_width).
    /// This preserves a window's width when moving it between workspaces.
    /// See (`docs/src/dev-guide/layout/mutations.md`).
    pub fn insert_window_with_width(&mut self, window: WindowId, width_px: u32) -> AppliedLayout {
        let focused = self.last_focused_window;
        let width = mutations::quantize_to_ladder(width_px as i32, &self.config);
        let new_layout = mutations::insert_window_after_focused_with_width(
            &self.virtual_layout,
            focused,
            window,
            width,
            &self.config,
        );
        self.last_focused_window = Some(window);
        self.apply_mutation(new_layout)
    }

    /// Shift the viewport so the focused column is visible with a gap margin.
    ///
    /// Delegates to [`mutations::ensure_column_visible`] on the focused
    /// column. Used by the MoveWindowToWorkspace overflow case to bring the
    /// moved window into view. See (`docs/src/dev-guide/layout/mutations.md`).
    ///
    /// Inside a stacked column the focused **row** is revealed as well
    /// (the row viewport shifts, [`mutations::ensure_row_visible`]), so a
    /// mouse click, a focus-monitor arrival or a workspace switch landing on
    /// a parked row scrolls it on screen.
    pub(crate) fn ensure_focused_visible(&mut self) -> Option<AppliedLayout> {
        if self.virtual_layout.columns.is_empty() {
            return None;
        }
        let focus_col = self.focus_col_index();
        let mut new_layout =
            mutations::ensure_column_visible(&self.virtual_layout, focus_col, &self.config);
        if let Some(f) = self.last_focused_window {
            new_layout = mutations::ensure_row_visible(&new_layout, f, &self.config);
        }
        // Already visible — no scroll, no diff. This makes the method a natural
        // no-op for self-induced foreground changes re-entering via the hook.
        if new_layout == self.virtual_layout {
            return None;
        }
        Some(self.apply_mutation(new_layout))
    }

    /// Resolve the focused window's column index.
    ///
    /// Falls back to index 0 when no window is focused or the focused window
    /// is no longer in the layout (stale focus state).
    pub(crate) fn focus_col_index(&self) -> usize {
        self.last_focused_window
            .and_then(|w| self.virtual_layout.find_window(w).map(|(c, _)| c))
            .unwrap_or(0)
    }

    /// Replace the committed virtual layout with a pre-computed one and
    /// re-project.
    ///
    /// This is the escape hatch for operations that need to apply a
    /// mutation that doesn't map to a single `ScrollingSpace` method
    /// (e.g. tile-drag commit which inserts at an arbitrary column index).
    /// The caller is responsible for ensuring the new layout is valid.
    ///
    /// Returns the resulting [`AppliedLayout`].
    pub(crate) fn commit_layout(&mut self, new_layout: VirtualLayout) -> AppliedLayout {
        self.apply_mutation(new_layout)
    }
}

/// Canvas x of column `idx`'s left edge: the leading gap plus every earlier
/// column's slot (`width_px + gap`), the prefix sum the projection walks.
fn column_left(layout: &VirtualLayout, idx: usize, gap: i32) -> i32 {
    gap + layout.columns[..idx]
        .iter()
        .map(|c| c.width_px + gap)
        .sum::<i32>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::Rect;

    fn test_monitor() -> MonitorInfo {
        MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        })
    }

    fn test_padding() -> Padding {
        Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        }
    }

    fn engine_with_three_columns() -> ScrollingSpace {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));
        engine
    }

    #[test]
    fn engine_add_windows_and_focus() {
        let engine = engine_with_three_columns();
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
        assert_eq!(engine.virtual_layout().columns.len(), 3);
        assert_eq!(engine.virtual_layout().window_count(), 3);
    }

    #[test]
    fn restore_window_at_puts_a_window_back_into_its_seat_or_appends() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));

        // Alone at column 1, 1200 wide: a new column at index 1.
        let applied = engine.restore_window_at(1, 0, true, WindowId(9), Some(1200));
        assert_eq!(
            applied.virtual_layout.find_window(WindowId(9)),
            Some((1, 0))
        );
        assert_eq!(applied.virtual_layout.columns[1].width_px, 1200);
        assert_eq!(applied.virtual_layout.columns.len(), 4);
        assert_eq!(engine.last_focused_window(), Some(WindowId(9)));

        // Shared column 0, row 0: back on top of window 1.
        let applied = engine.restore_window_at(0, 0, false, WindowId(8), None);
        assert_eq!(
            applied.virtual_layout.find_window(WindowId(8)),
            Some((0, 0))
        );
        assert_eq!(
            applied.virtual_layout.find_window(WindowId(1)),
            Some((0, 1))
        );
        assert_eq!(applied.virtual_layout.columns[0].rows.len(), 2);
        let heights: Vec<i32> = applied.virtual_layout.columns[0]
            .rows
            .iter()
            .map(|r| r.height)
            .collect();
        assert_eq!(
            heights.iter().sum::<i32>(),
            1080 - 3 * test_padding().window_gap
        );

        // A column index past the end appends; `None` takes the base width.
        let applied = engine.restore_window_at(99, 5, true, WindowId(7), None);
        let last = applied.virtual_layout.columns.len() - 1;
        assert_eq!(
            applied.virtual_layout.find_window(WindowId(7)),
            Some((last, 0))
        );
        assert_eq!(applied.virtual_layout.columns[last].width_px, 960);

        // A column that cannot take another row (min height) gets a new
        // column at that index instead.
        let mut tight = ScrollingSpace::new(test_monitor(), 960, 320, 600, 100, test_padding(), 4);
        tight.add_window(WindowId(1));
        let applied = tight.restore_window_at(0, 1, false, WindowId(2), None);
        assert_eq!(applied.virtual_layout.columns.len(), 2);
        assert_eq!(
            applied.virtual_layout.find_window(WindowId(2)),
            Some((0, 0))
        );

        // Already seated: nothing changes.
        let before = engine.virtual_layout().clone();
        let applied = engine.restore_window_at(0, 0, true, WindowId(7), None);
        assert_eq!(applied.virtual_layout, before);
    }

    /// Three columns, the view starting on the second one. Switching the
    /// widths must keep that column at the left edge (the view begins on a
    /// column boundary) instead of keeping the old pixel offset, which put
    /// the camera mid-column: a gap where the cut column was parked and the
    /// last column spilling off the right edge.
    #[test]
    fn width_changes_keep_the_left_edge_column_anchored() {
        let gap = test_padding().window_gap; // 4
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3)); // focus
        let col_left = |layout: &VirtualLayout, i: usize| column_left(layout, i, gap);

        // 1/3 + 2/3 of a 1920 screen: 636 + 1272 + 3 gaps = 1920. The focus
        // (column 2) must stay in view, so the run starts at column 1 and
        // the focus, last in the run, takes the first term.
        engine.apply_width_pattern(&[636, 1272]);
        assert_run(&engine, 1, &[1272, 636]);

        // Uniform 1/2 (954 + 954 + 3 gaps = 1920), through the preset entry
        // point so the pattern pin is released first: column 1 stays at the
        // left edge and column 2 ends exactly at the right edge.
        let applied = engine.apply_width_pattern(&[954]);
        let vl = &applied.virtual_layout;
        assert_eq!(vl.viewport_offset, col_left(vl, 1) - gap);
        assert_eq!(col_left(vl, 2) + 954 + gap, vl.viewport_offset + 1920);
        assert_eq!(engine.slot_shift(), 0);
        assert_eq!(engine.slot_anchor(), None);

        // Back to the pattern: the same column is anchored again.
        engine.apply_width_pattern(&[636, 1272]);
        assert_run(&engine, 1, &[1272, 636]);
    }

    /// Every column's width, left to right.
    fn widths(engine: &ScrollingSpace) -> Vec<i32> {
        engine
            .virtual_layout()
            .columns
            .iter()
            .map(|c| c.width_px)
            .collect()
    }

    /// The run of slots starts at `anchor`: the camera sits one gap before
    /// it, the columns from it have the slot widths, and the last slot ends
    /// at or before the right edge of the 1920 px test monitor.
    fn assert_run(engine: &ScrollingSpace, anchor: usize, slots: &[i32]) {
        let gap = test_padding().window_gap;
        let vl = engine.virtual_layout();
        assert_eq!(
            engine.slot_anchor(),
            Some(anchor),
            "anchor {:?}",
            widths(engine)
        );
        assert_eq!(
            vl.viewport_offset,
            (column_left(vl, anchor, gap) - gap).max(0),
            "camera on the anchor"
        );
        for (k, w) in slots.iter().enumerate() {
            assert_eq!(
                vl.columns[anchor + k].width_px,
                *w,
                "slot {k} of {:?}",
                widths(engine)
            );
        }
        let last = anchor + slots.len() - 1;
        assert!(
            column_left(vl, last, gap) + vl.columns[last].width_px <= vl.viewport_offset + 1920,
            "the run fits the screen"
        );
    }

    /// Whether column `idx` is wholly on screen.
    fn visible(engine: &ScrollingSpace, idx: usize) -> bool {
        let gap = test_padding().window_gap;
        let vl = engine.virtual_layout();
        let left = column_left(vl, idx, gap);
        left >= vl.viewport_offset && left + vl.columns[idx].width_px <= vl.viewport_offset + 1920
    }

    #[test]
    fn mod_r_gives_the_focused_column_the_first_term() {
        // Three columns of 634 (3 × 634 + 4 gaps = 1918) all on screen.
        let mut engine = ScrollingSpace::new(test_monitor(), 634, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        // "1/2+1/4+1/4": 954 + 475 + 475 + 4 gaps = 1920.
        engine.set_focus(WindowId(2));
        engine.apply_width_pattern(&[954, 475, 475]);
        assert_run(&engine, 0, &[475, 954, 475]);
        assert_eq!(engine.slot_shift(), 2);
        engine.set_focus(WindowId(3));
        engine.apply_width_pattern(&[954, 475, 475]);
        assert_run(&engine, 0, &[475, 475, 954]);
        assert_eq!(engine.slot_shift(), 1);
        engine.set_focus(WindowId(1));
        engine.apply_width_pattern(&[954, 475, 475]);
        assert_run(&engine, 0, &[954, 475, 475]);
        assert_eq!(engine.slot_shift(), 0);

        // "2/3+1/3" with the right window focused: it is the big one.
        let mut two = ScrollingSpace::new(test_monitor(), 954, 320, 100, 100, test_padding(), 4);
        two.add_window(WindowId(1));
        two.add_window(WindowId(2));
        two.apply_width_pattern(&[1272, 636]);
        assert_run(&two, 0, &[636, 1272]);
    }

    #[test]
    fn focus_past_the_edge_re_pins_the_run_and_keeps_the_slots() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3)); // focus
        engine.apply_width_pattern(&[1272, 636]);
        // The run is columns 1 and 2, the focus (column 2) big.
        assert_run(&engine, 1, &[636, 1272]);
        assert!(!visible(&engine, 0));

        // Focus within the run: nothing moves.
        let (focused, diff) = engine
            .focus(WindowId(3), Direction::Left)
            .expect("neighbour");
        assert_eq!(focused, WindowId(2));
        assert!(diff.is_none());
        assert_run(&engine, 1, &[636, 1272]);

        // Past the left edge: column 0 comes in and takes the left slot,
        // column 1 moves to the right slot, column 2 leaves on the right.
        let (focused, diff) = engine
            .focus(WindowId(2), Direction::Left)
            .expect("neighbour");
        assert_eq!(focused, WindowId(1));
        assert!(diff.is_some());
        assert_run(&engine, 0, &[636, 1272]);
        assert!(!visible(&engine, 2));

        // And back past the right edge: column 2 takes the right slot again.
        let _ = engine.focus(WindowId(1), Direction::Right);
        let (_, diff) = engine
            .focus(WindowId(2), Direction::Right)
            .expect("neighbour");
        assert!(diff.is_some());
        assert_run(&engine, 1, &[636, 1272]);
        assert!(!visible(&engine, 0));
    }

    #[test]
    fn a_column_added_next_to_the_focus_takes_its_slot_and_removal_keeps_a_full_run() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2)); // focus
        engine.apply_width_pattern(&[1272, 636]);
        assert_run(&engine, 0, &[636, 1272]);

        // A new column after the focus: the run moves one column right so
        // the new, focused column is in it; column 0 leaves on the left and
        // the slots stay where they were on screen.
        engine.add_window(WindowId(3));
        assert_run(&engine, 1, &[636, 1272]);
        assert!(!visible(&engine, 0));

        // Removing the anchor: the run re-forms from what is left.
        engine.remove_window(WindowId(2));
        assert_eq!(widths(&engine), vec![636, 1272]);
        assert_run(&engine, 0, &[636, 1272]);
    }

    #[test]
    fn swapping_visible_columns_moves_the_windows_not_the_slots() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2)); // focus
        engine.apply_width_pattern(&[1272, 636]);
        assert_run(&engine, 0, &[636, 1272]);
        engine
            .swap_column(WindowId(2), Direction::Left)
            .expect("swapped");
        let vl = engine.virtual_layout();
        assert_eq!(vl.columns[0].rows[0].window_id, WindowId(2));
        assert_eq!(vl.columns[1].rows[0].window_id, WindowId(1));
        assert_run(&engine, 0, &[636, 1272]);
    }

    #[test]
    fn fewer_slots_keep_the_anchor_and_the_focus_in_view_and_more_slots_grow_right() {
        // Four columns of 475 (4 × 475 + 5 gaps = 1920), all on screen.
        let mut engine = ScrollingSpace::new(test_monitor(), 475, 320, 100, 100, test_padding(), 4);
        for id in 1..=4 {
            engine.add_window(WindowId(id));
        }
        engine.apply_width_pattern(&[475]);
        assert!(visible(&engine, 0) && visible(&engine, 3));

        // Three slots with the first column focused: columns 0..2 take them,
        // column 3 leaves on the right.
        engine.set_focus(WindowId(1));
        engine.apply_width_pattern(&[954, 475, 475]);
        assert_run(&engine, 0, &[954, 475, 475]);
        assert!(!visible(&engine, 3));

        // The same with the last column focused: the run moves right by the
        // minimum that keeps it in view, so columns 1..3 take the slots, the
        // focus (last in the run) is the big one, column 0 leaves on the left.
        engine.apply_width_pattern(&[475]);
        engine.set_focus(WindowId(4));
        engine.apply_width_pattern(&[954, 475, 475]);
        assert_run(&engine, 1, &[475, 475, 954]);
        assert!(!visible(&engine, 0));

        // Back to four slots: the run grows to the right from the anchor,
        // and with no column beyond 3 the camera comes back so all four fit.
        engine.apply_width_pattern(&[475]);
        assert_eq!(engine.virtual_layout().viewport_offset, 0);
        assert!(visible(&engine, 0) && visible(&engine, 3));

        // With a fifth column the grown run is columns 1..4.
        engine.add_window(WindowId(5));
        engine.set_focus(WindowId(4));
        engine.apply_width_pattern(&[954, 475, 475]);
        assert_run(&engine, 1, &[475, 475, 954]);
        engine.apply_width_pattern(&[475]);
        assert!(!visible(&engine, 0));
        assert!(visible(&engine, 1) && visible(&engine, 4));
    }

    #[test]
    fn terms_under_one_screen_keep_the_focus_in_view() {
        // Five columns of 300 all on screen; "1/2+1/10"-sized slots (954 +
        // 188) leave room, so more than two old columns fit before the pin.
        let mut engine = ScrollingSpace::new(test_monitor(), 300, 320, 100, 100, test_padding(), 4);
        for id in 1..=5 {
            engine.add_window(WindowId(id));
        }
        engine.apply_width_pattern(&[954, 188]);
        // The focus (column 4) ends the run and takes the first term.
        assert_run(&engine, 3, &[188, 954]);
        assert!(visible(&engine, 4));
    }

    #[test]
    fn a_carried_over_pattern_keeps_its_slot_order() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2)); // focus
        engine.apply_width_pattern(&[1272, 636]);
        assert_eq!(engine.slot_shift(), 1);
        // A reload or hotplug re-installs the (re-floored) pattern: same order.
        engine.set_width_pattern(&[1272, 636]);
        assert_eq!(engine.slot_shift(), 1);
        assert_run(&engine, 0, &[636, 1272]);
        // A fraction has no slot order.
        engine.set_width_pattern(&[954]);
        assert_eq!(engine.slot_shift(), 0);
        assert!(!engine.widths_pinned());
    }

    #[test]
    fn apply_column_width_to_all_resizes_every_column_and_rebases_the_ladder() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        let _ = engine.expand_column(WindowId(2));
        let _ = engine.toggle_monocle(WindowId(3));
        let _ = engine.scroll_right();
        assert_ne!(engine.virtual_layout().viewport_offset, 0);

        let applied = engine.apply_column_width_to_all(600);
        assert!(
            applied
                .virtual_layout
                .columns
                .iter()
                .all(|c| c.width_px == 600),
            "{:?}",
            applied.virtual_layout.columns
        );
        assert_eq!(engine.config().column_width, 600);
        assert_eq!(applied.actual_layout.entries.len(), 3);
        // Three 600 px columns fit: the camera is pulled back onto the canvas.
        let canvas = projection::canvas_width(&applied.virtual_layout, test_padding().window_gap);
        assert!(canvas <= 1920);
        assert!(applied.virtual_layout.viewport_offset <= 0);
        // Monocle memory belongs to the old ladder and is forgotten.
        assert!(engine.monocle_saved_width.is_none());
        // Wider than the work area is clamped to abs_max_width.
        let applied = engine.apply_column_width_to_all(5000);
        assert!(
            applied
                .virtual_layout
                .columns
                .iter()
                .all(|c| c.width_px == 1920 - 2 * test_padding().window_gap)
        );
        // The ladder floor follows the new base width.
        assert!(engine.shrink_column(WindowId(1)).is_none());
    }

    #[test]
    fn set_monitor_info_refits_row_heights_and_reprojects_into_the_new_work_area() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine
            .add_window_to_focused_column(WindowId(3))
            .expect("column 1 gains a second row");
        // The monitor lost 80 px to a taller taskbar.
        let shorter = MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1000,
        });
        let actual = engine.set_monitor_info(shorter);
        assert_eq!(engine.monitor().work_area.height, 1000);
        // Every projected rect fits the new work area …
        for entry in &actual.entries {
            assert!(entry.rect.bottom() <= 1000, "{entry:?}");
        }
        // … the layout itself is untouched …
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
        // … and the ladder was re-derived from the new monitor.
        assert_eq!(engine.config().monitor_height, 1000);
        assert_eq!(engine.config().available_height(), 1000);
    }

    #[test]
    fn set_monitor_info_clamps_column_widths_when_the_work_area_narrows() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        // Column 0 at the monocle width of the 1920 monitor.
        engine
            .toggle_monocle(WindowId(1))
            .expect("monocle on column 0");
        let wide = engine.virtual_layout().columns[0].width_px;
        assert_eq!(wide, 1920 - 2 * test_padding().window_gap);
        // The display is replaced by a 1280-wide one.
        let narrower = MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1280,
            height: 1080,
        });
        let actual = engine.set_monitor_info(narrower);
        let max = engine.config().abs_max_width;
        assert_eq!(max, 1280 - 2 * test_padding().window_gap);
        assert_eq!(engine.virtual_layout().columns[0].width_px, max);
        // The narrower column was left alone.
        assert_eq!(engine.virtual_layout().columns[1].width_px, 960);
        // Nothing projects past the new right edge.
        let col0 = actual.find(WindowId(1)).expect("column 0 projected").rect;
        assert!(col0.right() <= 1280, "{col0:?}");
    }

    #[test]
    fn reconfigure_refits_row_heights_when_only_the_gap_changes() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine
            .add_window_to_focused_column(WindowId(2))
            .expect("second row");
        let rows_sum = |engine: &ScrollingSpace| -> i32 {
            engine.virtual_layout().columns[0]
                .rows
                .iter()
                .map(|r| r.height)
                .sum()
        };
        let before = rows_sum(&engine);
        // Same monitor, same widths, a 100 px gap instead of 4: the two rows
        // plus three gaps must still fit the 1080 budget.
        let big_gap = Padding {
            window_gap: 100,
            up: 0,
            down: 0,
        };
        let actual = engine.reconfigure(960, 320, 100, 100, big_gap, 4);
        let after = rows_sum(&engine);
        assert_eq!(after, 1080 - 3 * 100);
        assert!(after < before);
        for entry in &actual.entries {
            assert!(entry.rect.bottom() <= 1080, "{entry:?}");
        }
    }

    #[test]
    fn adding_a_seated_window_again_does_not_create_a_second_column() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.set_focus(WindowId(2));
        let before = engine.virtual_layout().clone();
        let applied = engine.add_window(WindowId(1));
        assert_eq!(engine.virtual_layout(), &before);
        assert_eq!(&applied.virtual_layout, &before);
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
        engine.insert_window(WindowId(1));
        engine.prepend_window(WindowId(1));
        engine.insert_window_at(1, WindowId(1));
        engine.seat_window_at(0, WindowId(1));
        assert_eq!(engine.virtual_layout(), &before);
        assert_eq!(engine.virtual_layout().window_count(), 2);
    }

    #[test]
    fn seat_window_at_leaves_focus_and_viewport_alone() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        for id in 1..=4 {
            engine.insert_window(WindowId(id));
        }
        // Inserting (and revealing) 4 scrolled the viewport right.
        let offset = engine.virtual_layout().viewport_offset;
        assert!(offset > 0);
        engine.seat_window_at(1, WindowId(9));
        assert_eq!(engine.virtual_layout().viewport_offset, offset);
        assert_eq!(engine.last_focused_window(), Some(WindowId(4)));
        assert_eq!(
            engine.virtual_layout().columns[1].rows[0].window_id,
            WindowId(9)
        );
    }

    #[test]
    fn insert_window_at_seats_a_window_at_the_given_column() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(3));
        engine.insert_window_at(1, WindowId(2));
        let order: Vec<WindowId> = engine
            .virtual_layout()
            .columns
            .iter()
            .map(|c| c.rows[0].window_id)
            .collect();
        assert_eq!(order, vec![WindowId(1), WindowId(2), WindowId(3)]);
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
        // Past the end appends.
        engine.insert_window_at(usize::MAX, WindowId(4));
        assert_eq!(
            engine.virtual_layout().columns[3].rows[0].window_id,
            WindowId(4)
        );
    }

    #[test]
    fn reconfigure_preserves_window_order_focus_and_count() {
        let mut engine = engine_with_three_columns();
        // Snapshot the column -> window mapping and focus before reconfiguring.
        let order_before: Vec<WindowId> = engine
            .virtual_layout()
            .columns
            .iter()
            .map(|c| c.rows[0].window_id)
            .collect();
        let focused_before = engine.last_focused_window();

        // Reconfigure with materially different geometry params.
        let new_padding = Padding {
            window_gap: 40,
            up: 0,
            down: 0,
        };
        engine.reconfigure(800, 300, 90, 90, new_padding, 2);

        // Virtual layout (columns + window order) is untouched.
        assert_eq!(engine.virtual_layout().columns.len(), order_before.len());
        let order_after: Vec<WindowId> = engine
            .virtual_layout()
            .columns
            .iter()
            .map(|c| c.rows[0].window_id)
            .collect();
        assert_eq!(
            order_after, order_before,
            "reconfigure must preserve column order"
        );
        // The focus cursor is untouched.
        assert_eq!(engine.last_focused_window(), focused_before);
    }

    #[test]
    fn reconfigure_updates_gap_and_reprojects_actual() {
        let mut engine = engine_with_three_columns();
        let first_before = engine
            .actual_layout()
            .find(WindowId(1))
            .expect("window 1 present")
            .rect;

        // A larger window_gap shifts the leftmost on-screen window to the right.
        let new_padding = Padding {
            window_gap: 40,
            up: 0,
            down: 0,
        };
        let returned = engine.reconfigure(960, 320, 100, 100, new_padding, 4);

        // Cached padding reflects the new gap.
        assert_eq!(engine.padding().window_gap, 40);
        // The returned and cached ActualLayouts agree and still hold all windows.
        assert_eq!(returned.entries.len(), 3);
        assert_eq!(engine.actual_layout().entries.len(), 3);
        // The first window shifted right due to the larger left gap.
        let first_after = engine
            .actual_layout()
            .find(WindowId(1))
            .expect("window 1 present")
            .rect;
        assert!(
            first_after.x > first_before.x,
            "larger window_gap should shift the leftmost window right: {} -> {}",
            first_before.x,
            first_after.x
        );
    }

    /// Contract: `reconfigure` MUST recompute the derived ladder bounds
    /// (`max_n`, `abs_max_width`) from the new `window_gap`, not keep stale
    /// values from `new`. Both bounds are documented in `build_mutation_config`.
    #[test]
    fn reconfigure_recomputes_derived_max_n_and_abs_max_width_from_new_gap() {
        // Arrange: monitor 1920 wide, base column 400, gap 10.
        //   abs_max_width = 1920 - 2*10 = 1900
        //   shift         = 400 + 10   = 410
        //   max_n         = (1900 - 400) / 410 = 3
        let mut engine = ScrollingSpace::new(
            test_monitor(),
            400,
            100,
            100,
            100,
            Padding {
                window_gap: 10,
                up: 0,
                down: 0,
            },
            4,
        );
        assert_eq!(engine.config.abs_max_width, 1900);
        assert_eq!(engine.config.max_n, 3);

        // Act: widen the gap to 100 — fewer slots fit, so the ladder must shrink.
        //   abs_max_width = 1920 - 2*100 = 1720
        //   shift         = 400 + 100    = 500
        //   max_n         = (1720 - 400) / 500 = 2
        engine.reconfigure(
            400,
            100,
            100,
            100,
            Padding {
                window_gap: 100,
                up: 0,
                down: 0,
            },
            4,
        );

        // Assert: both derived bounds reflect the NEW gap, not the original.
        assert_eq!(
            engine.config.abs_max_width, 1720,
            "abs_max_width must recompute from new gap"
        );
        assert_eq!(engine.config.max_n, 2, "max_n must recompute from new gap");
        assert_eq!(engine.padding().window_gap, 100);
    }

    /// Edge case for `build_mutation_config`: when the base column is wider than
    /// `abs_max_width`, the ladder is degenerate and `max_n` clamps to 0. Tested
    /// via `new` (which delegates to the shared `build_mutation_config`).
    #[test]
    fn build_mutation_config_clamps_max_n_to_zero_when_column_wider_than_monitor() {
        // Arrange: monitor 1920 wide, base column 2000 (wider than the monitor
        // even before subtracting gaps). gap = 4.
        //   abs_max_width = 1920 - 2*4 = 1912
        //   cw            = 2000
        //   abs_max_width > cw is false → max_n = 0.
        let engine = ScrollingSpace::new(test_monitor(), 2000, 100, 100, 100, test_padding(), 4);
        assert_eq!(engine.config.abs_max_width, 1912);
        assert_eq!(
            engine.config.max_n, 0,
            "max_n must be 0 when column is wider than abs_max_width"
        );
    }

    /// Edge case for `build_mutation_config`: when `column_shift` is 0 (both
    /// `column_width` and `window_gap` are 0), the degenerate-shift guard forces
    /// `max_n = 0` to avoid a divide-by-zero in the slot-ladder derivation.
    #[test]
    fn build_mutation_config_clamps_max_n_to_zero_when_shift_is_zero() {
        // Arrange: column_width 0 and gap 0 → column_shift = 0.
        let gapless = Padding {
            window_gap: 0,
            up: 0,
            down: 0,
        };
        let engine = ScrollingSpace::new(test_monitor(), 0, 0, 100, 100, gapless, 4);

        // Assert: degenerate shift is guarded; abs_max_width is still well-defined.
        assert_eq!(
            engine.config.max_n, 0,
            "max_n must be 0 when column_shift is 0 (avoid divide-by-zero)"
        );
        assert_eq!(
            engine.config.abs_max_width, 1920,
            "abs_max_width = monitor_width - 2*gap = 1920"
        );
    }

    #[test]
    fn engine_focus_moves() {
        let mut engine = engine_with_three_columns();
        let (new_focus, _diff) = engine
            .focus(WindowId(1), Direction::Right)
            .expect("focus right");
        assert_eq!(new_focus, WindowId(2));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    #[test]
    fn engine_swap_columns() {
        let mut engine = engine_with_three_columns();
        let diff = engine
            .swap_column(WindowId(1), Direction::Right)
            .expect("swap");
        assert_eq!(
            diff.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        );
        assert_eq!(
            diff.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        );
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_expand_column() {
        let mut engine = engine_with_three_columns();
        let diff = engine.expand_column(WindowId(1)).expect("expand");
        // 960px (base) + column_shift(964) = 1924px → clamped to abs_max 1912px
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1912);
        assert_eq!(diff.virtual_layout.columns[1].width_px, 960); // unchanged
    }

    #[test]
    fn engine_scroll_right_then_left() {
        let mut engine = engine_with_three_columns();
        let diff_r = engine.scroll_right().expect("scroll right");
        assert!(diff_r.virtual_layout.viewport_offset > 0);

        let diff_l = engine.scroll_left().expect("scroll left");
        assert_eq!(diff_l.virtual_layout.viewport_offset, 0);
    }

    #[test]
    fn engine_remove_window_updates_focus() {
        let mut engine = engine_with_three_columns();
        // Layout: [W1][W2][W3], focus on W2 (col 1).
        engine.set_focus(WindowId(2));
        let _ = engine.remove_window(WindowId(2));
        // Focus should fall back to the LEFT column's window (W1).
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
    }

    #[test]
    fn engine_monocle_toggle() {
        let mut engine = engine_with_three_columns();
        let diff_on = engine.toggle_monocle(WindowId(1)).expect("monocle on");
        assert_eq!(diff_on.virtual_layout.columns[0].width_px, 1912);

        let diff_off = engine.toggle_monocle(WindowId(1)).expect("monocle off");
        assert_eq!(diff_off.virtual_layout.columns[0].width_px, 960);
    }

    #[test]
    fn engine_add_remove_roundtrip() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        assert_eq!(engine.virtual_layout().window_count(), 2);

        let _ = engine.remove_window(WindowId(2));
        assert_eq!(engine.virtual_layout().window_count(), 1);
        assert_eq!(engine.virtual_layout().columns.len(), 1);

        let _ = engine.remove_window(WindowId(1));
        assert_eq!(engine.virtual_layout().window_count(), 0);
        assert!(engine.virtual_layout().columns.is_empty());
    }

    // --- Integration: Engine lifecycle tests ---

    #[test]
    fn engine_full_lifecycle() {
        // Positive: empty → add 3 → mutate → remove all → verify state at each step
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);

        // Step 1: Add windows
        let d1 = engine.add_window(WindowId(1));
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
        assert_eq!(d1.actual_layout.entries.len(), 1);

        let d2 = engine.add_window(WindowId(2));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
        assert_eq!(d2.actual_layout.entries.len(), 2);

        let d3 = engine.add_window(WindowId(3));
        assert_eq!(engine.virtual_layout().columns.len(), 3);
        assert_eq!(d3.actual_layout.entries.len(), 3);

        // Step 2: Mutate — swap columns 0 and 1
        engine.set_focus(WindowId(1));
        let d_swap = engine
            .swap_column(WindowId(1), Direction::Right)
            .expect("swap");
        assert_eq!(
            d_swap.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        );
        assert_eq!(
            d_swap.virtual_layout.columns[1].rows[0].window_id,
            WindowId(1)
        );
        assert!(!d_swap.actual_layout.entries.is_empty());

        // Step 3: Mutate — expand column (independent, no compensation)
        engine.set_focus(WindowId(1));
        let d_expand = engine.expand_column(WindowId(1)).expect("expand");
        // 960px + column_shift(964) = 1924 → clamped to abs_max 1912px, other columns unchanged
        assert_eq!(d_expand.virtual_layout.columns[1].width_px, 1912);
        assert_eq!(d_expand.virtual_layout.columns[0].width_px, 960);

        // Step 4: Remove windows one by one
        let _ = engine.remove_window(WindowId(1));
        assert_eq!(engine.virtual_layout().columns.len(), 2);

        let _ = engine.remove_window(WindowId(2));
        assert_eq!(engine.virtual_layout().columns.len(), 1);

        let _ = engine.remove_window(WindowId(3));
        assert_eq!(engine.virtual_layout().columns.len(), 0);
        assert_eq!(engine.virtual_layout().window_count(), 0);
    }

    #[test]
    fn engine_focus_triggers_viewport_scroll() {
        // Positive: focus into off-screen column triggers viewport scroll
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4)); // 4 columns × 960px each = 2× viewport width
        engine.set_focus(WindowId(1));

        // Focus right twice: col1→col2 (visible), col2→col3 (triggers scroll)
        let f1 = engine
            .focus(WindowId(1), Direction::Right)
            .expect("focus right 1");
        assert_eq!(f1.0, WindowId(2));
        // After first focus (visible), viewport_offset unchanged
        let offset_after_first = engine.virtual_layout().viewport_offset;

        let f2 = engine
            .focus(WindowId(2), Direction::Right)
            .expect("focus right 2");
        assert_eq!(f2.0, WindowId(3));
        assert!(
            engine.virtual_layout().viewport_offset > offset_after_first,
            "viewport should have scrolled to show col 3 (was {offset_after_first}, now {})",
            engine.virtual_layout().viewport_offset
        );
    }

    #[test]
    fn engine_single_window_all_operations() {
        // Positive: single window — swap, expand/shrink return None appropriately
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));

        // Swap column left → None (no column to left)
        assert!(engine.swap_column(WindowId(1), Direction::Left).is_none());
        // Swap column right → None (no column to right)
        assert!(engine.swap_column(WindowId(1), Direction::Right).is_none());
        // Swap column up → None (vertical direction invalid for column swap)
        assert!(engine.swap_column(WindowId(1), Direction::Up).is_none());
        // Swap column down → None (vertical direction invalid for column swap)
        assert!(engine.swap_column(WindowId(1), Direction::Down).is_none());

        // Expand/shrink — single column at base width (960px)
        // Expand: 960 + column_shift(964) = 1924 → clamped to abs_max 1912px
        let diff = engine.expand_column(WindowId(1)).expect("expand single");
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1912);
        // Shrink: abs_max(1912) → slot_max(960px)

        let diff2 = engine.shrink_column(WindowId(1)).expect("shrink single");
        assert_eq!(diff2.virtual_layout.columns[0].width_px, 960);

        // Focus vertical — only one row
        assert!(engine.focus(WindowId(1), Direction::Up).is_none());
        assert!(engine.focus(WindowId(1), Direction::Down).is_none());

        // Monocle still works (single column)
        let diff = engine.toggle_monocle(WindowId(1)).expect("monocle");
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1912);
    }

    #[test]
    fn engine_empty_operations_return_none() {
        // Negative: all operations on empty engine return None or produce empty diffs
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);

        assert!(engine.scroll_left().is_none());
        assert!(engine.scroll_right().is_none());
        assert!(engine.focus(WindowId(1), Direction::Right).is_none());
        assert!(engine.swap_column(WindowId(1), Direction::Right).is_none());
        assert!(engine.expand_column(WindowId(1)).is_none());
        assert!(engine.shrink_column(WindowId(1)).is_none());
        assert!(engine.toggle_monocle(WindowId(1)).is_none());
    }

    #[test]
    fn engine_add_window_to_focused_column() {
        // Positive: add window as row to focused column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        let diff = engine
            .add_window_to_focused_column(WindowId(2))
            .expect("add to focused col");
        assert_eq!(engine.virtual_layout().columns.len(), 1); // still one column
        assert_eq!(engine.virtual_layout().columns[0].rows.len(), 2);
        // Both windows' positions changed (existing window shrunk, new window appeared)
        assert_eq!(diff.actual_layout.entries.len(), 2);
    }

    #[test]
    fn engine_add_to_focused_column_no_focus_returns_none() {
        // Negative: no focus → can't add to focused column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        assert!(engine.add_window_to_focused_column(WindowId(1)).is_none());
    }

    // --- Engine insert_window tests ---

    #[test]
    fn engine_insert_window_into_empty_layout() {
        // Positive: first window in empty layout → sole column, focused.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.insert_window(WindowId(1));
        assert_eq!(engine.virtual_layout().columns.len(), 1);
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
    }

    #[test]
    fn engine_insert_window_after_focused_shifts_right() {
        // Positive: insert between focused col 0 and col 1.
        // Layout: [W1][W2][W3], focus W1. insert W4 → [W1][W4][W2][W3].
        let mut engine = engine_with_three_columns();
        // engine_with_three_columns sets focus to W1 (col 0)
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));

        engine.insert_window(WindowId(4));
        assert_eq!(engine.virtual_layout().columns.len(), 4);
        assert_eq!(
            engine.virtual_layout().columns[0].rows[0].window_id,
            WindowId(1)
        );
        assert_eq!(
            engine.virtual_layout().columns[1].rows[0].window_id,
            WindowId(4)
        ); // new
        assert_eq!(
            engine.virtual_layout().columns[2].rows[0].window_id,
            WindowId(2)
        ); // shifted
        assert_eq!(
            engine.virtual_layout().columns[3].rows[0].window_id,
            WindowId(3)
        ); // shifted
        assert_eq!(engine.last_focused_window(), Some(WindowId(4))); // focus moved to new
    }

    #[test]
    fn engine_insert_window_after_last_column() {
        // Positive: focused on last column → insert at end.
        let mut engine = engine_with_three_columns();
        engine.set_focus(WindowId(3)); // last column

        let _ = engine.insert_window(WindowId(4));
        assert_eq!(engine.virtual_layout().columns.len(), 4);
        assert_eq!(
            engine.virtual_layout().columns[3].rows[0].window_id,
            WindowId(4)
        );
        assert_eq!(engine.last_focused_window(), Some(WindowId(4)));
    }

    #[test]
    fn engine_insert_window_after_middle_column() {
        // Positive: focused on col 1 (middle) → insert at index 2.
        let mut engine = engine_with_three_columns();
        engine.set_focus(WindowId(2));

        let _ = engine.insert_window(WindowId(4));
        assert_eq!(engine.virtual_layout().columns.len(), 4);
        assert_eq!(
            engine.virtual_layout().columns[0].rows[0].window_id,
            WindowId(1)
        ); // unchanged
        assert_eq!(
            engine.virtual_layout().columns[1].rows[0].window_id,
            WindowId(2)
        ); // unchanged
        assert_eq!(
            engine.virtual_layout().columns[2].rows[0].window_id,
            WindowId(4)
        ); // new
        assert_eq!(
            engine.virtual_layout().columns[3].rows[0].window_id,
            WindowId(3)
        ); // shifted
    }

    #[test]
    fn engine_insert_window_scrolls_when_new_column_offscreen() {
        // Positive: many columns, insert creates a column off-screen right →
        // viewport scrolls to reveal it.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1)); // focus leftmost (col 0)

        engine.insert_window(WindowId(4));
        // New column at index 1. With 4 columns × 960px > 1920px viewport,
        // ensure_column_visible keeps the new column visible.
        assert_eq!(engine.last_focused_window(), Some(WindowId(4)));
    }

    #[test]
    fn engine_insert_window_after_remove_falls_back() {
        // Negative/edge: after removing the focused window, focus falls back
        // to the first column. Inserting then uses that focus as the anchor.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        // Remove focused (W2) → focus falls back to W1 (first column).
        let _ = engine.remove_window(WindowId(2));
        // Insert W3 after focused (W1) → W3 at col 1.
        engine.insert_window(WindowId(3));
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
    }

    #[test]
    fn engine_monocle_then_add_window() {
        // Positive: monocle on, add window (new column), toggle off on same column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));

        let d_on = engine.toggle_monocle(WindowId(1)).expect("monocle on");
        assert_eq!(d_on.virtual_layout.columns[0].width_px, 1912);

        // Add window → new column, focus moves to new window
        engine.add_window(WindowId(2));
        assert_eq!(engine.virtual_layout().columns.len(), 2);

        // Focus back to column 0 and toggle monocle off
        engine.set_focus(WindowId(1));
        let d_off = engine.toggle_monocle(WindowId(1)).expect("monocle off");
        assert_eq!(d_off.virtual_layout.columns[0].width_px, 960);
    }

    #[test]
    fn engine_expand_shrink_produces_pixel_diffs() {
        // Positive: expand → actual layout has different pixel sizes
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));

        // Focus WindowId(1) in column 0, expand (960 + column_shift(964) → abs_max 1912px)
        engine.set_focus(WindowId(1));
        let diff = engine.expand_column(WindowId(1)).expect("expand");
        // The layout must have entries (windows with new pixel positions)
        assert!(
            !diff.actual_layout.entries.is_empty(),
            "expand should produce layout entries"
        );
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1912);
        assert_eq!(diff.virtual_layout.columns[1].width_px, 960); // unchanged
    }

    #[test]
    fn engine_swap_shifts_camera_when_target_offscreen() {
        // Positive: swap with adjacent column that is off-screen shifts the camera
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.set_focus(WindowId(1));

        // Focus right twice to scroll viewport, then focus back to window 2
        // so column 0 (window 1) is off-screen left
        let _ = engine.focus(WindowId(1), Direction::Right).expect("f1");
        let _ = engine.focus(WindowId(2), Direction::Right).expect("f2");
        assert!(engine.virtual_layout().viewport_offset > 0);

        // Focus on window at the left edge of viewport, then swap left
        // The column to the left (window 1) is off-screen
        engine.set_focus(WindowId(2));
        let prev_offset = engine.virtual_layout().viewport_offset;
        let diff = engine
            .swap_column(WindowId(2), Direction::Left)
            .expect("swap left");
        assert!(!diff.actual_layout.entries.is_empty());
        // Camera should shift to make the swapped column visible
        assert!(
            diff.virtual_layout.viewport_offset < prev_offset,
            "viewport should have scrolled left to reveal swapped column"
        );
    }

    #[test]
    fn engine_scroll_right_at_boundary() {
        // Negative: can't scroll past rightmost column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1)); // single 960px column

        assert!(engine.scroll_right().is_none());
    }

    #[test]
    fn engine_set_column_width() {
        // Positive: explicit column width setting
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));

        // Focus WindowId(1) in column 0, set width to 1440px (free-form, independent)
        engine.set_focus(WindowId(1));
        let diff = engine
            .set_column_width(WindowId(1), 1440)
            .expect("set width");
        assert_eq!(diff.virtual_layout.columns[0].width_px, 1440);
        // Neighbor is unchanged (no compensation)
        assert_eq!(diff.virtual_layout.columns[1].width_px, 960);
    }

    #[test]
    fn engine_remove_focused_window_focus_falls_back() {
        // Positive: removing a focused middle window falls back to the LEFT column.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(2));

        let _ = engine.remove_window(WindowId(2));
        // Focus falls back to the left column's window (W1).
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
    }

    #[test]
    fn engine_remove_focused_leftmost_falls_to_right() {
        // Removing the focused leftmost window has no left column, so focus
        // falls back to the RIGHT column's window.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));

        let _ = engine.remove_window(WindowId(1));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    #[test]
    fn engine_remove_focused_rightmost_falls_to_left() {
        // Removing the focused rightmost window falls back to the LEFT column.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(3));

        let _ = engine.remove_window(WindowId(3));
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    #[test]
    fn engine_remove_only_window_clears_focus() {
        // Removing the only window leaves focus as None.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));

        let _ = engine.remove_window(WindowId(1));
        assert_eq!(engine.last_focused_window(), None);
        assert!(engine.virtual_layout().columns.is_empty());
    }

    #[test]
    fn engine_remove_nonfocused_keeps_focus() {
        // Removing a non-focused window must not change focus.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));

        let _ = engine.remove_window(WindowId(2));
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
    }

    #[test]
    fn engine_remove_nonexistent_window_is_noop() {
        // Negative: removing a window that doesn't exist in the layout should
        // not crash, not change focus, and not alter the layout.
        let mut engine = engine_with_three_columns();
        engine.set_focus(WindowId(1));

        let diff = engine.remove_window(WindowId(99));
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(1)),
            "focus must not change"
        );
        assert_eq!(
            diff.virtual_layout.columns.len(),
            3,
            "column count must not change"
        );
        assert_eq!(
            diff.virtual_layout.window_count(),
            3,
            "window count must not change"
        );
    }

    #[test]
    fn engine_remove_from_multirow_column_preserves_column() {
        // Positive: removing a window from a multi-row column removes only the
        // row — the column persists with the remaining window.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        // Layout: [W1, W2] in one column. Focus on W2.

        let _ = engine.remove_window(WindowId(2));
        // Column should still exist with W1.
        assert_eq!(engine.virtual_layout().columns.len(), 1);
        assert_eq!(engine.virtual_layout().columns[0].rows.len(), 1);
        assert_eq!(
            engine.virtual_layout().columns[0].rows[0].window_id,
            WindowId(1)
        );
        // Focus was on W2 (removed). next_available_window at (col 0, row 1):
        // no row below, so the same-column sibling above (W1) is chosen.
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(1)),
            "focus should move to the same-column sibling (W1) after removing \
             the focused bottom window from a single-column multi-row layout"
        );
    }

    #[test]
    fn engine_remove_nonfocused_left_of_focus_shifts_column_index() {
        // Positive: removing a non-focused window from a column to the LEFT of
        // the focused window shifts the focused window's column index left by 1.
        // The viewport should still show the focused window correctly.
        // Layout: [W1][W2][W3], focus W3 (col 2). Remove W1 (col 0).
        // After removal: [W2][W3], W3 is now at col 1.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(3));

        let diff = engine.remove_window(WindowId(1));
        // Focus should be unchanged (W3 was not removed).
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
        // Layout should have 2 columns.
        assert_eq!(diff.virtual_layout.columns.len(), 2);
        // W3 should be in column index 1 (shifted from 2).
        assert_eq!(
            diff.virtual_layout.columns[1].rows[0].window_id,
            WindowId(3)
        );
    }

    #[test]
    fn engine_remove_focused_scrolls_to_successor() {
        // Positive: removing the focused window when the successor's column is
        // off-screen should scroll the viewport to reveal the successor.
        // Setup: 4 columns, viewport scrolled to show columns 2–3, focus on col 3.
        // Remove col 3's window → successor is col 2 (left) which is already
        // visible, so viewport doesn't scroll. Then test a case where successor
        // IS off-screen: focus col 3, scroll viewport so col 0 is visible only,
        // then remove col 3 → successor col 2 is off-screen → viewport adjusts.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.set_focus(WindowId(4));

        // Scroll viewport so only the first column is visible (viewport_offset = 0).
        // All 4 columns span ~3860px, viewport is 1920px. Col 0 is at [4, 964],
        // col 1 at [968, 1928], col 2 at [1932, 2892], col 3 at [2896, 3860].
        // Only col 0 is fully visible from offset 0.

        let prev_offset = engine.virtual_layout().viewport_offset;
        let diff = engine.remove_window(WindowId(4));
        // Focus should fall to W3 (left neighbour).
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
        // W3 is now in column index 2 (columns shifted after removal).
        // W3's column is at canvas position ~1932, which is off-screen from
        // offset 0 (viewport right = 1920). ensure_column_visible should scroll.
        assert_eq!(
            diff.virtual_layout.columns.len(),
            3,
            "should have 3 columns after removing W4"
        );
        // The viewport_offset should have changed (or at minimum, the diff
        // should contain moves if any viewport shift happened).
        // W3 at col 2, canvas ~1932. ensure_column_visible should shift viewport.
        // The exact offset depends on the quantization math, but it should be
        // non-zero because col 2 is off-screen at offset 0.
        assert_ne!(
            diff.virtual_layout.viewport_offset, prev_offset,
            "viewport should scroll to reveal successor column that is off-screen"
        );
    }

    #[test]
    fn engine_focus_returns_none_diff_when_no_scroll() {
        // Positive: focus within fully visible columns → diff is None (no viewport change).
        // The (WindowId, Option<AppliedLayout>) tuple must have None in the diff slot.
        // Use a wider monitor (3840px) so all 3 columns fit in the viewport without scroll.
        let monitor = MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 3840,
            height: 1080,
        });
        let mut engine = ScrollingSpace::new(monitor, 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(1));

        let result = engine
            .focus(WindowId(1), Direction::Right)
            .expect("focus right");
        assert_eq!(result.0, WindowId(2));
        // Column 2 is visible within the 3840px viewport → no scroll needed → diff is None
        assert!(
            result.1.is_none(),
            "focus within visible area should not produce a AppliedLayout, got Some"
        );
    }

    #[test]
    fn engine_focus_returns_some_diff_when_viewport_scrolls() {
        // Positive: focus into off-screen column → diff is Some with non-empty moves.
        // This is the core Fix 1 behavior: the caller receives the AppliedLayout to animate.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4)); // 4 columns × 960px each = 2× viewport width
        engine.set_focus(WindowId(1));

        // Focus right twice: first stays visible, second triggers scroll
        let _ = engine
            .focus(WindowId(1), Direction::Right)
            .expect("focus right 1");
        let result = engine
            .focus(WindowId(2), Direction::Right)
            .expect("focus right 2");

        assert_eq!(result.0, WindowId(3));
        // Viewport scrolled → diff must be Some
        assert!(
            result.1.is_some(),
            "focus into off-screen column must produce Some(AppliedLayout)"
        );
        let diff = result.1.unwrap();
        // The diff must contain moves because windows shifted on screen
        assert!(
            !diff.actual_layout.entries.is_empty(),
            "AppliedLayout from viewport scroll must contain animation moves"
        );
    }

    #[test]
    fn engine_focus_vertical_never_produces_diff() {
        // Positive: vertical focus (Up/Down) never scrolls viewport, so diff is always None.
        // Even when the window has rows, vertical focus just changes the focused WindowId.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        engine.set_focus(WindowId(1));

        let result = engine
            .focus(WindowId(1), Direction::Down)
            .expect("focus down");
        assert_eq!(result.0, WindowId(2));
        assert!(
            result.1.is_none(),
            "vertical focus should never produce a AppliedLayout"
        );
    }

    #[test]
    fn engine_focus_no_change_in_layout_when_visible() {
        // Positive: focus within fully visible area → no viewport scroll needed.
        // With a single multi-row column, focus has nowhere to go horizontally,
        // but vertical focus doesn't trigger scroll. This test verifies vertical focus
        // doesn't change the viewport offset.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        engine.set_focus(WindowId(1));

        let prev_offset = engine.virtual_layout().viewport_offset;
        let _ = engine
            .focus(WindowId(1), Direction::Down)
            .expect("focus down");
        // Vertical focus within a fully visible column → no scroll
        assert_eq!(engine.virtual_layout().viewport_offset, prev_offset);
    }

    // --- Engine swap_window tests ---

    #[test]
    fn engine_swap_window_cross_column() {
        // Positive: engine swap_window moves individual windows between columns
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(5));
        engine.add_window(WindowId(2));
        engine.set_focus(WindowId(1));

        // Layout: [W1, W5] [W2]. swap_window Right on W1 → [W2, W5] [W1]
        let diff = engine
            .swap_window(WindowId(1), Direction::Right)
            .expect("swap window");
        assert_eq!(
            diff.virtual_layout.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2), WindowId(5)]
        );
        assert_eq!(
            diff.virtual_layout.columns[1]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(1)]
        );
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_swap_window_same_column_vertical() {
        // Positive: engine swap_window Up/Down within column
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        engine.set_focus(WindowId(2));

        let diff = engine
            .swap_window(WindowId(2), Direction::Up)
            .expect("swap window up");
        assert_eq!(
            diff.virtual_layout.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2), WindowId(1)]
        );
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_swap_window_none_when_no_focus() {
        // Negative: engine swap_window without focus → None
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        // Focus is already on WindowId(1) from add_window. Clear it.
        // We can't clear focus directly, so test with empty engine instead.
        let mut empty_engine =
            ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        assert!(
            empty_engine
                .swap_window(WindowId(1), Direction::Right)
                .is_none()
        );
        assert!(
            empty_engine
                .swap_window(WindowId(1), Direction::Up)
                .is_none()
        );
        assert!(
            empty_engine
                .swap_window(WindowId(1), Direction::Down)
                .is_none()
        );
    }

    #[test]
    fn engine_swap_window_none_at_boundary() {
        // Negative: engine swap_window at layout boundary → None
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.set_focus(WindowId(1));

        assert!(engine.swap_window(WindowId(1), Direction::Left).is_none());
        assert!(engine.swap_window(WindowId(1), Direction::Right).is_none());
        assert!(engine.swap_window(WindowId(1), Direction::Up).is_none());
        assert!(engine.swap_window(WindowId(1), Direction::Down).is_none());
    }

    // --- Engine merge_column tests ---

    #[test]
    fn engine_merge_column_right_combines_columns() {
        // Positive: merge W1 right into W2's column. Layout: [W1][W2] → [W2, W1].
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.set_focus(WindowId(1));

        let diff = engine
            .merge_column(WindowId(1), Direction::Right)
            .expect("merge right");
        assert_eq!(diff.virtual_layout.columns.len(), 1);
        assert_eq!(
            diff.virtual_layout.columns[0]
                .rows
                .iter()
                .map(|r| r.window_id)
                .collect::<Vec<_>>(),
            vec![WindowId(2), WindowId(1)]
        );
        // Two equal rows: (1080 - 3*4) / 2 = 534 each.
        assert_eq!(diff.virtual_layout.columns[0].rows[0].height, 534);
        assert_eq!(diff.virtual_layout.columns[0].rows[1].height, 534);
        // Focus follows the merged window.
        assert_eq!(engine.last_focused_window, Some(WindowId(1)));
        // ActualLayout must be projected.
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_merge_column_none_at_boundary() {
        // Negative: no neighbour in the requested direction.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.set_focus(WindowId(1));

        assert!(engine.merge_column(WindowId(1), Direction::Left).is_none());
        assert!(engine.merge_column(WindowId(1), Direction::Right).is_none());
        assert!(engine.merge_column(WindowId(1), Direction::Up).is_none());
        assert!(engine.merge_column(WindowId(1), Direction::Down).is_none());
    }

    // --- Engine promote_window tests ---

    #[test]
    fn engine_promote_right_splits_column() {
        // Positive: promote W2 out of [W1, W2] right → [W1][W2].
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window_to_focused_column(WindowId(2));
        engine.set_focus(WindowId(2));

        let diff = engine
            .promote_window(WindowId(2), Direction::Right)
            .expect("promote right");
        assert_eq!(diff.virtual_layout.columns.len(), 2);
        assert_eq!(
            diff.virtual_layout.columns[0].rows[0].window_id,
            WindowId(1)
        );
        assert_eq!(
            diff.virtual_layout.columns[1].rows[0].window_id,
            WindowId(2)
        );
        // Both single-row columns get the n=1 height (1080 - 2*4) / 1 = 1072.
        assert_eq!(diff.virtual_layout.columns[0].rows[0].height, 1072);
        assert_eq!(diff.virtual_layout.columns[1].rows[0].height, 1072);
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_promote_none_for_single_row_column() {
        // Negative: already alone in column → no-op per spec.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.set_focus(WindowId(1));

        assert!(
            engine
                .promote_window(WindowId(1), Direction::Left)
                .is_none()
        );
        assert!(
            engine
                .promote_window(WindowId(1), Direction::Right)
                .is_none()
        );
        assert!(engine.promote_window(WindowId(1), Direction::Up).is_none());
        assert!(
            engine
                .promote_window(WindowId(1), Direction::Down)
                .is_none()
        );
    }

    // --- Engine initialize_windows tests ---

    #[test]
    fn engine_initialize_windows_empty() {
        // Positive: empty vec → empty layout, no focus
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let diff = engine.initialize_windows(vec![], None, None);
        assert!(diff.virtual_layout.columns.is_empty());
        assert!(engine.last_focused_window().is_none());
    }

    #[test]
    fn engine_initialize_windows_single() {
        // Positive: single window → single column, focused
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let diff = engine.initialize_windows(vec![WindowId(1)], None, None);
        assert_eq!(diff.virtual_layout.columns.len(), 1);
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
        // Should produce moves (new window appeared)
        assert!(!diff.actual_layout.entries.is_empty());
    }

    #[test]
    fn engine_initialize_windows_multiple() {
        // Positive: multiple windows → multiple columns, focus on last
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let diff =
            engine.initialize_windows(vec![WindowId(10), WindowId(20), WindowId(30)], None, None);
        assert_eq!(diff.virtual_layout.columns.len(), 3);
        assert_eq!(
            diff.virtual_layout.columns[0].rows[0].window_id,
            WindowId(10)
        );
        assert_eq!(
            diff.virtual_layout.columns[1].rows[0].window_id,
            WindowId(20)
        );
        assert_eq!(
            diff.virtual_layout.columns[2].rows[0].window_id,
            WindowId(30)
        );
        assert_eq!(engine.last_focused_window(), Some(WindowId(30))); // last window
    }

    #[test]
    fn engine_initialize_windows_focus_first_column() {
        // Positive: focus_col_idx=Some(0) → focused should be the first window,
        // NOT the last window. This tests the viewport-init fix: the engine
        // now respects the focus column from the foreground window lookup
        // instead of blindly picking the last column.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let diff = engine.initialize_windows(
            vec![WindowId(10), WindowId(20), WindowId(30)],
            Some(0),
            None,
        );
        assert_eq!(diff.virtual_layout.columns.len(), 3);
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(10)),
            "focus_col_idx=0 should focus the first window, not the last"
        );
    }

    #[test]
    fn engine_initialize_windows_focus_middle_column() {
        // Positive: focus_col_idx=Some(1) → focused should be the second window.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.initialize_windows(
            vec![WindowId(10), WindowId(20), WindowId(30)],
            Some(1),
            None,
        );
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(20)),
            "focus_col_idx=1 should focus the second window"
        );
    }

    #[test]
    fn engine_initialize_windows_none_focus_falls_back_to_last() {
        // Positive: focus_col_idx=None → fallback to last window (existing behavior).
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.initialize_windows(vec![WindowId(10), WindowId(20), WindowId(30)], None, None);
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(30)),
            "focus_col_idx=None should fall back to last window"
        );
    }

    #[test]
    fn engine_initialize_windows_scroll_mode_focus_col() {
        // Positive: 4 cols, columns_per_screen=2 → scroll mode.
        // Focus on column 2 → viewport_offset should be non-zero,
        // and focused should be the window at column 2.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 2);
        let diff = engine.initialize_windows(
            vec![WindowId(1), WindowId(2), WindowId(3), WindowId(4)],
            Some(2),
            None,
        );
        assert_eq!(diff.virtual_layout.columns.len(), 4);
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(3)),
            "focus_col_idx=2 should focus WindowId(3)"
        );
        assert_ne!(
            diff.virtual_layout.viewport_offset, 0,
            "4 cols > columns_per_screen=2 → viewport should scroll to focus col"
        );
    }

    // -------------------------------------------------------------------
    // Bug 1 regression: AppliedLayout.actual_layout.entries must contain
    // ALL windows (not just "moved" ones). This is the core invariant
    // that prevents the swap-column animation stranding bug.
    // -------------------------------------------------------------------

    #[test]
    fn applied_layout_entries_match_window_count_after_swap() {
        // Positive: after a swap_column mutation, actual_layout.entries must
        // contain every window — not just the two columns that swapped.
        // Regression test for: rapid swap-column during in-flight animation
        // stranded mid-flight windows because the old diff only included
        // windows whose target changed.
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count(); // 3

        let result = engine
            .swap_column(WindowId(1), Direction::Right)
            .expect("swap");
        assert_eq!(
            result.actual_layout.entries.len(),
            total_windows,
            "AppliedLayout must contain ALL windows after swap_column, not just the swapped ones"
        );

        // Verify each specific window is present
        for wid in [WindowId(1), WindowId(2), WindowId(3)] {
            assert!(
                result
                    .actual_layout
                    .entries
                    .iter()
                    .any(|e| e.window_id == wid),
                "AppliedLayout must contain window after swap_column (id={:?})",
                wid
            );
        }
    }

    #[test]
    fn applied_layout_entries_match_window_count_after_expand() {
        // Positive: after expand_column, entries must still contain ALL windows
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count();

        let result = engine.expand_column(WindowId(1)).expect("expand");
        assert_eq!(
            result.actual_layout.entries.len(),
            total_windows,
            "AppliedLayout must contain ALL windows after expand_column"
        );
    }

    #[test]
    fn rapid_consecutive_mutations_both_return_full_layouts() {
        // Positive: simulate rapid mutations (A then B) without animation settling.
        // Both AppliedLayouts must contain all windows. This is the exact scenario
        // that caused Bug 1: a second swap while the first swap's animation was
        // still in-flight dropped mid-flight windows from the diff.
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count(); // 3

        // Mutation A: swap columns 0 ↔ 1
        let result_a = engine
            .swap_column(WindowId(1), Direction::Right)
            .expect("swap A");
        assert_eq!(
            result_a.actual_layout.entries.len(),
            total_windows,
            "First mutation (swap A) must contain ALL windows"
        );

        // Mutation B: immediately swap again (same direction, different result
        // because focus is still on WindowId(1) which is now in column 1).
        // [W2, W1, W3] → swap W1 right → [W2, W3, W1]
        let result_b = engine
            .swap_column(WindowId(1), Direction::Right)
            .expect("swap B");
        assert_eq!(
            result_b.actual_layout.entries.len(),
            total_windows,
            "Second mutation (swap B) must contain ALL windows — \
             this was Bug 1: mid-flight windows were stranded because the old \
             diff only included windows whose target changed"
        );

        // Verify the second swap actually moved W1 further right
        assert_eq!(
            result_b.virtual_layout.columns[0].rows[0].window_id,
            WindowId(2)
        );
        assert_eq!(
            result_b.virtual_layout.columns[1].rows[0].window_id,
            WindowId(3)
        );
        assert_eq!(
            result_b.virtual_layout.columns[2].rows[0].window_id,
            WindowId(1)
        );
    }

    #[test]
    fn rapid_swap_then_expand_both_return_full_layouts() {
        // Positive: rapid heterogeneous mutations (swap then expand) must both
        // return full layouts. Tests that the invariant holds across different
        // mutation types, not just consecutive same-type mutations.
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count();

        // Mutation A: swap column
        let result_a = engine
            .swap_column(WindowId(1), Direction::Right)
            .expect("swap");
        assert_eq!(result_a.actual_layout.entries.len(), total_windows);

        // Mutation B: expand column (different mutation type, still rapid)
        let result_b = engine.expand_column(WindowId(1)).expect("expand");
        assert_eq!(
            result_b.actual_layout.entries.len(),
            total_windows,
            "Expand after swap must still return ALL windows in actual_layout.entries"
        );
    }

    #[test]
    fn applied_layout_includes_parked_windows() {
        // Positive: windows that are off-screen (parked) must still appear in
        // actual_layout.entries. The projection parks them off-screen with
        // deterministic rects — they must not be omitted.
        // Use 5 columns: only ~2 visible at a time on a 1920px monitor.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.add_window(WindowId(5));
        let total_windows = engine.virtual_layout().window_count(); // 5

        // Viewport at offset 0: cols 0-1 visible, cols 2-4 parked right.
        let layout = engine.actual_layout();
        assert_eq!(
            layout.entries.len(),
            total_windows,
            "actual_layout must include parked (off-screen) windows, not just visible ones"
        );

        // Verify ALL window IDs are present, including parked ones
        for wid in [
            WindowId(1),
            WindowId(2),
            WindowId(3),
            WindowId(4),
            WindowId(5),
        ] {
            assert!(
                layout.entries.iter().any(|e| e.window_id == wid),
                "actual_layout must contain parked window (id={:?})",
                wid
            );
        }
    }

    #[test]
    fn applied_layout_after_scroll_contains_all_windows() {
        // Positive: scrolling changes viewport but all windows must still be present
        let mut engine = engine_with_three_columns();
        let total_windows = engine.virtual_layout().window_count();

        let result = engine.scroll_right().expect("scroll");
        assert_eq!(
            result.actual_layout.entries.len(),
            total_windows,
            "Scroll must return ALL windows in actual_layout.entries (visible + parked)"
        );
    }

    // --- column_width_bounds (Area 10) ---

    #[test]
    fn engine_column_width_bounds_returns_config_range() {
        // Area 10: column_width_bounds() returns (min_column_width_px, abs_max_width).
        // test_monitor: 1920×1080, test_padding: gap=4.
        // Engine created with min_column_width_px=320.
        // abs_max = 1920 − 2×4 = 1912.
        let engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let (min, max) = engine.column_width_bounds();
        assert_eq!(min, 320, "min should equal min_column_width_px");
        assert_eq!(max, 1912, "max should equal monitor_width − 2×gap");
    }

    #[test]
    fn engine_column_width_bounds_with_different_config() {
        // Area 10: verify bounds change with different min_column_width_px and gap.
        // gap=8 → abs_max = 1920 − 2×8 = 1904.
        let engine = ScrollingSpace::new(
            test_monitor(),
            960,
            500, // min_column_width_px
            100, // min_window_height_px
            100, // min_row_height_px
            Padding {
                window_gap: 8,
                up: 0,
                down: 0,
            },
            4,
        );
        let (min, max) = engine.column_width_bounds();
        assert_eq!(min, 500);
        assert_eq!(max, 1904);
    }

    // --- padding accessor (Phase 4 workspace-op support) ---
    //
    // The `padding()` accessor is how the workspace-op dispatch handlers read
    // the cached `window_gap` to compute the vertical parking unit
    // (`monitor_height + window_gap`) fed to `workspace_y_offset`. It is a
    // trivial getter, but it had zero direct coverage — every `test_padding()`
    // reference elsewhere in this module is the *helper*, not the accessor.

    #[test]
    fn engine_padding_returns_configured_value() {
        // Arrange: construct an engine with the canonical test padding.
        // Act/Assert: the accessor returns exactly the Padding handed to `new`,
        // unchanged — the dispatch path depends on this being the stored value.
        let engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        assert_eq!(engine.padding(), test_padding());
    }

    #[test]
    fn engine_padding_preserves_non_default_margins() {
        // Arrange: a distinct padding with non-zero up/down margins and a
        // different gap, so a future refactor that only stores `window_gap`
        // (or hardcodes the default) would be caught.
        // Act/Assert: the full Padding (gap + up + down) round-trips verbatim.
        let custom = Padding {
            window_gap: 12,
            up: 8,
            down: 40,
        };
        let engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, custom, 4);
        assert_eq!(engine.padding(), custom);
    }

    // Shared helper for the prefix-sum center tests below (col_width=400 so
    // single-column offsets are non-degenerate).

    /// Build an engine with `col_width=400` for non-degenerate center tests.
    fn center_test_engine() -> ScrollingSpace {
        ScrollingSpace::new(test_monitor(), 400, 100, 100, 100, test_padding(), 4)
    }

    // --- Prefix-sum center operations (center_focused_column / center_canvas) ---

    #[test]
    fn engine_center_focused_column_empty_returns_none() {
        // Negative: empty layout → None.
        let mut engine = center_test_engine();
        assert!(engine.center_focused_column(WindowId(1)).is_none());
    }

    #[test]
    fn engine_center_focused_column_centers_at_midpoint() {
        // Positive: single column (400px), gap=4, monitor=1920.
        // canvas_x(0) = 0*(anything) + (0+1)*4 = 4.
        // viewport_offset = 4 - (1920 - 400) / 2 = 4 - 760 = -756.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.set_focus(WindowId(1));

        let diff = engine
            .center_focused_column(WindowId(1))
            .expect("center focused on 1 col");
        assert_eq!(
            diff.virtual_layout.viewport_offset, -756,
            "focused column should land at monitor midpoint"
        );
        assert_eq!(engine.virtual_layout().viewport_offset, -756);
    }

    #[test]
    fn engine_center_focused_column_overflow_always_centers() {
        // Positive: 4 cols × 400px (overflow case), focus on col 2.
        // canvas_x(2) = (400 + 400)*1 + 3*4 = 812.
        // focused_width = 400.
        // offset = 812 - (1920 - 400) / 2 = 812 - 760 = 52.
        // Always centers even in overflow — this is the explicit center behavior.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.set_focus(WindowId(3)); // col 2

        let diff = engine
            .center_focused_column(WindowId(3))
            .expect("center focused overflow");
        assert_eq!(diff.virtual_layout.viewport_offset, 52);
    }

    #[test]
    fn engine_center_focused_column_preserves_all_windows() {
        // Invariant: centering only changes viewport_offset, not focus or columns.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.set_focus(WindowId(2));

        let total = engine.virtual_layout().window_count();
        let diff = engine
            .center_focused_column(WindowId(2))
            .expect("center focused");
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
        assert_eq!(diff.virtual_layout.window_count(), total);
        assert_eq!(diff.actual_layout.entries.len(), total);
    }

    #[test]
    fn engine_center_canvas_empty_returns_none() {
        // Negative: empty layout → None.
        let mut engine = center_test_engine();
        assert!(engine.center_canvas().is_none());
    }

    #[test]
    fn engine_center_canvas_negative_when_canvas_smaller_than_monitor() {
        // Positive: single col (400px), gap=4 → canvas_width = 4 + 404 = 408.
        // offset = (408 - 1920) / 2 = -756. Negative — projection handles it.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));

        let diff = engine.center_canvas().expect("center canvas on 1 col");
        assert_eq!(
            diff.virtual_layout.viewport_offset, -756,
            "canvas < monitor → negative offset"
        );
    }

    #[test]
    fn engine_center_canvas_overflow_positive() {
        // Positive: 5 cols × 400px, gap=4.
        // canvas_width = 4 + 5*(400+4) = 4 + 2020 = 2024.
        // offset = (2024 - 1920) / 2 = 52.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.add_window(WindowId(5));

        let diff = engine.center_canvas().expect("center canvas overflow");
        assert_eq!(diff.virtual_layout.viewport_offset, 52);
    }

    // --- insert_window_with_width tests ---

    #[test]
    fn engine_insert_window_with_width_preserves_custom_width() {
        // Positive: insert with width_px=1440 → column gets quantized width.
        // col_width=400, column_shift=404. Nearest rung to 1440:
        // n = round((1440-400)/404) = round(2.574) = 3. target = 400 + 3*404 = 1612.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.set_focus(WindowId(1));

        engine.insert_window_with_width(WindowId(2), 1440);
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(
            engine.virtual_layout().columns[1].width_px,
            1612,
            "width 1440 should quantize to nearest ladder rung (1612)"
        );
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    #[test]
    fn engine_insert_window_with_width_no_focus_appends() {
        // Positive: no focus → appends at end with custom width.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        // Clear focus to test the no-focus fallback.
        engine.last_focused_window = None;

        engine.insert_window_with_width(WindowId(2), 400);
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(
            engine.virtual_layout().columns[1].rows[0].window_id,
            WindowId(2)
        );
        assert_eq!(engine.last_focused_window(), Some(WindowId(2)));
    }

    // --- initialize_windows with widths tests ---

    #[test]
    fn engine_initialize_windows_with_widths_quantizes() {
        // Positive: widths are quantized to the nearest ladder rung.
        // col_width=400, shift=404. 1440 → nearest rung: n=round((1440-400)/404)=3, 400+3*404=1612.
        let mut engine = center_test_engine();
        engine.initialize_windows(vec![WindowId(1), WindowId(2)], None, Some(&[400, 1440]));
        assert_eq!(engine.virtual_layout().columns.len(), 2);
        assert_eq!(engine.virtual_layout().columns[0].width_px, 400);
        assert_eq!(
            engine.virtual_layout().columns[1].width_px,
            1612,
            "1440 should quantize to 1612"
        );
    }

    #[test]
    fn engine_initialize_windows_with_widths_fit_centers_canvas() {
        // Positive: 2 cols with small widths → fit case → canvas centered.
        // col_width=400. Both widths=400. canvas = 4 + 2*404 = 812.
        // offset = (812 - 1920) / 2 = -554.
        let mut engine = center_test_engine();
        let diff =
            engine.initialize_windows(vec![WindowId(1), WindowId(2)], None, Some(&[400, 400]));
        assert_eq!(
            diff.virtual_layout.viewport_offset, -554,
            "fit case should center entire canvas"
        );
    }

    // --- ensure_focused_visible tests ---
    //
    // `ensure_focused_visible` is the pub(crate) wrapper that dispatch's
    // MoveWindowToWorkspace overflow branch relies on. It resolves the
    // focused column (via `focus_col_index`, with col-0 fallback) and
    // delegates to `mutations::ensure_column_visible`. The underlying
    // mutation is heavily tested; these tests pin the wrapper's contract:
    // empty → None, no-op when visible, shifts to reveal when off-screen,
    // and the no-focus → col-0 fallback.

    #[test]
    fn engine_ensure_focused_visible_empty_returns_none() {
        // Negative: empty layout → None (mirrors center_focused_column /
        // center_canvas on empty).
        let mut engine = center_test_engine();
        assert!(engine.ensure_focused_visible().is_none());
    }

    #[test]
    fn engine_ensure_focused_visible_no_change_when_already_visible() {
        // Positive: 2 cols × 400px fit easily in 1920px viewport at offset 0.
        // Focused col already visible → no viewport shift, no diff (idempotency).
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.set_focus(WindowId(1));
        let before = engine.virtual_layout().viewport_offset;

        let diff = engine.ensure_focused_visible();

        assert!(
            diff.is_none(),
            "no diff when focused col is already visible (idempotency)"
        );
        assert_eq!(
            engine.virtual_layout().viewport_offset,
            before,
            "no shift when focused col is already visible"
        );
    }

    #[test]
    fn engine_ensure_focused_visible_shifts_viewport_to_reveal_focused_col() {
        // Positive: 5 cols × 400px (canvas=2024 > 1920). After 5 add_window
        // calls the viewport is still at offset 0 (add_window doesn't shift).
        // Focus col 4 (WindowId(5)) is off-screen right: canvas_x(4) = 4 +
        // 4*404 = 1620, right = 2020 > vp_right (1920). After
        // ensure_focused_visible, the focused col's right edge must be within
        // the viewport.
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.add_window(WindowId(3));
        engine.add_window(WindowId(4));
        engine.add_window(WindowId(5));
        engine.set_focus(WindowId(5)); // rightmost column

        let offset_before = engine.virtual_layout().viewport_offset;
        let _ = engine.ensure_focused_visible().expect("non-empty layout");

        let offset_after = engine.virtual_layout().viewport_offset;
        let vp_right = offset_after + 1920;
        let col4_right = 1620 + 400; // canvas_x(4) + col_width
        assert!(
            offset_after > offset_before,
            "viewport must shift right when focused col is off-screen"
        );
        assert!(
            col4_right <= vp_right,
            "col 4 right edge ({col4_right}) must be within viewport right ({vp_right}) \
             after ensure_focused_visible"
        );
    }

    #[test]
    fn engine_ensure_focused_visible_uses_col0_fallback_when_focus_is_none() {
        // Contract: when self.last_focused_window is None, focus_col_index returns 0 and
        // ensure_focused_visible targets col 0. With col 0 already visible
        // from offset 0, this is a no-op (returns None — idempotency).
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.last_focused_window = None; // bypass set_focus (which writes Some)
        let before = engine.virtual_layout().viewport_offset;

        let diff = engine.ensure_focused_visible();

        assert!(
            diff.is_none(),
            "no-focus path falls back to col 0; col 0 visible → no diff"
        );
        assert_eq!(
            engine.virtual_layout().viewport_offset,
            before,
            "no shift when col 0 is already visible"
        );
    }

    #[test]
    fn engine_ensure_focused_visible_uses_col0_fallback_when_focus_is_stale() {
        // Contract: when self.last_focused_window points at a window no longer in the
        // layout, focus_col_index falls back to 0. With col 0 visible the
        // fallback is a no-op (returns None — idempotency).
        let mut engine = center_test_engine();
        engine.add_window(WindowId(1));
        engine.add_window(WindowId(2));
        engine.set_focus(WindowId(99)); // not in layout
        let before = engine.virtual_layout().viewport_offset;

        let diff = engine.ensure_focused_visible();

        assert!(
            diff.is_none(),
            "stale focus falls back to col 0; col 0 visible → no diff"
        );
        assert_eq!(
            engine.virtual_layout().viewport_offset,
            before,
            "no shift when col 0 is already visible"
        );
    }

    // --- set_layout tests (loadout restore primitive) ---

    #[test]
    fn set_layout_single_column_single_window() {
        // Positive: single column with one window — verify column count, focus,
        // viewport_offset echoed back, entries present.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let result = engine.set_layout(vec![(960, vec![WindowId(1)])], Some(WindowId(1)), 0);
        assert_eq!(result.virtual_layout.columns.len(), 1);
        assert_eq!(result.virtual_layout.columns[0].rows.len(), 1);
        assert_eq!(
            result.virtual_layout.columns[0].rows[0].window_id,
            WindowId(1)
        );
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
        assert_eq!(result.virtual_layout.viewport_offset, 0);
        assert!(!result.actual_layout.entries.is_empty());
    }

    #[test]
    fn set_layout_multi_column_one_window_each() {
        // Positive: multi-column, one window each — verify column order + widths + focus set.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let result = engine.set_layout(
            vec![
                (960, vec![WindowId(10)]),
                (800, vec![WindowId(20)]),
                (960, vec![WindowId(30)]),
            ],
            Some(WindowId(20)),
            -100,
        );
        assert_eq!(result.virtual_layout.columns.len(), 3);
        // Column order preserved (widths quantized but IDs order intact).
        assert_eq!(
            result.virtual_layout.columns[0].rows[0].window_id,
            WindowId(10)
        );
        assert_eq!(
            result.virtual_layout.columns[1].rows[0].window_id,
            WindowId(20)
        );
        assert_eq!(
            result.virtual_layout.columns[2].rows[0].window_id,
            WindowId(30)
        );
        assert_eq!(engine.last_focused_window(), Some(WindowId(20)));
        assert_eq!(result.virtual_layout.viewport_offset, -100);
    }

    #[test]
    fn set_layout_multirow_column() {
        // Positive: multi-row column (960, vec![W1, W2]) — verify 2 rows in one
        // column (the key capability initialize_windows can't express).
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let result = engine.set_layout(
            vec![(960, vec![WindowId(1), WindowId(2)])],
            Some(WindowId(1)),
            0,
        );
        assert_eq!(result.virtual_layout.columns.len(), 1);
        assert_eq!(
            result.virtual_layout.columns[0].rows.len(),
            2,
            "multi-row column must have 2 rows"
        );
        assert_eq!(
            result.virtual_layout.columns[0].rows[0].window_id,
            WindowId(1)
        );
        assert_eq!(
            result.virtual_layout.columns[0].rows[1].window_id,
            WindowId(2)
        );
    }

    #[test]
    fn set_layout_focus_threaded_through() {
        // Positive: focus param threaded through — last_focused_window reflects
        // the passed focus even when it's not the last window.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let _ = engine.set_layout(
            vec![
                (960, vec![WindowId(1)]),
                (960, vec![WindowId(2)]),
                (960, vec![WindowId(3)]),
            ],
            Some(WindowId(1)), // focus first, not last
            0,
        );
        assert_eq!(
            engine.last_focused_window(),
            Some(WindowId(1)),
            "focus should be WindowId(1), not the last window"
        );
    }

    #[test]
    fn set_layout_viewport_offset_honored() {
        // Positive: viewport_offset honored — pass a non-zero offset, assert
        // it's in the returned virtual_layout.viewport_offset.
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let result = engine.set_layout(vec![(960, vec![WindowId(1)])], Some(WindowId(1)), 500);
        assert_eq!(
            result.virtual_layout.viewport_offset, 500,
            "viewport_offset must be passed through verbatim"
        );
    }

    #[test]
    fn set_layout_overwrites_existing_layout() {
        // Positive: overwrites existing layout — build an engine with add_window
        // a few times, then set_layout with a different arrangement, assert the
        // new arrangement replaced the old (column count/contents changed).
        let mut engine = engine_with_three_columns(); // [W1][W2][W3]
        assert_eq!(engine.virtual_layout().columns.len(), 3);

        // Replace with a single multi-row column.
        let result = engine.set_layout(
            vec![(960, vec![WindowId(10), WindowId(20)])],
            Some(WindowId(10)),
            -200,
        );
        assert_eq!(
            result.virtual_layout.columns.len(),
            1,
            "old 3-column layout replaced by single column"
        );
        assert_eq!(
            result.virtual_layout.columns[0].rows[0].window_id,
            WindowId(10)
        );
        assert_eq!(
            result.virtual_layout.columns[0].rows[1].window_id,
            WindowId(20)
        );
        assert_eq!(engine.last_focused_window(), Some(WindowId(10)));
        assert_eq!(result.virtual_layout.viewport_offset, -200);
    }

    // --- prepend_window tests ---

    #[test]
    fn prepend_window_lands_at_the_left_end_focused_and_visible() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        // Enough columns to overflow the viewport, then scroll to the last
        // one so the strip start is off-screen.
        for i in 1..=6 {
            engine.add_window(WindowId(i));
        }
        engine.set_focus(WindowId(6));
        let _ = engine.ensure_focused_visible();
        assert!(engine.virtual_layout().viewport_offset > 0);

        let applied = engine.prepend_window(WindowId(7));

        assert_eq!(
            applied.virtual_layout.columns[0].rows[0].window_id,
            WindowId(7)
        );
        assert_eq!(applied.virtual_layout.columns.len(), 7);
        assert_eq!(engine.last_focused_window(), Some(WindowId(7)));
        assert_eq!(
            engine.virtual_layout().viewport_offset,
            0,
            "viewport scrolls back to the strip start"
        );
        let entry = &applied.actual_layout.entries[0];
        assert_eq!(entry.window_id, WindowId(7));
        assert_eq!(entry.rect.x, test_padding().window_gap);
        assert!(!test_monitor().is_parked(entry.rect));
    }

    #[test]
    fn prepend_window_into_empty_space_behaves_like_add() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4);
        let applied = engine.prepend_window(WindowId(1));
        assert_eq!(applied.virtual_layout.columns.len(), 1);
        assert_eq!(engine.last_focused_window(), Some(WindowId(1)));
        assert_eq!(applied.actual_layout.entries.len(), 1);
    }

    // ── Row viewports (stacked columns) ───────────────────────────────

    /// A strip with a two-row viewport holding `[1,2,3,4]` in one column,
    /// focus on window 1.
    fn stacked_engine() -> ScrollingSpace {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4)
            .with_row_limits(2, 0);
        engine.add_window(WindowId(1));
        for id in 2..=4 {
            engine
                .add_window_to_focused_column(WindowId(id))
                .expect("stack");
        }
        engine.set_focus(WindowId(1));
        let _ = engine.ensure_focused_visible();
        engine
    }

    fn parked(engine: &ScrollingSpace, id: isize) -> bool {
        let rect = engine
            .actual_layout()
            .find(WindowId(id))
            .expect("window projected")
            .rect;
        engine.monitor().is_parked(rect)
    }

    #[test]
    fn stacked_engine_shows_two_rows_and_parks_the_rest() {
        let engine = stacked_engine();
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 0);
        assert!(!parked(&engine, 1) && !parked(&engine, 2));
        assert!(parked(&engine, 3) && parked(&engine, 4));
        let h = engine
            .actual_layout()
            .find(WindowId(1))
            .expect("1")
            .rect
            .height;
        assert_eq!(h, mutations::column_heights(4, engine.config())[0]);
    }

    #[test]
    fn focus_down_through_a_stack_scrolls_it_and_returns_a_diff() {
        let mut engine = stacked_engine();
        let (f, diff) = engine.focus(WindowId(1), Direction::Down).expect("down");
        assert_eq!(f, WindowId(2));
        assert!(diff.is_none(), "row 2 was already visible");
        let (f, diff) = engine.focus(WindowId(2), Direction::Down).expect("down");
        assert_eq!(f, WindowId(3));
        assert!(diff.is_some(), "the viewport shifted");
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 1);
        assert!(parked(&engine, 1) && !parked(&engine, 3) && parked(&engine, 4));
        let (f, _) = engine.focus(WindowId(3), Direction::Down).expect("down");
        assert_eq!(f, WindowId(4));
        assert!(engine.focus(WindowId(4), Direction::Down).is_none());
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 2);
    }

    #[test]
    fn ensure_focused_visible_reveals_a_parked_row() {
        let mut engine = stacked_engine();
        engine.set_focus(WindowId(4));
        let diff = engine.ensure_focused_visible();
        assert!(diff.is_some());
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 2);
        assert!(!parked(&engine, 4));
        // Already visible: no diff.
        assert!(engine.ensure_focused_visible().is_none());
    }

    #[test]
    fn removing_the_last_rows_clamps_the_viewport() {
        let mut engine = stacked_engine();
        engine.set_focus(WindowId(4));
        let _ = engine.ensure_focused_visible();
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 2);
        let _ = engine.remove_window(WindowId(4));
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 1);
        assert!(!parked(&engine, 3) && !parked(&engine, 2) && parked(&engine, 1));
    }

    #[test]
    fn set_row_limits_reshares_heights_and_keeps_the_focus_visible() {
        let mut engine = stacked_engine();
        engine.set_focus(WindowId(4));
        let _ = engine.ensure_focused_visible();
        // Widen the viewport to three rows: the offset is pulled back so the
        // focused row stays in, heights are re-shared over three slots.
        let _ = engine.set_row_limits(3, 0);
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 1);
        assert!(!parked(&engine, 4) && parked(&engine, 1));
        let h = engine
            .actual_layout()
            .find(WindowId(4))
            .expect("4")
            .rect
            .height;
        assert_eq!(h, mutations::column_heights(4, engine.config())[3]);
        // Back to no viewport: everything on screen, offset 0.
        let _ = engine.set_row_limits(0, 0);
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 0);
        assert!((1..=4).all(|id| !parked(&engine, id)));
        // Reconfigure keeps the limits.
        let _ = engine.set_row_limits(2, 3);
        let _ = engine.set_base_column_width(800);
        assert_eq!(
            (engine.config().visible_rows, engine.config().max_rows),
            (2, 3)
        );
    }

    #[test]
    fn restore_into_a_full_column_seats_the_window_after_it() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4)
            .with_row_limits(2, 2);
        engine.add_window(WindowId(1));
        engine
            .add_window_to_focused_column(WindowId(2))
            .expect("stack");
        engine.add_window(WindowId(9));
        // Window 3 claims row 1 of column 0, which is full: it becomes a
        // new column between the stack and window 9.
        let _ = engine.restore_window_at(0, 1, false, WindowId(3), None);
        let order: Vec<Vec<isize>> = engine
            .virtual_layout()
            .columns
            .iter()
            .map(|c| c.rows.iter().map(|r| r.window_id.0).collect())
            .collect();
        assert_eq!(order, vec![vec![1, 2], vec![3], vec![9]]);
        assert_eq!(engine.last_focused_window(), Some(WindowId(3)));
    }

    #[test]
    fn set_layout_clamps_the_viewport_to_the_restored_focus() {
        let mut engine = ScrollingSpace::new(test_monitor(), 960, 320, 100, 100, test_padding(), 4)
            .with_row_limits(2, 0);
        let _ = engine.set_layout(
            vec![(
                960,
                vec![WindowId(1), WindowId(2), WindowId(3), WindowId(4)],
            )],
            Some(WindowId(4)),
            0,
        );
        assert_eq!(engine.virtual_layout().columns[0].row_offset, 2);
        assert!(!parked(&engine, 4) && parked(&engine, 1));
    }
}
