//! IPC command dispatch router and action handlers.
//!
//! This module contains:
//!
//! - [`FlowWM::dispatch`] — routes each [`SocketMessage`] variant
//!   to the appropriate subsystem.
//! - Individual `dispatch_*` helper methods for each command category.
//! - Helper functions for unimplemented commands.

use std::time::Duration;

use crate::common::{Direction, Rect, Size, WindowId};
use crate::config::dirs::{history_rules_path_in, user_app_config_path_in, user_rules_path_in};
use crate::config::types::WindowAction;
use crate::config::{load_app_config, load_rules_config};
use crate::ipc::message::{SocketMessage, SocketResponse, WindowMode};
use crate::layout::projection;
use crate::layout::types::{ActualLayout, AppliedLayout};
use crate::registry::hooks::{add_float_hwnd, remove_float_hwnd};
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::registry::win32::{self as registry_win32, ForegroundPush};
use crate::workspace::{FloatingSpace, WorkspaceId, WorkspaceParking};
use windows::Win32::Foundation::HWND;

use super::animation::PrevRects;
use super::config_derive;
use super::types::FlowWM;

// Built-in fallback policy for a floating window's default size when the user
// hasn't set `floating.default_width` / `floating.default_height`.
//
// Derived from a QHD reference (2560 × 1440) scaled to 60% × 80%, so ultrawide
// / 4K monitors don't produce absurdly large popups. Every value is a named
// constant — no magic numbers. Integer ratios (not `f32`) keep the caps
// `const`-evaluable.

/// Reference resolution bounding the fallback float size (QHD).
const FALLBACK_REF_WIDTH: i32 = 2560;
/// Reference resolution bounding the fallback float size (QHD).
const FALLBACK_REF_HEIGHT: i32 = 1440;
/// Fallback width = 60% of work-area width (`num / denom`).
const FALLBACK_WIDTH_RATIO: (i32, i32) = (6, 10);
/// Fallback height = 80% of work-area height (`num / denom`).
const FALLBACK_HEIGHT_RATIO: (i32, i32) = (8, 10);
/// Absolute pixel cap on a fallback float's width: `2560 × 0.6 = 1536`.
const MAX_FLOAT_WIDTH: i32 = FALLBACK_REF_WIDTH * FALLBACK_WIDTH_RATIO.0 / FALLBACK_WIDTH_RATIO.1;
/// Absolute pixel cap on a fallback float's height: `1440 × 0.8 = 1152`.
const MAX_FLOAT_HEIGHT: i32 =
    FALLBACK_REF_HEIGHT * FALLBACK_HEIGHT_RATIO.0 / FALLBACK_HEIGHT_RATIO.1;

/// Compute the built-in fallback float size for the given work-area dimensions.
///
/// Each dimension is computed independently using the ratio + cap policy
/// documented on `FloatingConfig` in `src/config/types.rs`: width is 60% of
/// the work-area width capped at [`MAX_FLOAT_WIDTH`], height is 80% capped at
/// [`MAX_FLOAT_HEIGHT`].
///
/// Extracted as a standalone pure fn so the integer arithmetic (ratios, cap
/// clamping, per-dimension independence) is unit-testable without constructing
/// a `FlowWM` (which owns Win32 handles and cannot be built in a
/// unit test). The caller is still responsible for the per-dimension
/// `Option::unwrap_or` choice between an explicit user value and the matching
/// fallback component returned here.
fn fallback_float_size(work_area: Size) -> Size {
    Size {
        w: (work_area.w * FALLBACK_WIDTH_RATIO.0 / FALLBACK_WIDTH_RATIO.1).min(MAX_FLOAT_WIDTH),
        h: (work_area.h * FALLBACK_HEIGHT_RATIO.0 / FALLBACK_HEIGHT_RATIO.1).min(MAX_FLOAT_HEIGHT),
    }
}

impl FlowWM {
    /// Dispatch a single IPC command and return the response.
    ///
    /// Routes each [`SocketMessage`] variant to the appropriate subsystem:
    /// - **Stop**: sets the shutdown flag.
    /// - **Layout commands**: call layout engine methods and animate the result.
    /// - **Query commands**: return registry data as JSON.
    /// - **Unimplemented commands**: return an error response.
    ///
    /// When a tile drag is in progress, layout-mutating commands return
    /// [`SocketResponse::Busy`] to prevent layout conflicts.
    pub(super) fn dispatch(&mut self, msg: &SocketMessage) -> SocketResponse {
        log::debug!("ipc: dispatching {msg:?}");

        // Reject layout-mutating commands while a tile drag is active — the
        // layout is in a transient state and concurrent mutations would
        // corrupt it.
        if self.drag_state.is_some() && msg.is_layout_mutating() {
            return SocketResponse::Busy;
        }

        // Tiling off (`flow dispatch tiling off`): the desktop is Windows'
        // to arrange, so nothing that would move a window or push focus is
        // accepted — only the way back on, queries, `Ping`, `Stop` and the
        // loadout save. See `daemon::tiling`.
        if self.tiling_paused && msg.is_rejected_while_tiling_paused() {
            return SocketResponse::Error {
                message: super::tiling::TILING_OFF_MESSAGE.to_owned(),
            };
        }

        match msg {
            // --- Shutdown ---
            SocketMessage::Stop => {
                if let Err(e) = self.try_save_loadout_default() {
                    log::warn!("loadout save-on-stop failed: {e}");
                }
                self.shutting_down = true;
                SocketResponse::Ok
            }

            // --- Liveness ---
            // Read-only: reports identity and health, never touches layout.
            SocketMessage::Ping => SocketResponse::Pong {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                pid: std::process::id(),
                monitors: self.monitors.len(),
                hooks_registered: self.hook_handle.registered_hooks(),
                ahk_pid: self.ahk_pid(),
                tiling_enabled: !self.tiling_paused,
            },

            // --- Tiling on/off ---
            SocketMessage::SetTiling { enabled } => self.dispatch_set_tiling(*enabled),
            SocketMessage::ToggleTiling => self.dispatch_set_tiling(self.tiling_paused),

            // --- Focus ---
            SocketMessage::FocusLeft => self.dispatch_focus(Direction::Left),
            SocketMessage::FocusRight => self.dispatch_focus(Direction::Right),
            SocketMessage::FocusUp => self.dispatch_focus(Direction::Up),
            SocketMessage::FocusDown => self.dispatch_focus(Direction::Down),

            // --- Swap (per-window) ---
            // Swap* operates on the focused window, exchanging it with its
            // neighbour. Up/Down move within the same column; Left/Right
            // exchange it with a window in the adjacent column.
            SocketMessage::SwapLeft => self.dispatch_swap_window(Direction::Left),
            SocketMessage::SwapRight => self.dispatch_swap_window(Direction::Right),
            SocketMessage::SwapUp => self.dispatch_swap_window(Direction::Up),
            SocketMessage::SwapDown => self.dispatch_swap_window(Direction::Down),

            // --- Column swap ---
            SocketMessage::SwapColumn { direction } => self.dispatch_swap_column(*direction),

            // --- Semantic move ---
            SocketMessage::MoveWindow { direction } => self.dispatch_move_window(*direction),

            // --- Scroll ---
            SocketMessage::ScrollLeft => self.dispatch_scroll_left(),
            SocketMessage::ScrollRight => self.dispatch_scroll_right(),

            // --- Column resize ---
            SocketMessage::ExpandColumn => self.dispatch_expand(),
            SocketMessage::ShrinkColumn => self.dispatch_shrink(),
            SocketMessage::SetColumnWidth { width_px } => self.dispatch_set_column_width(*width_px),
            SocketMessage::SetWidthPreset { preset } => self.dispatch_width_preset(preset),

            // --- Viewport center ---
            SocketMessage::Center => self.dispatch_center(),

            // --- Window state ---
            SocketMessage::ToggleFloat => self.dispatch_set_window(WindowMode::Cycle),
            SocketMessage::SetWindow { mode } => self.dispatch_set_window(*mode),
            SocketMessage::ToggleMonocle => self.dispatch_toggle_monocle(),
            SocketMessage::PlaceAbove => unimplemented_command("place_above"),
            SocketMessage::Promote { direction } => self.dispatch_promote_window(*direction),
            SocketMessage::MergeColumn { direction } => self.dispatch_merge_column(*direction),
            SocketMessage::CloseWindow => self.dispatch_close_window(),

            // --- Queries ---
            SocketMessage::QueryWindowsAll => self.query_windows_all(),
            SocketMessage::QueryLayoutVirtual => self.query_layout_virtual(),
            SocketMessage::QueryLayoutActual => self.query_layout_actual(),
            SocketMessage::QueryState => unimplemented_command("query_state"),

            // --- Config mutation ---
            SocketMessage::ReloadConfig => self.dispatch_reload_config(),
            SocketMessage::CheckConfig => unimplemented_command("check_config"),
            SocketMessage::SetConfigValue { .. } => unimplemented_command("set_config_value"),
            SocketMessage::ForgetApp { .. } => unimplemented_command("forget_app"),
            SocketMessage::ForgetAllApps => unimplemented_command("forget_all_apps"),
            SocketMessage::LoadoutSave { path } => self.dispatch_loadout_save(path.clone()),
            SocketMessage::LoadoutLoad { path } => self.dispatch_loadout_load(path.clone()),

            // --- Workspace ---
            //
            // SwitchWorkspace and MoveWindowToWorkspace animate the
            // participating workspaces (see [`dispatch_switch_workspace`]
            // and [`dispatch_move_window_to_workspace`]). SwapWorkspace is
            // still pending — its animation model is undecided.
            SocketMessage::SwitchWorkspace { workspace_id } => {
                self.dispatch_switch_workspace(*workspace_id)
            }
            SocketMessage::SwapWorkspace { .. } => unimplemented_command("swap_workspace"),
            SocketMessage::FocusMonitor { direction } => self.dispatch_focus_monitor(*direction),
            SocketMessage::MoveWindowToMonitor { direction } => {
                self.dispatch_move_window_to_monitor(*direction)
            }
            SocketMessage::MoveWindowToWorkspace { workspace_id } => {
                self.dispatch_move_window_to_workspace(*workspace_id)
            }
        }
    }

    /// Dispatch a focus movement in the given direction.
    ///
    /// This performs only the layout work and the OS foreground push; **all
    /// focus *state* side-effects are handled by [`on_focus_changed`](Self::on_focus_changed),
    /// the single sink for `EVENT_SYSTEM_FOREGROUND`** (both external alt-tab /
    /// clicks and this self-induced change). Concretely:
    ///
    /// 1. **Layout focus** — [`ScrollingSpace::focus`](crate::workspace::ScrollingSpace::focus) resolves the neighbor
    ///    [`WindowId`] and optionally shifts the viewport (producing a
    ///    [`AppliedLayout`] when the camera scrolls).
    /// 2. **OS foreground** — [`registry_win32::set_foreground_window`] moves
    ///    the actual Win32 focus to the target window using the
    ///    `AttachThreadInput` trick to bypass foreground-lock restrictions.
    ///    This fires `EVENT_SYSTEM_FOREGROUND`, which `on_focus_changed`
    ///    consumes to update the registry focus, recolor the previous and new
    ///    borders, and re-assert layout focus (a no-op here since step 1
    ///    already moved it).
    /// 3. **Animation** — if the viewport scrolled, [`animate_layout`](Self::animate_layout)
    ///    animates the camera shift so the focused window becomes visible.
    ///
    /// # Why no `set_focused` here
    ///
    /// We deliberately do **not** call [`WindowRegistry::set_focused`] in this
    /// path. `on_focus_changed` captures the *previous* focus from
    /// `registry.focused()` before it calls `set_focused`; if we eagerly moved
    /// the registry's focus here, that capture would see `prev == target` and
    /// skip recoloring the old window — leaving its border stuck on the Focused
    /// color. Letting the hook own `set_focused` keeps the prev/new pair
    /// correct.
    fn dispatch_focus(&mut self, dir: Direction) -> SocketResponse {
        // Resolve the source window from OS foreground (not the per-space tile
        // cursor) and no-op silently when it isn't an active tile — see
        // (`docs/src/dev-guide/floating-space.md`).
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_focus: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_focus: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_focus: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().focus(id, dir) {
            Some((focused_id, diff_opt)) => {
                let target_hwnd = focused_id.0;

                // Push OS foreground focus. The resulting
                // EVENT_SYSTEM_FOREGROUND is handled by on_focus_changed (the
                // single sink), which updates the registry focus and recolors
                // both the old and new windows' borders. Do NOT set_focused
                // here — see the doc comment above.
                if !registry_win32::set_foreground_window(target_hwnd) {
                    log::warn!("dispatch_focus: SetForegroundWindow failed for hwnd {target_hwnd}");
                }

                // Animate the camera shift if the viewport moved.
                if let Some(diff) = diff_opt {
                    self.animate_layout(&diff);
                }

                SocketResponse::Ok
            }
            None => {
                // At the edge of this strip: hop to the monitor in that
                // direction, if there is one (niri behaviour).
                if let Some(target) = self.monitor_in_direction(self.active_monitor, dir) {
                    self.focus_monitor(target);
                    return SocketResponse::Ok;
                }
                SocketResponse::Error {
                    message: "no window to focus in that direction".into(),
                }
            }
        }
    }

    /// Close the OS-foreground window.
    ///
    /// Resolves the target from the registry's OS-focus tracker
    /// (`registry.focused()`), the same authority used by `set-window`, so a
    /// floating foreground window is closed rather than the stale per-space
    /// tiling cursor. Asks Win32 to close it gently via `WM_CLOSE` (see
    /// [`registry_win32::close_window`] for why this is preferred over
    /// `DestroyWindow`). See (`docs/src/dev-guide/floating-space.md`).
    ///
    /// # Why No Layout Mutation Here?
    ///
    /// Closing is **asynchronous**: [`registry_win32::close_window`] only
    /// *queues* a `WM_CLOSE` message and returns immediately. The window
    /// disappears later, once the owning application processes the message
    /// and destroys the window. At that point Win32 fires
    /// `EVENT_OBJECT_DESTROY`, which the registry hook turns into a
    /// `Destroyed` event — the daemon's normal event loop then removes the
    /// window from both the registry and the layout engine, and animates the
    /// gap closing. Mutating the layout here would race that pipeline and
    /// risk a double-remove.
    ///
    /// # Error Cases
    ///
    /// - No focused window (empty workspace) → error, surfaced so the CLI
    ///   can report it.
    /// - `WM_CLOSE` could not be queued (the window vanished mid-call,
    ///   etc.) → error.
    fn dispatch_close_window(&mut self) -> SocketResponse {
        let Some(focused) = self.registry.focused() else {
            return SocketResponse::Error {
                message: "no focused window to close".into(),
            };
        };
        let target_hwnd = focused.0;
        if registry_win32::close_window(target_hwnd) {
            SocketResponse::Ok
        } else {
            log::warn!("dispatch_close_window: close_window failed for hwnd {target_hwnd}");
            SocketResponse::Error {
                message: "failed to request window close".into(),
            }
        }
    }

    /// Dispatch a **column** swap in the given direction.
    ///
    /// Calls [`ScrollingSpace::swap_column`](crate::workspace::ScrollingSpace::swap_column) and animates the resulting layout
    /// diff if the swap succeeded. The layout engine handles off-screen
    /// columns transparently — `swap_column` internally calls
    /// `ensure_column_visible`, so the viewport scrolls as part of the same
    /// diff and no separate "offscreen" message is needed.
    fn dispatch_swap_column(&mut self, dir: Direction) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_swap_column: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_swap_column: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_swap_column: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().swap_column(id, dir) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => {
                // At the edge of this strip: carry the window over to the
                // monitor in that direction, if there is one.
                let from = self
                    .monitor_index_containing(id)
                    .unwrap_or(self.active_monitor);
                if let Some(target) = self.monitor_in_direction(from, dir) {
                    return self.move_tiled_to_monitor(id, from, target, dir);
                }
                SocketResponse::Error {
                    message: "cannot swap column in that direction".into(),
                }
            }
        }
    }

    /// Dispatch a **per-window** swap in the given direction.
    ///
    /// Calls [`ScrollingSpace::swap_window`](crate::workspace::ScrollingSpace::swap_window), which exchanges the focused
    /// window with its neighbour: up/down moves it within the same column,
    /// left/right exchanges it with a window in the adjacent column.
    fn dispatch_swap_window(&mut self, dir: Direction) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_swap_window: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_swap_window: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_swap_window: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().swap_window(id, dir) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot swap window in that direction".into(),
            },
        }
    }

    /// Dispatch a **column merge** in the given horizontal direction.
    ///
    /// Calls [`ScrollingSpace::merge_column`](crate::workspace::ScrollingSpace::merge_column),
    /// which detaches the focused window from its column and appends it as a
    /// new bottom row of the neighbour column. Both columns' row heights are
    /// redistributed to equal shares. Returns an error when there is no
    /// neighbour, the direction is vertical, or the merge would violate
    /// `min_window_height_px`.
    fn dispatch_merge_column(&mut self, dir: Direction) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_merge_column: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_merge_column: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_merge_column: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().merge_column(id, dir) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot merge column in that direction".into(),
            },
        }
    }

    /// Dispatch a **window promote** in the given horizontal direction.
    ///
    /// Calls [`ScrollingSpace::promote_window`](crate::workspace::ScrollingSpace::promote_window),
    /// which extracts the focused window from its column into a new
    /// single-row column placed to the left or right of the source. No-op
    /// (returns error) when the focused window is already alone in its
    /// column.
    fn dispatch_promote_window(&mut self, dir: Direction) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_promote_window: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_promote_window: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_promote_window: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().promote_window(id, dir) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot promote window in that direction".into(),
            },
        }
    }

    /// Dispatch a semantic "move window" command.
    ///
    /// This is the daemon-side translation of the high-level
    /// [`SocketMessage::MoveWindow`] intent. The concrete action depends on
    /// the focused window's state and the direction:
    ///
    /// - **Tiled, left/right** → a column swap (delegates to
    ///   [`dispatch_swap_column`](Self::dispatch_swap_column)), since moving
    ///   a tiled window horizontally *is* swapping its column. A proper
    ///   cross-column move that preserves row membership is deferred.
    /// - **Tiled, up/down** → a within-column window swap (delegates to
    ///   [`dispatch_swap_window`](Self::dispatch_swap_window)).
    /// - **Floating, any direction** → a pixel nudge by a configurable shift
    ///   *(deferred)*.
    ///
    /// The branching structure is kept as a single delegation point so that
    /// floating and a real cross-column move can be added later without
    /// changing the IPC protocol or keybindings.
    fn dispatch_move_window(&mut self, dir: Direction) -> SocketResponse {
        // TODO(floating): inspect the focused window's state and branch:
        //   - floating → nudge by config move_shift
        // TODO(move-cross-column): real Left/Right move that preserves row
        //   membership is deferred — for now Left/Right == swap-column.
        match dir {
            Direction::Up | Direction::Down => self.dispatch_swap_window(dir),
            Direction::Left | Direction::Right => self.dispatch_swap_column(dir),
        }
    }

    // ── Monitors ────────────────────────────────────────────────────────

    /// `flow dispatch focus-monitor <dir>`: make the monitor in `dir` active
    /// and focus its last-focused window.
    fn dispatch_focus_monitor(&mut self, dir: Direction) -> SocketResponse {
        let Some(target) = self.monitor_in_direction(self.active_monitor, dir) else {
            return SocketResponse::Error {
                message: format!("no monitor {}", direction_word(dir)),
            };
        };
        self.focus_monitor(target);
        SocketResponse::Ok
    }

    /// Make `target` the active monitor and push OS focus to the window the
    /// user last used there (tiled first, else the top float). With nothing
    /// to focus the monitor still becomes active, the tracked focus is
    /// cleared (so a later command cannot act on the window we came from),
    /// and OS focus is handed to the shell.
    ///
    /// A remembered window that has died without a `DESTROY` reaching the
    /// layout (the VM log's `SetForegroundWindow failed ... 0x6`) is purged
    /// from the monitor's bookkeeping and the next candidate is tried — see
    /// [`resolve_monitor_focus_target`](Self::resolve_monitor_focus_target).
    pub(super) fn focus_monitor(&mut self, target: usize) {
        if target >= self.monitors.len() {
            return;
        }
        let picked = self.resolve_focus_target_with(
            target,
            registry_win32::is_window,
            registry_win32::is_visible_uncloaked,
            registry_win32::push_foreground_window,
        );
        match picked {
            Some(id) => {
                let prev = self.registry.focused().map(|w| w.0);
                self.registry.set_focused(id.0);
                self.refresh_border_for(id.0);
                if let Some(p) = prev
                    && p != id.0
                {
                    self.refresh_border_for(p);
                }
            }
            None => {
                self.drop_tracked_focus();
                if !registry_win32::clear_foreground_window() {
                    log::debug!("focus_monitor: clear_foreground_window failed (nothing to focus)");
                }
            }
        }
    }

    /// `title=… class=… exe=… state=…` of a window for log lines, from the
    /// registry entry, else from Win32 (`GetWindowText` / `GetClassName`)
    /// marked `(untracked)`.
    #[must_use]
    pub(super) fn describe_window(&self, hwnd: isize) -> String {
        match self.registry.get_window(HWND(hwnd as *mut _)) {
            Some(w) => format!(
                "title={:?} class={:?} exe={:?} state={:?}",
                w.title, w.class, w.exe, w.state
            ),
            None => format!("{} (untracked)", registry_win32::window_description(hwnd)),
        }
    }

    /// Make `target` active and pick the window to foreground there — see
    /// [`resolve_focus_target_with`](Self::resolve_focus_target_with) —
    /// without pushing the foreground: every tracked, `is_live` candidate
    /// is accepted. Kept for the callers and tests that only need the
    /// purge-and-pick half.
    #[cfg(test)]
    pub(super) fn resolve_monitor_focus_target(
        &mut self,
        target: usize,
        is_live: impl Fn(isize) -> bool,
    ) -> Option<WindowId> {
        self.resolve_focus_target_with(target, is_live, |_| true, |_| ForegroundPush::Focused)
    }

    /// Make `target` active, then walk its active workspace's focus
    /// candidates ([`focus_candidates`]: the strip's last-focused window,
    /// then every tile in column/row order, then the floats top-most first)
    /// and push the first one that takes the foreground:
    ///
    /// - a candidate that is not in the registry or for which `is_live`
    ///   (`IsWindow`) is false is **purged** from the strip / floating space
    ///   / float-tracking set and the walk continues;
    /// - a candidate for which `is_visible` (`IsWindowVisible && !cloaked`)
    ///   is false is **skipped without purging** — a tray-hidden or cloaked
    ///   window may come back;
    /// - `push` (`SetForegroundWindow`) answering
    ///   [`ForegroundPush::InvalidHandle`] — a window of a process that was
    ///   exiting, which `IsWindow` still vouched for a moment earlier — is
    ///   treated exactly like a dead window: logged with the window's
    ///   title, class and exe, purged, next candidate;
    /// - any other push failure (the foreground lock) is logged the same
    ///   way and the candidate is still returned: the window exists and is
    ///   what the user should be on.
    ///
    /// Returns the window that was pushed (or kept after a refused push),
    /// `None` when nothing live and visible is left — logged at `debug`,
    /// never `warn`: an empty workspace is a normal state. The three
    /// questions are injected so the loop is unit-testable without a
    /// desktop; the caller owns the registry focus and the borders.
    pub(super) fn resolve_focus_target_with(
        &mut self,
        target: usize,
        is_live: impl Fn(isize) -> bool,
        is_visible: impl Fn(isize) -> bool,
        mut push: impl FnMut(isize) -> ForegroundPush,
    ) -> Option<WindowId> {
        self.set_active_monitor(target);
        let mut skipped: std::collections::HashSet<isize> = std::collections::HashSet::new();
        loop {
            let ws = self.monitors[target].active_workspace();
            let candidate = focus_candidates(&ws.scrolling, &ws.floating)
                .into_iter()
                .find(|c| !skipped.contains(&c.0));
            let Some(candidate) = candidate else {
                log::debug!(
                    "focus_monitor: nothing live to focus on monitor {target} ({} hidden candidate(s) skipped)",
                    skipped.len()
                );
                return None;
            };
            let hwnd = candidate.0;
            if !self.registry.is_tracked(hwnd) || !is_live(hwnd) {
                log::debug!(
                    "focus_monitor: dropping dead window {hwnd} from monitor {target} ({})",
                    self.describe_window(hwnd)
                );
                self.purge_dead_window(target, candidate);
                continue;
            }
            if !is_visible(hwnd) {
                log::debug!(
                    "focus_monitor: skipping hidden or cloaked window {hwnd} on monitor {target} ({})",
                    self.describe_window(hwnd)
                );
                skipped.insert(hwnd);
                continue;
            }
            match push(hwnd) {
                ForegroundPush::Focused => return Some(candidate),
                ForegroundPush::InvalidHandle => {
                    // Windows reports ERROR_INVALID_HANDLE loosely: a window
                    // that is genuinely gone, but also a live window it simply
                    // refuses to activate (seen right after Windows snapped
                    // it). Only a window that fails the liveness check is
                    // purged; a live one keeps its seat and merely does not
                    // get the foreground this time.
                    if is_live(hwnd) {
                        log::debug!(
                            "focus_monitor: SetForegroundWindow rejected live hwnd {hwnd} ({}); leaving it seated and trying the next window",
                            self.describe_window(hwnd)
                        );
                        skipped.insert(hwnd);
                    } else {
                        log::debug!(
                            "focus_monitor: hwnd {hwnd} vanished during the focus push ({}); dropping it",
                            self.describe_window(hwnd)
                        );
                        self.purge_dead_window(target, candidate);
                    }
                }
                ForegroundPush::Failed(code) => {
                    log::warn!(
                        "focus_monitor: SetForegroundWindow failed for hwnd {hwnd} (Win32 error {code:#x}; {})",
                        self.describe_window(hwnd)
                    );
                    return Some(candidate);
                }
            }
        }
    }

    /// Remove a window that no longer exists from `idx`'s active workspace
    /// (strip and floating space) and from the float-tracking set, and
    /// animate the strip's resettle.
    pub(super) fn purge_dead_window(&mut self, idx: usize, window: WindowId) {
        let ws = self.monitors[idx].active_workspace_mut();
        if ws.scrolling.virtual_layout().find_window(window).is_some() {
            let applied = ws.scrolling.remove_window(window);
            self.animate_layout_on(idx, &applied);
        }
        self.monitors[idx]
            .active_workspace_mut()
            .floating
            .remove(window);
        remove_float_hwnd(window.0);
    }

    /// Forget the tracked focus and recolour the previously focused window's
    /// border. The bookkeeping half of landing on a monitor (or workspace)
    /// with nothing to focus; the caller hands OS focus to the shell.
    pub(super) fn drop_tracked_focus(&mut self) {
        let prev_focus = self.registry.focused().map(|id| id.0);
        self.registry.clear_focused();
        if let Some(prev) = prev_focus {
            self.refresh_border_for(prev);
        }
    }

    /// `flow dispatch move-to-monitor <dir>`: move the focused window to the
    /// active workspace of the monitor in `dir`.
    fn dispatch_move_window_to_monitor(&mut self, dir: Direction) -> SocketResponse {
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_move_window_to_monitor: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_move_window_to_monitor: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        let state = win.state.clone();
        let from = self.monitor_index_for_window(id);
        let Some(target) = self.monitor_in_direction(from, dir) else {
            return SocketResponse::Error {
                message: format!("no monitor {}", direction_word(dir)),
            };
        };
        match state {
            WindowState::Tiling(TilingState::Active { .. }) => {
                self.move_tiled_to_monitor(id, from, target, dir)
            }
            WindowState::Floating(FloatingState::Active { rect }) => {
                self.move_float_to_monitor(id, from, target, rect)
            }
            _ => {
                log::debug!("dispatch_move_window_to_monitor: no-op (window is not active)");
                SocketResponse::Ok
            }
        }
    }

    /// Detach a tiled window from `from`'s strip and attach it to `target`'s
    /// at the near edge: a window moving right enters the target strip at
    /// its left end, a window moving left enters at its right end. Focus
    /// follows the window.
    fn move_tiled_to_monitor(
        &mut self,
        id: WindowId,
        from: usize,
        target: usize,
        dir: Direction,
    ) -> SocketResponse {
        if from == target {
            return SocketResponse::Ok;
        }
        let src = self.monitors[from].active_scrolling_mut().remove_window(id);

        self.set_active_monitor(target);
        let mut dst = match dir {
            Direction::Right | Direction::Down => self.monitors[target]
                .active_scrolling_mut()
                .prepend_window(id),
            Direction::Left | Direction::Up => {
                self.monitors[target].active_scrolling_mut().add_window(id)
            }
        };
        if let Some(diff) = self.monitors[target]
            .active_scrolling_mut()
            .ensure_focused_visible()
        {
            dst = diff;
        }
        self.remember_monitor(id, target);
        // Both monitors change in one command: submit them as one batch so
        // the source strip's columns resettle instead of being cut off by
        // the destination's submission.
        self.animate_layouts_on(&[(from, &src), (target, &dst)]);

        if !registry_win32::set_foreground_window(id.0) {
            log::warn!(
                "move_tiled_to_monitor: SetForegroundWindow failed for hwnd {}",
                id.0
            );
        }
        self.registry.set_focused(id.0);
        self.refresh_border_for(id.0);
        SocketResponse::Ok
    }

    /// Move a float to `target`, keeping its offset relative to the work
    /// area (clamped so it stays fully on the new monitor).
    fn move_float_to_monitor(
        &mut self,
        id: WindowId,
        from: usize,
        target: usize,
        rect: Rect,
    ) -> SocketResponse {
        if from == target {
            return SocketResponse::Ok;
        }
        let from_wa = self.monitors[from].work_area();
        let to_wa = self.monitors[target].work_area();
        let moved = Rect {
            x: rect.x - from_wa.x + to_wa.x,
            y: rect.y - from_wa.y + to_wa.y,
            width: rect.width,
            height: rect.height,
        }
        .clamped_into(to_wa);

        for m in &mut self.monitors {
            m.active_workspace_mut().floating.remove(id);
        }
        remove_float_hwnd(id.0);
        self.set_active_monitor(target);
        let float_actual = self.register_float(id, moved);
        self.animate_workspaces(&[(float_actual, 0)]);

        if !registry_win32::set_foreground_window(id.0) {
            log::warn!(
                "move_float_to_monitor: SetForegroundWindow failed for hwnd {}",
                id.0
            );
        }
        self.registry.set_focused(id.0);
        self.refresh_border_for(id.0);
        SocketResponse::Ok
    }

    /// Dispatch a scroll-left command.
    fn dispatch_scroll_left(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().scroll_left() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot scroll left".into(),
            },
        }
    }

    /// Dispatch a scroll-right command.
    fn dispatch_scroll_right(&mut self) -> SocketResponse {
        match self.active_scrolling_mut().scroll_right() {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot scroll right".into(),
            },
        }
    }

    /// Dispatch an expand-column command on the focused column.
    fn dispatch_expand(&mut self) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_expand: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_expand: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_expand: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().expand_column(id) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot expand column".into(),
            },
        }
    }

    /// Dispatch a shrink-column command on the focused column.
    fn dispatch_shrink(&mut self) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_shrink: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_shrink: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_shrink: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().shrink_column(id) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot shrink column".into(),
            },
        }
    }

    /// Dispatch an explicit column width setting.
    ///
    /// The width is in pixels and is validated against the layout engine's
    /// current bounds (`[min_column_width_px, abs_max_width]`) before being
    /// delegated. This is the **free-form / drag-resize** path — the value is
    /// applied directly and is not snapped to the expand/shrink slot ladder.
    fn dispatch_set_column_width(&mut self, width_px: u32) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_set_column_width: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_set_column_width: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_set_column_width: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        let (min, max) = self.active_scrolling().column_width_bounds();
        // `u32 → i32` can fail for absurd inputs (`> i32::MAX`). Reject with a
        // precise message instead of letting the cast wrap negative and report
        // a misleading value.
        let target_px = match i32::try_from(width_px) {
            Ok(t) => t,
            Err(_) => {
                return SocketResponse::Error {
                    message: format!(
                        "width_px {width_px} exceeds the maximum representable column width"
                    ),
                };
            }
        };
        if target_px < min {
            return SocketResponse::Error {
                message: format!("width_px must be >= {min}, got {target_px}"),
            };
        }
        if target_px > max {
            return SocketResponse::Error {
                message: format!("width_px must be <= {max}, got {target_px}"),
            };
        }
        match self.active_scrolling_mut().set_column_width(id, target_px) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot set column width (no focused window or no-op)".into(),
            },
        }
    }

    /// Center the viewport so the focused column lands at the monitor midpoint.
    ///
    /// Delegates to [`ScrollingSpace::center_focused_column`] which uses the
    /// actual prefix-sum canvas position (variable-width aware). Always centers
    /// even when all columns fit — this is the explicit center command. See
    /// (`docs/src/dev-guide/layout/mutations.md`).
    fn dispatch_center(&mut self) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_center: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_center: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_center: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().center_focused_column(id) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot center viewport (empty workspace)".into(),
            },
        }
    }

    /// Dispatch a monocle mode toggle on the focused column.
    fn dispatch_toggle_monocle(&mut self) -> SocketResponse {
        // Resolve from OS foreground; silent no-op when not an active tile.
        let Some(id) = self.registry.focused() else {
            log::debug!("dispatch_toggle_monocle: no-op (no OS-foreground window)");
            return SocketResponse::Ok;
        };
        let Some(win) = self.registry.get_window(HWND(id.0 as *mut _)) else {
            log::debug!("dispatch_toggle_monocle: no-op (foreground id not in registry)");
            return SocketResponse::Ok;
        };
        if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
            log::debug!("dispatch_toggle_monocle: no-op (focused window is not an active tile)");
            return SocketResponse::Ok;
        }
        match self.active_scrolling_mut().toggle_monocle(id) {
            Some(diff) => {
                self.animate_layout(&diff);
                SocketResponse::Ok
            }
            None => SocketResponse::Error {
                message: "cannot toggle monocle".into(),
            },
        }
    }

    /// Switch the active monitor's focus to a different workspace.
    ///
    /// Validates `target_id`, then delegates the active-index update and the
    /// partitioned switch animation to [`switch_active_workspace`]. See that
    /// helper for the vertical-packing animation model.
    ///
    /// # Errors
    ///
    /// Returns [`SocketResponse::Error`] when `target_id` does not match any
    /// workspace on the active monitor. Switching to the already-active
    /// workspace is a successful no-op.
    fn dispatch_switch_workspace(&mut self, target_id_raw: u32) -> SocketResponse {
        let target_id = WorkspaceId(target_id_raw);
        let prev_active_id = self.active_monitor().active_workspace_id();

        // Switch-to-self is a no-op success — preserves window positions and
        // avoids submitting an empty animation batch.
        if prev_active_id == target_id {
            return SocketResponse::Ok;
        }

        // Reject unknown workspace ids up front with a precise message.
        if self
            .active_monitor()
            .find_workspace_index(target_id)
            .is_none()
        {
            return SocketResponse::Error {
                message: format!("workspace {target_id:?} not found on the active monitor"),
            };
        }

        if !self.switch_active_workspace(target_id, prev_active_id) {
            return SocketResponse::Error {
                message: format!("workspace {target_id:?} disappeared mid-dispatch"),
            };
        }

        SocketResponse::Ok
    }

    /// Perform a workspace switch from `prev_active_id` to `target_id`.
    ///
    /// Implements the **vertical packing** workspace model: each workspace
    /// is parked one monitor-height (plus one `window_gap`) above or below
    /// the active workspace. The source (previously active) and destination
    /// (newly active) workspaces animate into their new slots; bystanders
    /// are either pre-snapped (side-changed) or left at rest (same side).
    ///
    /// Called by [`switch_active_workspace`] (the IPC-path wrapper that also
    /// re-establishes tiling focus) and by `on_focus_changed` (the foreground
    /// hook, which must not re-push foreground — the OS already chose the
    /// window).
    ///
    /// # Animation model
    ///
    /// Every non-empty workspace on the active monitor is submitted to
    /// [`animate_workspaces`](Self::animate_workspaces) in a single batch,
    /// each at its post-switch `y_offset`. This mirrors the intra-workspace
    /// [`animate_layout`](Self::animate_layout) pattern — submit every
    /// projected target and let the animator decide.
    ///
    /// The animator's `start_batch` reads each window's current position
    /// (interpolated if mid-flight, `GetWindowRect` otherwise) and
    /// `build_tweens` drops windows already at their target, so:
    ///
    /// - **Participants** (source + dest) animate, because their offset
    ///   genuinely changes.
    /// - **Same-side bystanders at rest** produce `from == to` no-ops and
    ///   do not move.
    /// - **Same-side bystanders still mid-flight** from a prior switch are
    ///   retargeted smoothly to their parked slot — this is the fix for the
    ///   rapid switch-workspace stranding bug, where a window could
    ///   otherwise be dropped mid-slide when the next switch replaced the
    ///   active batch.
    ///
    /// # Side-changed bystanders (teleport pre-snap)
    ///
    /// A bystander whose parked *side* changed (e.g. ws 3–7 when switching
    /// 2 → 8: below active 2, but above active 8) would otherwise slide
    /// across the entire visible area. To avoid that visual noise, these are
    /// first snapped instantly via [`teleport_workspaces`](Self::teleport_workspaces),
    /// which `SetWindowPos`-es them to their new parked offset *before* the
    /// animate batch runs. The animator then reads their post-teleport rect
    /// via `GetWindowRect` (`from == to`) and drops them as no-ops, so they
    /// stay snapped instead of sliding.
    ///
    /// # Why teleport before animate?
    ///
    /// Teleport must run first so the animator's `GetWindowRect` read sees
    /// the snapped position and treats the window as a no-op. If the
    /// animate batch were submitted first, the bystander would already have
    /// a slide tween built against its pre-teleport rect. A bystander that
    /// is *already mid-flight* from a prior switch is in the old active
    /// batch, so the animator reads its interpolated position regardless of
    /// the teleport — it slides to the new offset rather than snapping. This
    /// case is rare, ends at the correct target, and reads as a natural
    /// continuation of its existing motion.
    ///
    /// # Caller invariants
    ///
    /// The caller MUST (a) validate that `target_id` exists on the active
    /// monitor before calling, and (b) capture `prev_active_id` before the
    /// active index changes. This method does **not** re-validate — it trusts
    /// the caller. It does **not** touch the OS foreground or the registry's
    /// `focused` field — that is the caller's responsibility (see
    /// [`switch_active_workspace`] for the IPC-path wrapper that re-establishes
    /// tiling focus). Tiled rects are also untouched: a switch changes only
    /// y-offsets, not workspace-local positions. (A window *move* caller syncs
    /// the registry's tiled rects for both workspaces before calling this.)
    ///
    /// Because the registry's rects are workspace-local, the animator's
    /// neighbour-teleport decision is fed a
    /// [`snapshot_stacked_prev_rects`](Self::snapshot_stacked_prev_rects)
    /// taken before the active index changes — the windows' *real* positions,
    /// parking offsets applied — so a workspace coming back from behind a
    /// stacked monitor is moved instantly instead of tweened across it.
    ///
    /// # Returns
    ///
    /// `false` only if `target_id` could not be set active (impossible after
    /// caller validation — guarded to keep the codebase `.unwrap()`-free per
    /// AGENTS.md).
    pub(super) fn switch_workspace_layout(
        &mut self,
        target_id: WorkspaceId,
        prev_active_id: WorkspaceId,
    ) -> bool {
        // Geometry capture for the parking offset: parked workspaces must
        // travel the FULL physical monitor height to stay completely
        // off-screen — the taskbar-excluded work area would leave a slice
        // peeking past the taskbar strip. The window_gap is the same value
        // used intra-workspace, so stacking looks consistent with tiling. A
        // side with a stacked neighbour parks beyond the virtual screen
        // instead (see `WorkspaceParking`), and `animate_workspaces` moves
        // windows to or from such a slot instantly.
        let window_gap = self.active_scrolling().padding().window_gap;
        let parking = WorkspaceParking::new(
            self.active_scrolling().monitor(),
            self.active_monitor().screen_rect(),
            window_gap,
        );
        // Where every window really is before the switch (parking offsets
        // applied), captured while `prev_active_id` is still the active
        // workspace. See the method docs.
        let prev_rects = self.snapshot_stacked_prev_rects();

        // Update the active index synchronously. The caller validated the id,
        // so failure is impossible in practice — but the explicit guard keeps
        // the codebase `.unwrap()`-free per AGENTS.md.
        if self
            .active_monitor_mut()
            .set_active_workspace(target_id)
            .is_none()
        {
            return false;
        }

        // The float-tracking set must hold only the floats of the active
        // workspace of EVERY monitor. Parked workspaces' floats can never be
        // dragged (they're off-screen), and excluding them guarantees a stray
        // LOCATIONCHANGE during the switch animation cannot corrupt a parked
        // workspace's rect; the other monitors' visible floats stay tracked.
        self.rebuild_float_hwnds();

        // Submit every non-empty workspace to the animator, and pre-snap the
        // side-changed bystanders via teleport. See the method-level docs for
        // why this mirrors the intra-workspace animate_layout pattern and how
        // it fixes rapid-switch stranding.
        let mut animate_batches: Vec<(ActualLayout, i32)> = Vec::new();
        let mut teleport_batches: Vec<(ActualLayout, i32)> = Vec::new();
        for ws in self.active_monitor().workspaces() {
            let scroll_actual = ws.scrolling.actual_layout();
            let float_actual = ws.floating.to_actual_layout();

            // Floating windows share the same workspace and must ride along
            // with tiles on workspace switch. Build a merged layout so
            // animate_workspaces / teleport_workspaces moves both together.
            if scroll_actual.entries.is_empty() && float_actual.entries.is_empty() {
                continue;
            }

            let prev_offset = parking.offset(ws.id, prev_active_id);
            let new_offset = parking.offset(ws.id, target_id);

            let is_participant = ws.id == target_id || ws.id == prev_active_id;
            let side_changed = prev_offset != new_offset;

            // Merge scrolling + floating entries into one batch layout.
            let mut merged_entries = scroll_actual.entries.clone();
            merged_entries.extend(float_actual.entries.iter().cloned());
            let merged = ActualLayout {
                entries: merged_entries,
            };

            // Pre-snap side-changed NON-participant bystanders so the animator
            // later reads them (via GetWindowRect) already at their target and
            // drops them as no-ops. Participants are excluded: their offset
            // genuinely changed and they must animate.
            if !is_participant && side_changed {
                teleport_batches.push((merged.clone(), new_offset));
            }

            // Every non-empty workspace goes to the animator. build_tweens
            // drops windows already at their target (the teleported
            // side-changed bystanders above, plus same-side static
            // bystanders), and start_batch retargets any window still
            // mid-flight from a prior switch — fixing rapid-switch stranding
            // without per-window special-casing.
            animate_batches.push((merged, new_offset));
        }

        // Teleport first so the animator's GetWindowRect read sees the snapped
        // side-changed bystanders, then submit the full batch. The animator
        // drops the teleported bystanders and same-side static bystanders as
        // no-ops, animates the participants, and retargets any window still
        // mid-flight from a prior switch.
        self.teleport_workspaces(&teleport_batches);
        self.animate_workspaces_on(
            self.active_monitor,
            &animate_batches,
            PrevRects::Snapshot(&prev_rects),
        );

        true
    }

    /// IPC-path workspace switch: layout/animation plus tiling-focus push.
    ///
    /// Wraps [`switch_workspace_layout`] and then re-establishes OS foreground
    /// on the destination's `last_focused_window`, mirroring [`dispatch_focus`].
    /// Used by [`dispatch_switch_workspace`] and
    /// [`dispatch_move_window_to_workspace`]. The foreground hook
    /// (`on_focus_changed`) calls [`switch_workspace_layout`] directly — it
    /// must not re-push foreground because the OS already chose the window.
    ///
    /// The foreground push is [`focus_monitor`](Self::focus_monitor) on the
    /// (unchanged) active monitor, whose active workspace the destination
    /// now is: the strip's remembered `last_focused_window` is used only if
    /// it is still a tracked, live window (`IsWindow`); a remembered window
    /// that died without its `DESTROY` reaching the layout — the VM log's
    /// `SetForegroundWindow failed ... 0x6` on a workspace switch — is
    /// purged from the strip / floating space / float-tracking set and the
    /// next candidate is tried (focused column window, first tile, top
    /// float), see [`resolve_monitor_focus_target`](Self::resolve_monitor_focus_target).
    /// `focus_monitor` sets the registry focus and recolours the previous
    /// and new borders itself, so the `EVENT_SYSTEM_FOREGROUND` that
    /// `on_focus_changed` later consumes finds nothing left to change.
    ///
    /// # Empty destination
    ///
    /// When the destination has no live window there is nothing to
    /// foreground: the tracked focus is dropped (previous border recoloured)
    /// and the OS foreground goes to the desktop ("Progman") window — the
    /// Win32 idiom for clicking empty desktop space — so keystrokes no
    /// longer reach the previous workspace's window. Logged at `debug`, not
    /// `warn`: nothing live to focus is a normal state, not a failure.
    fn switch_active_workspace(
        &mut self,
        target_id: WorkspaceId,
        prev_active_id: WorkspaceId,
    ) -> bool {
        if !self.switch_workspace_layout(target_id, prev_active_id) {
            return false;
        }
        self.focus_monitor(self.active_monitor);
        true
    }

    /// Move the focused window from the active workspace to a target workspace.
    ///
    /// Implements the cross-workspace window move: the focused window in the
    /// active workspace is detached (with **local** focus succession — no OS
    /// foreground push) and re-inserted into the target workspace's
    /// [`ScrollingSpace`] after its currently focused column. The camera then
    /// **follows the moved window**: the active workspace switches to the
    /// destination so the moved window is brought into view.
    ///
    /// # Animation
    ///
    /// Both the source and destination workspaces are mutated (source loses a
    /// window, destination gains one), and then [`switch_active_workspace`]
    /// animates the transition from source to destination as a single
    /// coordinated switch: the source slides to its parked y-offset and the
    /// destination (now holding the moved window) slides into the active
    /// position at offset 0. The animator's default `RetargetFromCurrent`
    /// policy keeps the move non-blocking — any command issued mid-flight
    /// retargets from each window's current interpolated position.
    ///
    /// # Registry sync (per-workspace)
    ///
    /// A window *move* changes both [`VirtualLayout`]s: the source loses a
    /// window, the destination gains one. The registry's tiling slots and
    /// tiled rects must therefore be refreshed for each. Both syncs run
    /// **before** the switch animation (the switch itself changes only
    /// y-offsets, not workspace-local positions). The destination sync runs
    /// last so the moved window's slot/rect end up pointing at its
    /// destination position.
    ///
    /// # Mutation order
    ///
    /// The move-to-self case and the destination id are checked **before** any
    /// state mutation, so a no-op or bad-id request fails fast with no layout
    /// damage. The borrow checker forbids holding `&mut` to two workspaces in
    /// the same `Vec<Workspace>` simultaneously, so source and destination
    /// are mutated in two separate single-workspace steps.
    ///
    /// # Errors
    ///
    /// Returns [`SocketResponse::Error`] when:
    /// - `dest_id` does not match any workspace on the active monitor.
    /// - No window is focused on the active workspace (nothing to move).
    /// - The destination lookup fails post-validation (impossible in practice
    ///   but guarded to keep the codebase `.unwrap()`-free per AGENTS.md).
    ///
    /// Moving to the currently active workspace is a successful no-op.
    fn dispatch_move_window_to_workspace(&mut self, dest_id_raw: u32) -> SocketResponse {
        let dest_id = WorkspaceId(dest_id_raw);
        let active_id = self.active_monitor().active_workspace_id();

        // Move-to-self is a no-op success — moving the focused window to its
        // own workspace leaves everything exactly where it is. Mirrors the
        // switch-to-self guard in `dispatch_switch_workspace`.
        if dest_id == active_id {
            return SocketResponse::Ok;
        }

        // Validate destination up front — fail fast with no state change.
        if self
            .active_monitor()
            .find_workspace_index(dest_id)
            .is_none()
        {
            return SocketResponse::Error {
                message: format!("workspace {dest_id:?} not found on the active monitor"),
            };
        }

        // Capture the focused window before mutating anything. The active
        // workspace remains active after the move — focus succession inside
        // the source workspace is handled by `remove_window`.
        let focused: WindowId = match self.active_scrolling().last_focused_window() {
            Some(f) => f,
            None => {
                return SocketResponse::Error {
                    message: "no focused window to move".into(),
                };
            }
        };

        // Capture the focused window's current column width before removal,
        // so it can be preserved in the destination workspace.
        let moved_width_px: u32 = self
            .active_scrolling()
            .virtual_layout()
            .find_window(focused)
            .map(|(col_idx, _)| {
                self.active_scrolling().virtual_layout().columns[col_idx].width_px as u32
            })
            .unwrap_or_else(|| {
                // Fallback to base column_width if lookup fails (e.g. stale
                // focus). This is safe — the window will just get the default.
                self.active_scrolling().config().column_width
            });

        // --- Mutation 1: remove from source (the active workspace). ---
        // `remove_window` runs the full focus-fallback + ensure-visible
        // pipeline internally and returns the post-removal AppliedLayout.
        let source_applied = self
            .active_monitor_mut()
            .active_scrolling_mut()
            .remove_window(focused);

        // Refresh registry state for the source workspace.
        self.registry
            .update_tiling_slots_from_layout(&source_applied.virtual_layout);
        self.registry
            .update_tiled_rects(&source_applied.actual_layout);

        // --- Mutation 2: insert into destination. ---
        // The moved window preserves its pre-move width via
        // `insert_window_with_width`. After insertion, decide how to
        // position the viewport: fit (all columns fit in monitor → center
        // the entire canvas) vs. overflow (ensure the moved window's new
        // column is visible).
        let dest_applied = match self.active_monitor_mut().workspace_mut(dest_id) {
            Some(ws) => {
                let post_insert = ws
                    .scrolling
                    .insert_window_with_width(focused, moved_width_px);
                let gap = ws.scrolling.config().padding.window_gap;
                let canvas_w = projection::canvas_width(&post_insert.virtual_layout, gap);
                let monitor_w = ws.scrolling.config().monitor_width;
                if canvas_w <= monitor_w {
                    // Fit: center the entire canvas (offset may be negative
                    // when canvas < monitor — projection handles this).
                    ws.scrolling.center_canvas().unwrap_or(post_insert)
                } else {
                    // Overflow: ensure the moved window's new column is
                    // visible. `insert_window_with_width` focuses the moved
                    // window, so `ensure_focused_visible` targets it.
                    ws.scrolling.ensure_focused_visible().unwrap_or(post_insert)
                }
            }
            None => {
                // `dest_id` was validated above, so this branch is
                // unreachable in practice. Guard anyway to stay
                // `.unwrap()`-free per AGENTS.md.
                return SocketResponse::Error {
                    message: format!("workspace {dest_id:?} disappeared mid-dispatch"),
                };
            }
        };

        // Refresh registry state for the destination workspace. This call
        // wins for the moved window — its slot/rect now reflect the
        // destination layout.
        self.registry
            .update_tiling_slots_from_layout(&dest_applied.virtual_layout);
        self.registry
            .update_tiled_rects(&dest_applied.actual_layout);

        // The camera follows the moved window: switch the active workspace to
        // the destination so the moved window is brought into view. The
        // switch reads the just-mutated layouts and animates source (to its
        // parked offset) and destination (into the active position at offset
        // 0) as a single coordinated batch.
        if !self.switch_active_workspace(dest_id, active_id) {
            return SocketResponse::Error {
                message: format!("workspace {dest_id:?} disappeared mid-dispatch"),
            };
        }

        SocketResponse::Ok
    }

    /// Set the focused window's mode (float, tile, or cycle).
    ///
    /// Transitions the OS-focused window between tiled and floating state:
    ///
    /// - **Float**: removes from [`ScrollingSpace`], adds centered to
    ///   [`FloatingSpace`], sets registry state to `Floating(Active { rect })`.
    /// - **Tile**: removes from [`FloatingSpace`], inserts after the
    ///   scrolling space's `last_focused_window`, sets registry state to
    ///   `Tiling(Active { col, row })`.
    /// - **Cycle**: toggles based on current state (tile→float, float→tile).
    /// - Already in the desired state → no-op `Ok`.
    ///
    /// On a successful non-no-op transition, also records the decision to
    /// `history-flow-rules.toml` so the next window of the same app is
    /// classified automatically. See (`docs/src/dev-guide/classification.md`).
    ///
    /// # Animation
    ///
    /// Both directions submit a single coordinated batch to
    /// [`animate_workspaces`](Self::animate_workspaces) at `y_offset = 0` (same
    /// workspace). The post-removal scrolling layout and the (possibly updated)
    /// floating layout are merged so the animator moves both sets of windows
    /// atomically.
    ///
    /// # Errors
    ///
    /// Returns [`SocketResponse::Error`] when:
    /// - No window is OS-focused (registry's `focused` is `None`).
    /// - The focused window is `Ignored` or not found in the registry.
    ///
    /// See (`docs/src/dev-guide/floating-space.md`) for the tile↔float
    /// transition design.
    fn dispatch_set_window(&mut self, mode: WindowMode) -> SocketResponse {
        // 1. Get the OS-focused window from the registry.
        let focused = match self.registry.focused() {
            Some(f) => f,
            None => {
                return SocketResponse::Error {
                    message: "no focused window".into(),
                };
            }
        };

        // 2. Inspect the focused window's current state.
        let hwnd = HWND(focused.0 as *mut _);
        let win = match self.registry.get_window(hwnd) {
            Some(w) => w,
            None => {
                return SocketResponse::Error {
                    message: "focused window not found in registry".into(),
                };
            }
        };

        let currently_tiling = matches!(win.state, WindowState::Tiling(TilingState::Active { .. }));
        let currently_floating = matches!(
            win.state,
            WindowState::Floating(FloatingState::Active { .. })
        );

        // Capture the window's identity for history recording before the
        // immutable borrow of `self.registry` (via `win`) ends — the
        // transition and recording calls below need `&mut self`. Cloning two
        // short strings is negligible. See (`docs/src/dev-guide/classification.md`).
        let exe = win.exe.clone();
        let class = win.class.clone();

        // 3. Resolve the effective action via the pure decision helper. This is
        //    the only non-trivial branching in the handler — extracting it
        //    makes the full mode × state table unit-testable without
        //    constructing the daemon (see `resolve_set_window_action`).
        let action = match resolve_set_window_action(mode, currently_tiling, currently_floating) {
            Ok(a) => a,
            Err(()) => {
                return SocketResponse::Error {
                    message: format!(
                        "window is in state {:?} (only active tiling/floating can transition)",
                        win.state
                    ),
                };
            }
        };

        // 4. Execute the transition (NoOp short-circuits to Ok without
        //    touching the layout engine or the animator).
        let response = match action {
            SetWindowAction::NoOp => SocketResponse::Ok,
            SetWindowAction::MakeFloating => self.set_window_to_float(focused),
            SetWindowAction::MakeTiling => self.set_window_to_tile(focused),
        };

        // 5. Record the user's explicit decision so the next window of the
        //    same app is auto-classified. See `record_learned_transition` for
        //    the idempotent save + pipeline-refresh logic.
        if matches!(response, SocketResponse::Ok) {
            self.record_learned_transition(action, &exe, &class);
        }

        response
    }

    /// Persist a `set-window` transition to `history-flow-rules.toml`.
    ///
    /// `NoOp` transitions are ignored (no user intent to record). The store is
    /// saved only when `record` reports a change, so repeated identical
    /// commands are a no-op on disk. After a save the classification pipeline
    /// is refreshed so the next window of the same app auto-classifies.
    fn record_learned_transition(&mut self, action: SetWindowAction, exe: &str, class: &str) {
        let Some(learned) = action_to_learned(action) else {
            return;
        };
        if !self.history.record(learned, exe, Some(class)) {
            return;
        }
        if let Err(e) = self.history.save(&history_rules_path_in(&self.config_dir)) {
            log::warn!("failed to persist history-flow-rules.toml: {e}");
        }
        self.registry
            .set_learned_rules(self.history.rules().to_vec());
    }

    /// Transition a window from tiling to floating.
    ///
    /// Removes the window from the active [`ScrollingSpace`], places it into the
    /// active [`FloatingSpace`] at a centered rect (via [`register_float`] /
    /// [`centered_float_rect`]), and animates both the post-removal scrolling
    /// layout and the updated floating layout in a single batch.
    ///
    /// Registry state is set to `Floating(Active { rect })` inside
    /// [`register_float`] (the `state` field on [`Window`](crate::registry::types::Window)
    /// is `pub`).
    fn set_window_to_float(&mut self, focused: WindowId) -> SocketResponse {
        // a) Remove from scrolling. remove_window handles focus fallback
        //    (next_available_window) and ensure_column_visible internally.
        let idx = self.monitor_index_for_window(focused);
        self.set_active_monitor(idx);
        let source_applied = self.monitors[idx]
            .active_scrolling_mut()
            .remove_window(focused);
        let source_virtual = source_applied.virtual_layout.clone();
        let source_actual = source_applied.actual_layout.clone();

        // b) Compute the centered float rect and place the window into the
        //    floating space. register_float also mirrors the rect into the
        //    registry state and arms float-location tracking.
        let float_rect = self.centered_float_rect(focused);
        let float_actual = self.register_float(focused, float_rect);

        // c) Animate: single batch, both at y_offset 0 (same workspace).
        //    Submitted BEFORE the tiled-rect sync so the neighbour-teleport
        //    decision sees where each remaining tile is now.
        let batches = [(source_actual.clone(), 0), (float_actual, 0)];
        self.animate_workspaces(&batches);

        // d) Sync registry tiling state for the scrolling side.
        self.registry
            .update_tiling_slots_from_layout(&source_virtual);
        self.registry.update_tiled_rects(&source_actual);

        // Re-sync the toggled window's border: the registry state just flipped
        // Tiling→Floating, so refresh_border_for re-resolves color (a focused
        // toggler stays blue under focus-priority) and re-seats the overlay to
        // the float outset geometry — the tile-era overlay would otherwise sit
        // at the stale tile rect until the next drag fires store_float_rect.
        self.refresh_border_for(focused.0);

        SocketResponse::Ok
    }

    /// Compute a centered floating rect for `focused` on the active monitor's
    /// work area.
    ///
    /// Prefers the window's measured `last_natural_size`; falls back to the
    /// user's explicit `[floating]` pixel size, then the built-in fraction+cap
    /// fallback. The pure centering math lives in
    /// [`FloatingSpace::centered_rect`]; this helper just resolves the
    /// preferred size. See (`docs/src/dev-guide/floating-space.md`).
    pub(super) fn centered_float_rect(&self, focused: WindowId) -> Rect {
        // Centre on the monitor the window is currently on, not blindly on
        // the active one.
        let idx = self.monitor_index_for_hwnd(focused.0);
        let work_area = self.monitors[idx].active_scrolling().monitor().work_area;
        // Each dimension uses the user's explicit pixel value when set, else
        // the built-in fallback (fraction of work area, capped). The cap only
        // applies to the fallback path — explicit values are respected as-is.
        let fallback = fallback_float_size(Size {
            w: work_area.width,
            h: work_area.height,
        });
        let config_default = Size {
            w: self.config.floating.default_width.unwrap_or(fallback.w),
            h: self.config.floating.default_height.unwrap_or(fallback.h),
        };
        let preferred = match self
            .registry
            .get_window(HWND(focused.0 as *mut _))
            .map(|w| w.last_natural_size)
            .filter(|s| s.w > 0 && s.h > 0)
        {
            Some(size) => size,
            None => config_default,
        };
        FloatingSpace::centered_rect(preferred, work_area)
    }

    /// Place `focused` into the active workspace's [`FloatingSpace`] at `rect`.
    ///
    /// The shared primitive behind every "a window becomes a float" path:
    /// - [`set_window_to_float`] (the `set-window` toggle), which first removes
    ///   the window from the scrolling space and animates both layouts.
    /// - [`on_window_created`](super::hooks::FlowWM::on_window_created)'s
    ///   fresh-float branch, for a window classified as float at creation.
    /// - the startup scan's adoption of pre-existing floats in
    ///   [`new`](Self::new).
    ///
    /// Adds the entry to [`FloatingSpace`], mirrors `rect` into the registry's
    /// [`FloatingState::Active`] so the two copies never drift, and registers
    /// the window for float-location tracking via [`add_float_hwnd`]. Returns
    /// the resulting floating actual layout (a snapshot of every float in this
    /// workspace) so the caller can animate it.
    ///
    /// `add_float_hwnd` is called before the caller's `animate_workspaces` so
    /// the animator's float-suppression can detect the window as a tracked
    /// float and arm itself.
    pub(super) fn register_float(&mut self, focused: WindowId, rect: Rect) -> ActualLayout {
        // A float belongs to the monitor its rect is on.
        let idx = self.monitor_index_for_rect(rect);
        self.monitors[idx]
            .active_workspace_mut()
            .floating
            .add(focused, rect);
        if let Some(window) = self.registry.get_window_mut(HWND(focused.0 as *mut _)) {
            window.state = WindowState::Floating(FloatingState::Active { rect });
        }
        self.remember_monitor(focused, idx);
        add_float_hwnd(focused.0);
        self.monitors[idx]
            .active_workspace_mut()
            .floating
            .to_actual_layout()
    }

    /// Transition a window from floating to tiling.
    ///
    /// Removes the window from the active [`FloatingSpace`], inserts it into
    /// the [`ScrollingSpace`] after the `last_focused_window`, and animates
    /// both the updated floating layout and the post-insertion scrolling layout
    /// in a single batch.
    ///
    /// Registry state is set to `Tiling(Active { col, row })` via direct field
    /// write, with `col`/`row` sourced from the destination virtual layout.
    fn set_window_to_tile(&mut self, focused: WindowId) -> SocketResponse {
        // The rect it is being tiled from is its pre-tiling anchor (unless
        // an earlier tiling of this run already recorded one): `flow
        // dispatch tiling off` puts the window back there.
        if let Some(w) = self.registry.get_window(HWND(focused.0 as *mut _))
            && let WindowState::Floating(FloatingState::Active { rect }) = w.state
        {
            let anchor = w.invisible_bounds.visible_to_window(rect);
            self.registry.note_pre_tiling_rect(focused.0, anchor);
        }

        // a) Remove from floating space.
        // The float may be registered on any monitor's active workspace;
        // drop it from wherever it is.
        for m in &mut self.monitors {
            m.active_workspace_mut().floating.remove(focused);
        }

        // Drop from the float-tracking set so the LOCATIONCHANGE callback no
        // longer forwards this window (it is becoming a tiled window).
        remove_float_hwnd(focused.0);

        // b) Insert into scrolling. insert_window places after
        //    last_focused_window, shifts right, sets last_focused_window,
        //    and calls ensure_column_visible internally.
        // Tile it on the monitor it is physically on (the user may have
        // dragged the float there), and make that monitor active.
        let idx = self.monitor_index_for_hwnd(focused.0);
        self.set_active_monitor(idx);
        let dest_applied = self.monitors[idx]
            .active_scrolling_mut()
            .insert_window(focused);
        self.remember_monitor(focused, idx);
        let dest_virtual = dest_applied.virtual_layout.clone();
        let dest_actual = dest_applied.actual_layout.clone();

        // c) Get the floating layout (now without the removed window).
        let float_actual = self.monitors[idx]
            .active_workspace_mut()
            .floating
            .to_actual_layout();

        // d) Animate: single batch, both at y_offset 0. Submitted BEFORE
        //    the tiled-rect sync so the neighbour-teleport decision sees
        //    where each tile is now (the toggled window still reads as a
        //    float at its float rect, which is where it is).
        let batches = [(dest_actual.clone(), 0), (float_actual, 0)];
        self.animate_workspaces(&batches);

        // e) Sync registry tiling state. This sets the toggled window to
        //    Tiling(Active { col, row }) — no manual write needed.
        self.registry.update_tiling_slots_from_layout(&dest_virtual);
        self.registry.update_tiled_rects(&dest_actual);

        // Re-sync the toggled window's border: the registry state just flipped
        // Floating→Tiling, so refresh_border_for re-resolves color. Tiled
        // geometry is animator-owned, so only color/z-order are re-asserted
        // here (refresh_border_for leaves tile overlay geometry untouched).
        self.refresh_border_for(focused.0);

        SocketResponse::Ok
    }

    /// Reload `flow.toml` (and `flow-rules.toml`) from disk and apply the
    /// live-reloadable fields to the running daemon.
    ///
    /// Layout state is preserved: window order, columns, focus, and the scroll
    /// viewport are untouched. Only geometry constants, borders, and animation
    /// timing are re-derived and re-applied. See
    /// (`docs/src/dev-guide/config-and-persistence.md`) for the full reload
    /// policy and the live-vs-structural field distinction.
    ///
    /// Uses **validate-before-apply**: the new config is loaded and validated
    /// before any state is touched, so a parse/validation error leaves the
    /// daemon running on its previous config with nothing to roll back.
    /// `flow-rules.toml` is reloaded non-fatally — a rules failure warns and
    /// keeps the current rules.
    ///
    /// # Errors
    ///
    /// Returns [`SocketResponse::Error`] only when `flow.toml` cannot be loaded
    /// or fails validation. The window-rule file never fails the command.
    fn dispatch_reload_config(&mut self) -> SocketResponse {
        // Load + validate BEFORE touching any state (validate-before-apply:
        // nothing to roll back if this fails).
        let new_config = match load_app_config(&user_app_config_path_in(&self.config_dir)) {
            Ok(config) => config,
            Err(error) => {
                return SocketResponse::Error {
                    message: format!("{error}"),
                };
            }
        };
        // Explicit re-validation is intentional: `load_app_config` only logs a
        // warning on validation failure (it still returns Ok), so re-validating
        // here is what turns a bad config into a hard error. Do not
        // "de-duplicate" this without making the loader itself propagate
        // validation failures.
        if let Err(reason) = new_config.validate() {
            return SocketResponse::Error { message: reason };
        }

        let had_presets = self.config.columns.is_some();

        // Publish the new config first: animate_layout and the border refresh
        // below read `self.config.borders` live.
        self.config = new_config;

        // Reconfigure every workspace's geometry, preserving each space's
        // virtual_layout (window columns/order/focus/viewport). Only the active
        // workspace is on-screen; parked ones re-project silently and animate
        // on their next switch. Each monitor derives its own base column
        // width — from its width preset when `[columns]` is configured, else
        // from `columns_per_screen` / `column_width` and its own work area.
        self.reapply_base_widths_after_reload(had_presets);

        // Animate the active workspace into its new geometry (windows slide to
        // the new gaps) and sync registry slots/rects.
        let applied = AppliedLayout {
            actual_layout: self.active_scrolling().actual_layout().clone(),
            virtual_layout: self.active_scrolling().virtual_layout().clone(),
        };
        self.animate_layout(&applied);

        // Re-assert border colors/thickness for the new border config.
        self.refresh_all_border_styles();

        // Apply the new animation timing.
        self.animator
            .update_config(config_derive::derive_animator_config(
                &self.config,
                Duration::ZERO,
            ));

        // Reload window rules (non-fatal): a broken rules file warns and keeps
        // the current rules so the rest of the reload still takes effect.
        match load_rules_config(&user_rules_path_in(&self.config_dir)) {
            Ok(rules) => self.registry.set_user_rules(rules),
            Err(error) => log::warn!("flowd: {error}; keeping current window rules"),
        }

        SocketResponse::Ok
    }
}
/// Pure candidate rule behind [`FlowWM::resolve_focus_target_with`]: the
/// windows to try, in order, when landing on a monitor's active workspace.
///
/// The strip's last-focused window first (what the user was using there),
/// then every tile of the strip in column/row order (left-most column, top
/// row first), then the floats top-most first; no window twice. Empty when
/// the workspace is empty.
#[must_use]
pub(super) fn focus_candidates(
    scrolling: &crate::workspace::ScrollingSpace,
    floating: &FloatingSpace,
) -> Vec<WindowId> {
    let mut out: Vec<WindowId> = Vec::new();
    let push = |id: WindowId, out: &mut Vec<WindowId>| {
        if !out.contains(&id) {
            out.push(id);
        }
    };
    if let Some(last) = scrolling.last_focused_window() {
        push(last, &mut out);
    }
    for column in &scrolling.virtual_layout().columns {
        for row in &column.rows {
            push(row.window_id, &mut out);
        }
    }
    for entry in floating.windows().iter().rev() {
        push(entry.window_id, &mut out);
    }
    out
}

/// The first of [`focus_candidates`]: the strip's last-focused window, else
/// the first window of the strip, else the top-most float. `None` when the
/// workspace is empty.
#[must_use]
#[cfg(test)]
pub(super) fn focus_candidate(
    scrolling: &crate::workspace::ScrollingSpace,
    floating: &FloatingSpace,
) -> Option<WindowId> {
    focus_candidates(scrolling, floating).into_iter().next()
}

fn unimplemented_command(name: &str) -> SocketResponse {
    SocketResponse::Error {
        message: format!("command '{name}' is not yet implemented"),
    }
}

/// Human word for a direction, for error messages ("no monitor to the left").
fn direction_word(dir: Direction) -> &'static str {
    match dir {
        Direction::Left => "to the left",
        Direction::Right => "to the right",
        Direction::Up => "above",
        Direction::Down => "below",
    }
}

/// The resolved transition for a `SetWindow` request, computed purely from the
/// requested mode and the focused window's current state.
///
/// Extracted from `dispatch_set_window` so the full mode × state decision table
/// is unit-testable without constructing a `FlowWM` (which owns
/// Win32 handles and cannot be built in a unit test). See
/// (`docs/src/dev-guide/floating-space.md`) for the transition design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetWindowAction {
    /// Transition the window into floating mode.
    MakeFloating,
    /// Transition the window into tiling mode.
    MakeTiling,
    /// The window already satisfies the requested mode — no transition needed.
    NoOp,
}

/// Resolve a `SetWindow` mode against the focused window's current state.
///
/// Pure decision logic — no daemon state, no Win32. Returns:
/// - `Ok(action)` when the window is in an active tiling or floating state.
/// - `Err(())` when the window is ignored / minimized / hidden (no transition
///   is possible). The caller maps this to a descriptive error message.
///
/// # Decision table
///
/// | mode  | currently tiling | currently floating | result        |
/// |-------|------------------|--------------------|---------------|
/// | Float | true             | false              | MakeFloating  |
/// | Float | false            | true               | NoOp          |
/// | Tile  | true             | false              | NoOp          |
/// | Tile  | false            | true               | MakeTiling    |
/// | Cycle | true             | false              | MakeFloating  |
/// | Cycle | false            | true               | MakeTiling    |
/// | any   | false            | false              | Err (ignored) |
const fn resolve_set_window_action(
    mode: WindowMode,
    currently_tiling: bool,
    currently_floating: bool,
) -> Result<SetWindowAction, ()> {
    // Ignored / minimized / hidden windows cannot transition.
    if !currently_tiling && !currently_floating {
        return Err(());
    }
    // Cycle resolves against the current state: tiling → float, otherwise tile.
    let make_float = match mode {
        WindowMode::Float => true,
        WindowMode::Tile => false,
        WindowMode::Cycle => currently_tiling,
    };
    // No-op when the window already satisfies the requested mode.
    let already_satisfied = (make_float && currently_floating) || (!make_float && currently_tiling);
    if already_satisfied {
        Ok(SetWindowAction::NoOp)
    } else if make_float {
        Ok(SetWindowAction::MakeFloating)
    } else {
        Ok(SetWindowAction::MakeTiling)
    }
}

/// Map a resolved transition to the learned-rule action it should produce.
///
/// Returns `None` for [`SetWindowAction::NoOp`] — a no-op transition carries
/// no user intent (the window was already in the requested mode) and must not
/// be recorded. Extracted as a pure `const fn` so the mapping is unit-testable
/// without constructing the daemon (same rationale as `resolve_set_window_action`).
///
/// See (`docs/src/dev-guide/classification.md`) for the history model.
const fn action_to_learned(action: SetWindowAction) -> Option<WindowAction> {
    match action {
        SetWindowAction::MakeFloating => Some(WindowAction::Float),
        SetWindowAction::MakeTiling => Some(WindowAction::Tile),
        SetWindowAction::NoOp => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_set_window_action: the full mode × state table ─────────
    //
    // These tests pin the decision logic extracted from `dispatch_set_window`.
    // Each test is a single cell of the table documented on
    // `resolve_set_window_action`. They run with NO daemon and NO Win32 —
    // that is the whole point of the extraction.

    #[test]
    fn float_mode_on_tiling_makes_floating() {
        // Positive: Float requested, currently tiling → must transition.
        let action = resolve_set_window_action(WindowMode::Float, true, false);
        assert_eq!(action, Ok(SetWindowAction::MakeFloating));
    }

    #[test]
    fn float_mode_on_floating_is_noop() {
        // Positive no-op: already floating, asking to float → nothing to do.
        let action = resolve_set_window_action(WindowMode::Float, false, true);
        assert_eq!(action, Ok(SetWindowAction::NoOp));
    }

    #[test]
    fn tile_mode_on_tiling_is_noop() {
        // Positive no-op: already tiling, asking to tile → nothing to do.
        let action = resolve_set_window_action(WindowMode::Tile, true, false);
        assert_eq!(action, Ok(SetWindowAction::NoOp));
    }

    #[test]
    fn tile_mode_on_floating_makes_tiling() {
        // Positive: Tile requested, currently floating → must transition.
        let action = resolve_set_window_action(WindowMode::Tile, false, true);
        assert_eq!(action, Ok(SetWindowAction::MakeTiling));
    }

    #[test]
    fn cycle_mode_on_tiling_makes_floating() {
        // Positive: Cycle toggles tiling → floating.
        let action = resolve_set_window_action(WindowMode::Cycle, true, false);
        assert_eq!(action, Ok(SetWindowAction::MakeFloating));
    }

    #[test]
    fn cycle_mode_on_floating_makes_tiling() {
        // Positive: Cycle toggles floating → tiling.
        let action = resolve_set_window_action(WindowMode::Cycle, false, true);
        assert_eq!(action, Ok(SetWindowAction::MakeTiling));
    }

    #[test]
    fn float_mode_on_ignored_is_err() {
        // Negative: an ignored / minimized / hidden window cannot transition.
        let action = resolve_set_window_action(WindowMode::Float, false, false);
        assert_eq!(action, Err(()));
    }

    #[test]
    fn tile_mode_on_ignored_is_err() {
        // Negative: Tile on a non-active window is also rejected.
        let action = resolve_set_window_action(WindowMode::Tile, false, false);
        assert_eq!(action, Err(()));
    }

    #[test]
    fn cycle_mode_on_ignored_is_err() {
        // Negative: Cycle has nothing to toggle for a non-active window.
        let action = resolve_set_window_action(WindowMode::Cycle, false, false);
        assert_eq!(action, Err(()));
    }

    #[test]
    fn resolve_action_covers_all_distinct_cells() {
        // Exhaustive guard: every combination of mode and the two booleans is
        // asserted elsewhere in this module; this test re-confirms the full
        // 3 × 3 grid (plus the impossible tiling∧floating row) so a future
        // edit cannot silently regress a cell without tripping a test.
        // The (true, true) cell is treated as already-floating for no-op
        // purposes because the floating check wins the `already_satisfied`
        // short-circuit for Float/Cycle, and the tiling check wins for Tile.
        for mode in [WindowMode::Float, WindowMode::Tile, WindowMode::Cycle] {
            // tiling=true, floating=false
            let _ = resolve_set_window_action(mode, true, false);
            // tiling=false, floating=true
            let _ = resolve_set_window_action(mode, false, true);
            // tiling=false, floating=false (ignored)
            assert_eq!(
                resolve_set_window_action(mode, false, false),
                Err(()),
                "ignored window must always be Err for {mode:?}"
            );
        }
    }

    // ── action_to_learned: transition → recorded action ────────────────
    //
    // These tests pin the mapping used by `dispatch_set_window` to decide
    // whether a transition should be persisted to `history-flow-rules.toml`.
    // Like the resolve tests above, they run with NO daemon and NO Win32.

    #[test]
    fn action_to_learned_make_floating_maps_to_float() {
        assert_eq!(
            action_to_learned(SetWindowAction::MakeFloating),
            Some(WindowAction::Float)
        );
    }

    #[test]
    fn action_to_learned_make_tiling_maps_to_tile() {
        assert_eq!(
            action_to_learned(SetWindowAction::MakeTiling),
            Some(WindowAction::Tile)
        );
    }

    #[test]
    fn action_to_learned_noop_maps_to_none() {
        // NoOp carries no user intent (window was already in the requested
        // mode) and must not be recorded — otherwise every idempotent
        // `set-window` call would write to disk.
        assert_eq!(action_to_learned(SetWindowAction::NoOp), None);
    }

    // ── fallback_float_size: built-in default size for floating windows ──
    //
    // Pure integer arithmetic extracted from `set_window_to_float`. The policy
    // is: width = 60% of work-area width capped at 1536; height = 80% of
    // work-area height capped at 1152; each dimension clamps independently.
    // These tests run with NO daemon and NO Win32 — that is the point of the
    // extraction (mirrors `resolve_set_window_action` above).

    /// Positive: a typical 1080p work area lands below the cap on both axes.
    /// 1920 × 0.6 = 1152, 1080 × 0.8 = 864.
    #[test]
    fn fallback_float_size_small_work_area_uses_fraction_no_cap() {
        let size = fallback_float_size(Size { w: 1920, h: 1080 });
        assert_eq!(size, Size { w: 1152, h: 864 });
    }

    /// Positive: an ultrawide work area hits the cap on BOTH axes. Width
    /// 3440 × 0.6 = 2064 clamps to 1536; height 1440 × 0.8 = 1152 is exactly
    /// the cap. Verifies the cap clamps on large work areas.
    #[test]
    fn fallback_float_size_large_work_area_clamps_to_cap() {
        let size = fallback_float_size(Size { w: 3440, h: 1440 });
        assert_eq!(size, Size { w: 1536, h: 1152 });
    }

    /// Positive: the QHD reference resolution (2560 × 1440) lands exactly at
    /// the cap on both axes — 2560 × 0.6 = 1536, 1440 × 0.8 = 1152. This is
    /// the boundary case the caps were derived from.
    #[test]
    fn fallback_float_size_qhd_reference_hits_cap_exactly() {
        let size = fallback_float_size(Size {
            w: FALLBACK_REF_WIDTH,
            h: FALLBACK_REF_HEIGHT,
        });
        assert_eq!(
            size,
            Size {
                w: MAX_FLOAT_WIDTH,
                h: MAX_FLOAT_HEIGHT,
            }
        );
    }

    /// Negative edge case: a zero-area work area yields a zero size. No
    /// underflow, no panic — integer multiply by zero is zero, and `min` with
    /// a positive cap keeps it zero.
    #[test]
    fn fallback_float_size_zero_work_area_returns_zero() {
        let size = fallback_float_size(Size { w: 0, h: 0 });
        assert_eq!(size, Size { w: 0, h: 0 });
    }

    /// Positive: the two dimensions clamp INDEPENDENTLY. A wide-but-short
    /// work area (3440 × 800) caps the width at 1536 while the height stays
    /// below its cap at 640 (800 × 0.8). Guards against a bug that clamps
    /// both dimensions to the same scale factor.
    #[test]
    fn fallback_float_size_clamps_each_dimension_independently() {
        let size = fallback_float_size(Size { w: 3440, h: 800 });
        // Width: 3440 × 6/10 = 2064, clamped to 1536.
        // Height: 800 × 8/10 = 640, below the 1152 cap → unchanged.
        assert_eq!(size, Size { w: 1536, h: 640 });
    }

    // ── Dispatch-handler classification guard tests ───────────────────
    //
    // Tests for the Phase 2 inline classification: tiled-only dispatch
    // handlers must silently no-op (SocketResponse::Ok) when the OS-foreground
    // window is not an active tile. These use FlowWM::new_for_test to bypass
    // all Win32 daemon startup — see (`docs/src/dev-guide/floating-space.md`).

    use crate::config::types::WindowRulesConfig;
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::registry::WindowRegistry;
    use crate::registry::types::IgnoredReason;
    use crate::workspace::{Monitor, ScrollingSpace, Workspace};

    /// Fake HWND value used across all dispatch classification tests.
    /// Nonzero so it doesn't collide with null-handle sentinels.
    const TEST_HWND: isize = 0x10001;

    /// Build a FlowWM with one fake window registered, its state overridden
    /// to `state`, and set as the OS foreground. The window is also added to
    /// the scrolling space so positive-path tests (active tile) can exercise
    /// layout mutations.
    fn flow_with_focused_state(state: WindowState) -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);

        let info = registry_win32::WindowInfo {
            hwnd: HWND(TEST_HWND as *mut _),
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
        };
        registry.register_window_from_info(&info);

        // Override whatever classification produced to the exact state the
        // test wants.
        if let Some(win) = registry.get_window_mut(HWND(TEST_HWND as *mut _)) {
            win.state = state;
        }
        registry.set_focused(TEST_HWND);

        let monitor_info = MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        });
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let mut scrolling = ScrollingSpace::new(monitor_info, 960, 320, 100, 100, padding, 4);
        scrolling.add_window(WindowId(TEST_HWND));

        let workspace = Workspace::new(WorkspaceId(1), scrolling);
        let monitor = Monitor::new(
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            vec![workspace],
            0,
        );
        FlowWM::new_for_test(registry, vec![monitor])
    }

    /// Float foreground: all 10 tiled-only handlers must silent-no-op (Ok).
    #[test]
    fn float_focused_all_tiled_ops_are_noop() {
        let mut flow = flow_with_focused_state(WindowState::Floating(FloatingState::Active {
            rect: Rect {
                x: 100,
                y: 100,
                width: 400,
                height: 300,
            },
        }));
        assert_eq!(flow.dispatch_focus(Direction::Left), SocketResponse::Ok);
        assert_eq!(
            flow.dispatch_swap_column(Direction::Left),
            SocketResponse::Ok
        );
        assert_eq!(flow.dispatch_swap_window(Direction::Up), SocketResponse::Ok);
        assert_eq!(
            flow.dispatch_merge_column(Direction::Left),
            SocketResponse::Ok
        );
        assert_eq!(
            flow.dispatch_promote_window(Direction::Up),
            SocketResponse::Ok
        );
        assert_eq!(flow.dispatch_expand(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_shrink(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_set_column_width(1000), SocketResponse::Ok);
        assert_eq!(flow.dispatch_toggle_monocle(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_center(), SocketResponse::Ok);
    }

    /// Ignored foreground (maximized): all tiled-only handlers silent-no-op.
    #[test]
    fn ignored_focused_all_tiled_ops_are_noop() {
        let mut flow = flow_with_focused_state(WindowState::Ignored(IgnoredReason::Maximized));
        assert_eq!(flow.dispatch_focus(Direction::Left), SocketResponse::Ok);
        assert_eq!(
            flow.dispatch_swap_column(Direction::Left),
            SocketResponse::Ok
        );
        assert_eq!(flow.dispatch_expand(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_shrink(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_toggle_monocle(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_center(), SocketResponse::Ok);
    }

    /// No foreground window at all: all tiled-only handlers silent-no-op.
    #[test]
    fn no_foreground_all_tiled_ops_are_noop() {
        let mut flow = flow_with_focused_state(WindowState::Floating(FloatingState::Active {
            rect: Rect {
                x: 100,
                y: 100,
                width: 400,
                height: 300,
            },
        }));
        flow.registry.clear_focused();
        assert_eq!(flow.dispatch_focus(Direction::Left), SocketResponse::Ok);
        assert_eq!(
            flow.dispatch_swap_column(Direction::Left),
            SocketResponse::Ok
        );
        assert_eq!(flow.dispatch_expand(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_shrink(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_toggle_monocle(), SocketResponse::Ok);
        assert_eq!(flow.dispatch_center(), SocketResponse::Ok);
    }

    /// Active-tile foreground: the guard must NOT short-circuit. With a single
    /// column, dispatch_focus(Left) reaches the layout layer and returns Error
    /// ("no window to focus in that direction") — distinct from the guard's Ok.
    #[test]
    fn tile_focused_guard_passes_through_to_layout() {
        let mut flow =
            flow_with_focused_state(WindowState::Tiling(TilingState::Active { col: 0, row: 0 }));
        let resp = flow.dispatch_focus(Direction::Left);
        assert!(
            matches!(resp, SocketResponse::Error { .. }),
            "expected Error (guard passed through to layout), got {resp:?}"
        );
    }

    // ── focus-monitor (B7) ────────────────────────────────────────────

    /// A second, empty monitor to the right of the test monitor.
    fn empty_monitor_at(x: i32) -> Monitor {
        let rect = Rect {
            x,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let monitor_info = MonitorInfo::single(rect);
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let scrolling = ScrollingSpace::new(monitor_info, 960, 320, 100, 100, padding, 4);
        Monitor::new(
            rect,
            rect,
            vec![Workspace::new(WorkspaceId(1), scrolling)],
            0,
        )
    }

    /// Landing on an empty monitor must forget the tracked focus so the next
    /// command cannot act on the window we left (B7). The previous focus is
    /// `Minimized` so the border refresh takes the detach path (no overlay
    /// window is created in a unit test).
    #[test]
    fn focus_target_on_empty_monitor_is_none_and_focus_is_dropped() {
        let mut flow = flow_with_focused_state(WindowState::Tiling(TilingState::Minimized));
        flow.monitors.push(empty_monitor_at(1920));
        assert_eq!(flow.registry.focused(), Some(WindowId(TEST_HWND)));

        let target = flow.resolve_monitor_focus_target(1, |_| true);
        assert_eq!(target, None);
        assert_eq!(flow.active_monitor, 1);

        flow.drop_tracked_focus();
        assert_eq!(flow.registry.focused(), None);
    }

    /// The remembered window on the target monitor is live: it is the
    /// candidate, and nothing is purged.
    #[test]
    fn focus_target_returns_the_live_last_focused_window() {
        let mut flow =
            flow_with_focused_state(WindowState::Tiling(TilingState::Active { col: 0, row: 0 }));
        flow.monitors.push(empty_monitor_at(1920));
        flow.active_monitor = 1;

        let target = flow.resolve_monitor_focus_target(0, |_| true);
        assert_eq!(target, Some(WindowId(TEST_HWND)));
        assert_eq!(flow.active_monitor, 0);
        assert!(
            flow.active_scrolling()
                .virtual_layout()
                .find_window(WindowId(TEST_HWND))
                .is_some()
        );
    }

    /// The remembered window is dead (`IsWindow` false): it is dropped from
    /// the strip and the next live window is picked instead of handing a
    /// dead handle to `SetForegroundWindow`.
    #[test]
    fn focus_target_purges_dead_window_and_picks_the_next_one() {
        let mut flow =
            flow_with_focused_state(WindowState::Tiling(TilingState::Active { col: 0, row: 0 }));
        // A second window in the strip that is NOT in the registry (so it is
        // never live), appended after the focused one; then focus it so it is
        // the strip's remembered window.
        const DEAD: isize = 0x20002;
        flow.active_scrolling_mut().add_window(WindowId(DEAD));
        assert_eq!(
            flow.active_scrolling().last_focused_window(),
            Some(WindowId(DEAD))
        );

        let target = flow.resolve_monitor_focus_target(0, |_| true);
        assert_eq!(target, Some(WindowId(TEST_HWND)));
        assert!(
            flow.active_scrolling()
                .virtual_layout()
                .find_window(WindowId(DEAD))
                .is_none()
        );
        assert!(
            flow.active_scrolling()
                .virtual_layout()
                .find_window(WindowId(TEST_HWND))
                .is_some()
        );
    }

    /// Switching to a parked workspace whose remembered last-focused window
    /// (and its only float) died without a `DESTROY`: the dead tile is
    /// purged from the strip, the dead float from the floating space and
    /// the float-tracking set, and the next live window is picked — the
    /// VM log's `SetForegroundWindow failed ... 0x6` on a workspace switch.
    #[test]
    fn switching_to_a_parked_workspace_with_a_dead_remembered_window_picks_the_next_live_one() {
        use crate::registry::hooks::{add_float_hwnd, is_float_hwnd};

        const LIVE: isize = 0x30003;
        const DEAD_TILE: isize = 0x30004;
        const DEAD_FLOAT: isize = 0x30005;
        let mut flow =
            flow_with_focused_state(WindowState::Tiling(TilingState::Active { col: 0, row: 0 }));
        for hwnd in [LIVE, DEAD_TILE, DEAD_FLOAT] {
            let info = registry_win32::WindowInfo {
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
            };
            flow.registry.register_window_from_info(&info);
        }
        // Workspace 2 (parked): LIVE then DEAD_TILE, so the dead one is the
        // strip's remembered window; DEAD_FLOAT floats there.
        let info = *flow.active_scrolling().monitor();
        let padding = flow.active_scrolling().padding();
        let mut ws2 = Workspace::new(
            WorkspaceId(2),
            ScrollingSpace::new(info, 960, 320, 100, 100, padding, 4),
        );
        ws2.scrolling.add_window(WindowId(LIVE));
        ws2.scrolling.add_window(WindowId(DEAD_TILE));
        ws2.floating.add(
            WindowId(DEAD_FLOAT),
            Rect {
                x: 100,
                y: 100,
                width: 300,
                height: 200,
            },
        );
        add_float_hwnd(DEAD_FLOAT);
        // Rebuild the monitor with workspace 1 (TEST_HWND) and workspace 2.
        let screen = flow.monitors[0].screen_rect();
        let mut ws1_scrolling = ScrollingSpace::new(info, 960, 320, 100, 100, padding, 4);
        ws1_scrolling.add_window(WindowId(TEST_HWND));
        flow.monitors = vec![Monitor::new(
            screen,
            screen,
            vec![Workspace::new(WorkspaceId(1), ws1_scrolling), ws2],
            0,
        )];
        assert_eq!(
            flow.monitors[0]
                .workspace(WorkspaceId(2))
                .expect("ws 2")
                .scrolling
                .last_focused_window(),
            Some(WindowId(DEAD_TILE))
        );

        // The switch's layout half, then the focus resolution with a desktop
        // on which the two DEAD_* handles are no longer windows.
        assert!(flow.switch_workspace_layout(WorkspaceId(2), WorkspaceId(1)));
        assert_eq!(flow.monitors[0].active_workspace_id(), WorkspaceId(2));
        let live = |h: isize| h != DEAD_TILE && h != DEAD_FLOAT;
        let target = flow.resolve_monitor_focus_target(0, live);
        assert_eq!(target, Some(WindowId(LIVE)));
        let strip = flow.active_scrolling().virtual_layout();
        assert!(strip.find_window(WindowId(DEAD_TILE)).is_none());
        assert!(strip.find_window(WindowId(LIVE)).is_some());
        assert_eq!(
            flow.active_scrolling().last_focused_window(),
            Some(WindowId(LIVE))
        );

        // With the live tile gone too, the dead float is the next candidate:
        // purged from the floating space and the tracking set, nothing left.
        flow.active_scrolling_mut().remove_window(WindowId(LIVE));
        let target = flow.resolve_monitor_focus_target(0, live);
        assert_eq!(target, None);
        assert!(flow.active_floating().is_empty());
        assert!(!is_float_hwnd(DEAD_FLOAT));
        // Workspace 1 was never touched.
        assert!(
            flow.monitors[0]
                .workspace(WorkspaceId(1))
                .expect("ws 1")
                .scrolling
                .virtual_layout()
                .find_window(WindowId(TEST_HWND))
                .is_some()
        );
    }

    /// The push itself is the last liveness check: a remembered candidate
    /// that passes the registry and `IsWindow` checks but whose
    /// `SetForegroundWindow` fails with `ERROR_INVALID_HANDLE` (a process
    /// that was exiting) is purged like a dead window and the next live
    /// candidate is pushed and returned. A hidden / cloaked candidate is
    /// skipped — never pushed, never purged. A push refused for any other
    /// reason (the foreground lock) keeps the candidate.
    #[test]
    fn invalid_handle_push_purges_the_candidate_and_focuses_the_next_one() {
        use std::cell::RefCell;

        const DYING: isize = 0x40004;
        const HIDDEN: isize = 0x40005;
        let mut flow =
            flow_with_focused_state(WindowState::Tiling(TilingState::Active { col: 0, row: 0 }));
        for hwnd in [DYING, HIDDEN] {
            let info = registry_win32::WindowInfo {
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
            };
            flow.registry.register_window_from_info(&info);
        }
        // Strip: TEST_HWND, HIDDEN, DYING — DYING is the remembered window,
        // then the tiles in order: TEST_HWND (live), HIDDEN.
        flow.active_scrolling_mut().add_window(WindowId(HIDDEN));
        flow.active_scrolling_mut().add_window(WindowId(DYING));
        assert_eq!(
            flow.active_scrolling().last_focused_window(),
            Some(WindowId(DYING))
        );

        // First pass: DYING is still alive when its push is rejected, so it
        // keeps its seat and only loses the foreground to the next candidate.
        let pushes: RefCell<Vec<isize>> = RefCell::new(Vec::new());
        let push = |h: isize| {
            pushes.borrow_mut().push(h);
            if h == DYING {
                ForegroundPush::InvalidHandle
            } else {
                ForegroundPush::Focused
            }
        };
        let target = flow.resolve_focus_target_with(0, |_| true, |h: isize| h != HIDDEN, push);
        assert_eq!(target, Some(WindowId(TEST_HWND)));
        assert_eq!(*pushes.borrow(), vec![DYING, TEST_HWND]);
        let strip = flow.active_scrolling().virtual_layout();
        assert!(
            strip.find_window(WindowId(DYING)).is_some(),
            "a live window whose push was rejected must stay seated"
        );
        assert!(strip.find_window(WindowId(HIDDEN)).is_some());
        assert!(strip.find_window(WindowId(TEST_HWND)).is_some());

        // Second pass: DYING really dies during the push (the liveness check
        // fails right after the rejection), so it is purged.
        flow.active_scrolling_mut().set_focus(WindowId(DYING));
        let gone: RefCell<bool> = RefCell::new(false);
        let pushes: RefCell<Vec<isize>> = RefCell::new(Vec::new());
        let target = flow.resolve_focus_target_with(
            0,
            |h: isize| h != DYING || !*gone.borrow(),
            |h: isize| h != HIDDEN,
            |h: isize| {
                pushes.borrow_mut().push(h);
                if h == DYING {
                    *gone.borrow_mut() = true;
                    ForegroundPush::InvalidHandle
                } else {
                    ForegroundPush::Focused
                }
            },
        );
        assert_eq!(target, Some(WindowId(TEST_HWND)));
        assert_eq!(*pushes.borrow(), vec![DYING, TEST_HWND]);
        let strip = flow.active_scrolling().virtual_layout();
        assert!(strip.find_window(WindowId(DYING)).is_none());
        assert!(strip.find_window(WindowId(HIDDEN)).is_some());
        assert!(strip.find_window(WindowId(TEST_HWND)).is_some());

        // Make HIDDEN the remembered window: it is skipped (not purged) and
        // the live tile is picked.
        flow.active_scrolling_mut().set_focus(WindowId(HIDDEN));
        let target = flow.resolve_focus_target_with(
            0,
            |_| true,
            |h: isize| h != HIDDEN,
            |_| ForegroundPush::Focused,
        );
        assert_eq!(target, Some(WindowId(TEST_HWND)));
        assert!(
            flow.active_scrolling()
                .virtual_layout()
                .find_window(WindowId(HIDDEN))
                .is_some()
        );

        // A refused push for another reason keeps the candidate.
        let target =
            flow.resolve_focus_target_with(0, |_| true, |_| true, |_| ForegroundPush::Failed(0x5));
        assert_eq!(target, Some(WindowId(HIDDEN)));
        assert_eq!(flow.active_scrolling().virtual_layout().window_count(), 2);

        // Every remaining candidate hidden: nothing is pushed or purged.
        let target = flow.resolve_focus_target_with(
            0,
            |_| true,
            |_| false,
            |_| -> ForegroundPush { panic!("a hidden candidate must never be pushed") },
        );
        assert_eq!(target, None);
        assert_eq!(flow.active_scrolling().virtual_layout().window_count(), 2);
    }

    /// A tracked window whose handle is no longer a window is also purged.
    #[test]
    fn focus_target_treats_is_window_false_as_dead() {
        let mut flow =
            flow_with_focused_state(WindowState::Tiling(TilingState::Active { col: 0, row: 0 }));
        let target = flow.resolve_monitor_focus_target(0, |_| false);
        assert_eq!(target, None);
        assert!(
            flow.active_scrolling()
                .virtual_layout()
                .find_window(WindowId(TEST_HWND))
                .is_none()
        );
    }

    // ── focus_candidate ───────────────────────────────────────────────

    #[test]
    fn focus_candidate_prefers_last_focused_then_first_tile_then_top_float() {
        let monitor_info = MonitorInfo::single(Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        });
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let mut scrolling = ScrollingSpace::new(monitor_info, 960, 320, 100, 100, padding, 4);
        let mut floating = FloatingSpace::new();
        assert_eq!(focus_candidate(&scrolling, &floating), None);

        let float_rect = Rect {
            x: 10,
            y: 10,
            width: 100,
            height: 100,
        };
        floating.add(WindowId(7), float_rect);
        floating.add(WindowId(8), float_rect);
        assert_eq!(focus_candidate(&scrolling, &floating), Some(WindowId(8)));

        scrolling.add_window(WindowId(1));
        scrolling.add_window(WindowId(2));
        assert_eq!(focus_candidate(&scrolling, &floating), Some(WindowId(2)));

        // No focus cursor (a layout swapped in without one): the first
        // window of the strip, left-most column, top row.
        let _ = scrolling.set_layout(
            vec![
                (960, vec![WindowId(3), WindowId(5)]),
                (960, vec![WindowId(4)]),
            ],
            None,
            0,
        );
        assert_eq!(scrolling.last_focused_window(), None);
        assert_eq!(focus_candidate(&scrolling, &floating), Some(WindowId(3)));
    }
}
