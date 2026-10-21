//! Loadout save/restore orchestration.
//!
//! This module implements the daemon-side logic for serializing the current
//! workspace arrangement to a versioned JSON file (save) and resolving a
//! saved file back onto live windows (load). Both paths are wired through
//! [`FlowWM::dispatch`] and share a single save code path so that
//! `flow loadout save` and the save-on-stop hook behave identically.
//!
//! # Window identity: HWND-exact
//!
//! The matcher keys each saved slot to its stored `HWND` (see [`WindowRef`]) —
//! an exact, unambiguous lookup with no scoring or tie-breaking. `exe`/`title`
//! ride along as diagnostic-only fields so a failed restore can name the
//! missing window. The rationale for HWND-exact matching (and the rejected
//! fuzzy alternative) is in the dev guide (`docs/src/dev-guide/loadout.md`).
//!
//! # Save algorithm
//!
//! Walk every monitor → workspace → join the virtual layout (columns, rows,
//! viewport offset, focus) with per-window registry metadata (HWND, exe,
//! title). Swap `WindowId` for [`WindowRef`]. Serialize as JSON.
//!
//! # Load algorithm (no-partial guarantee)
//!
//! 1. Parse the file; reject on a non-current `version`.
//! 2. Collect the set of live managed windows' `HWND`s (skipping `Ignored`).
//! 3. **Phase B (resolve, no mutation):** for each loadout slot, pop its
//!    stored `HWND` from the live set. If any slot's `HWND` is not live →
//!    abort with a diagnostic error naming `exe`/`title`, zero state touched.
//! 4. **Purge:** every window the file references is removed from every
//!    workspace on every monitor, so a window can never end up seated in two
//!    spaces when the file and the live desktop disagree about monitors.
//! 5. **Phase D (apply):** per-workspace `set_layout` + float adds, then
//!    sync registry slots/rects and the owning-monitor memory. A snapshot
//!    whose monitor index is beyond the attached monitors is *merged* into
//!    the fallback monitor's workspace (columns appended, floats clamped into
//!    its work area) instead of overwriting it.
//! 6. Leftover tiling windows (still live but unreferenced by the loadout
//!    and seated nowhere) are appended as new columns on the active workspace.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::common::{Rect, WindowId};
use crate::ipc::message::SocketResponse;
use crate::layout::types::VirtualLayout;
use crate::layout::types::{ActualLayout, AppliedLayout};
use crate::loadout::{
    ColumnSnapshot, FloatingEntry, LoadoutFile, MonitorSnapshot, RectJson, RowSnapshot,
    ScrollingSnapshot, WindowRef, WorkspaceSnapshot,
};
use crate::registry::types::{FloatingState, TilingState, WindowState};
use crate::workspace::{Workspace, WorkspaceId, WorkspaceParking};
use windows::Win32::Foundation::HWND;

use super::animation::PrevRects;
use super::types::FlowWM;

// ── Pure helpers (unit-testable without FlowWM) ────────────────────

/// Convert a [`RectJson`] (loadout file format, `w`/`h` fields) to a
/// [`Rect`] (project-wide format, `width`/`height` fields).
#[must_use]
fn rect_json_to_rect(rj: &RectJson) -> Rect {
    Rect {
        x: rj.x,
        y: rj.y,
        width: rj.w,
        height: rj.h,
    }
}

/// Resolve a loadout slot's stored [`WindowRef`] against the live set.
///
/// On a hit, removes the slot's `HWND` from `live` (so each live window is
/// claimed by at most one slot) and returns the matching [`WindowId`]. On a
/// miss, returns an error naming the slot's diagnostic `exe`/`title` so a
/// failed restore can identify which application did not come back — that
/// identity is only known at save time, which is why `exe`/`title` are
/// persisted in the file despite never driving the match.
///
/// This is the whole matcher: a single exact `HWND` lookup, no scoring.
/// Each live window may be claimed once; a second slot naming the same
/// window is reported as such (`claimed` holds the windows taken so far)
/// rather than as a window that is not open.
fn resolve_hwnd(
    live: &mut HashSet<isize>,
    claimed: &mut HashSet<isize>,
    window: &WindowRef,
) -> Result<WindowId, String> {
    if live.remove(&window.hwnd) {
        claimed.insert(window.hwnd);
        Ok(WindowId(window.hwnd))
    } else if claimed.contains(&window.hwnd) {
        Err(format!(
            "window referenced twice in the loadout: \"{}\" ({}, hwnd {:#x}) — aborting load (no-partial)",
            window.title, window.exe, window.hwnd as u64
        ))
    } else {
        Err(format!(
            "window not currently open: \"{}\" ({}, hwnd {:#x}) — aborting load (no-partial)",
            window.title, window.exe, window.hwnd as u64
        ))
    }
}

/// One column in the `(width_px, rows)` form
/// [`ScrollingSpace::set_layout`](crate::workspace::ScrollingSpace::set_layout)
/// consumes.
type ColumnSpec = (u32, Vec<WindowId>);

/// The columns of `vl` as [`ColumnSpec`]s, so an existing layout can be
/// re-submitted with more columns appended.
#[must_use]
fn columns_of(vl: &VirtualLayout) -> Vec<ColumnSpec> {
    vl.columns
        .iter()
        .map(|c| {
            (
                u32::try_from(c.width_px).unwrap_or_default(),
                c.rows.iter().map(|r| r.window_id).collect(),
            )
        })
        .collect()
}

/// What to apply to a workspace: the outcome of [`merge_plan`], or the
/// snapshot as-is when its monitor is attached.
#[derive(Debug, PartialEq, Eq)]
struct WorkspacePlan {
    /// Columns for `set_layout`, in strip order.
    columns: Vec<ColumnSpec>,
    /// The strip's focus cursor after the apply.
    focus: Option<WindowId>,
    /// Viewport offset for `set_layout`.
    viewport_offset: i32,
    /// Floats to add to the workspace's floating space.
    floats: Vec<(WindowId, Rect)>,
}

/// Plan for merging a snapshot into a workspace that already has content —
/// used when the snapshot's monitor is not attached (B4).
///
/// The snapshot's columns are appended after the existing ones (the live
/// strip keeps its order and viewport), the existing focus cursor wins over
/// the snapshot's, and each incoming float rect is clamped into
/// `work_area` so a float saved on a detached display cannot be restored
/// off-screen.
#[must_use]
fn merge_plan(
    existing: Vec<ColumnSpec>,
    existing_focus: Option<WindowId>,
    existing_viewport: i32,
    incoming: Vec<ColumnSpec>,
    incoming_focus: Option<WindowId>,
    incoming_floats: Vec<(WindowId, Rect)>,
    work_area: Rect,
) -> WorkspacePlan {
    let mut columns = existing;
    columns.extend(incoming);
    let floats = incoming_floats
        .into_iter()
        .map(|(wid, rect)| (wid, rect.clamped_into(work_area)))
        .collect();
    WorkspacePlan {
        columns,
        focus: existing_focus.or(incoming_focus),
        viewport_offset: existing_viewport,
        floats,
    }
}

/// Resolve the seating target from the loadout's saved active-workspace flag.
///
/// Returns the [`WorkspaceId`] of the first snapshot marked `active: true`,
/// falling back to workspace 1 when none is marked. The `active` flag is
/// persisted per workspace at save time (exactly one snapshot is marked) but
/// was historically ignored at load — this reads it for the first time so the
/// workspace the user had visible is the one seated on restore.
///
/// Pure: operates only on the parsed snapshot list — no daemon state, no Win32.
/// The foreground-first refinement (foreground window's workspace → this
/// fallback → workspace 1) layers on top of this elsewhere.
#[must_use]
fn saved_active_target(workspaces: &[WorkspaceSnapshot]) -> WorkspaceId {
    workspaces
        .iter()
        .find(|ws| ws.active)
        .map(|ws| WorkspaceId(ws.workspace_id))
        .unwrap_or(WorkspaceId(1))
}

/// Find which workspace contains a given window, searching both each
/// workspace's scrolling space and its floating space.
///
/// Returns the [`WorkspaceId`] of the first workspace whose tiling columns or
/// floating set holds `window_id`, or `None` when no workspace owns it. The
/// search is exhaustive across both spaces because a window is seated in
/// exactly one — tiles live in `scrolling`, floats in `floating` — so the first
/// hit is authoritative.
///
/// Pure: walks the workspace slice's read-only views only — no daemon state, no
/// Win32. Used by the seating-target resolution to seat the workspace that
/// currently holds the OS foreground window (read separately via
/// `get_foreground_window`).
#[must_use]
fn workspace_containing_window(
    workspaces: &[Workspace],
    window_id: WindowId,
) -> Option<WorkspaceId> {
    workspaces.iter().find_map(|ws| {
        let in_scrolling = ws
            .scrolling
            .virtual_layout()
            .columns
            .iter()
            .any(|col| col.rows.iter().any(|row| row.window_id == window_id));
        let in_floating = ws.floating.contains(window_id);
        if in_scrolling || in_floating {
            Some(ws.id)
        } else {
            None
        }
    })
}

/// Resolve the seating target, preferring the workspace holding the OS
/// foreground window.
///
/// Order: foreground window's workspace → the loadout's saved active workspace
/// (via [`saved_active_target`], itself falling back to workspace 1). When the
/// foreground window is absent (`foreground: None`, e.g. desktop has focus) or
/// not managed by any workspace, resolution degrades to the saved-active
/// fallback unchanged.
///
/// The foreground is read-only here — this never calls `SetForegroundWindow`.
/// Seating the foreground's workspace keeps that window on-screen by
/// construction (it is the workspace the focused window actually lives in),
/// sidestepping the Windows startup foreground lock. The caller resolves the
/// foreground HWND against the post-apply workspaces, so `workspace_containing`
/// reports where the foreground window NOW sits after the loadout is applied.
#[must_use]
fn resolve_seating_target(
    workspaces: &[Workspace],
    foreground: Option<WindowId>,
    saved: &[WorkspaceSnapshot],
) -> WorkspaceId {
    foreground
        .and_then(|wid| workspace_containing_window(workspaces, wid))
        .unwrap_or_else(|| saved_active_target(saved))
}

/// Build the seating batch list that seats every non-empty workspace relative
/// to a target workspace.
///
/// For each non-empty workspace on the monitor, merges its scrolling and
/// floating actual layouts into one batch entry and computes that workspace's
/// vertical parking offset relative to `target_id` via
/// [`WorkspaceParking::offset`]: the target sits at offset `0` (visible),
/// every other workspace parks off-screen above (`-`) or below (`+`) — one
/// monitor height away on a free side, beyond the virtual screen on a side
/// with a stacked neighbour. Workspaces with no windows in either space are
/// skipped — there is nothing to seat.
///
/// Pure: no Win32, no animator, no daemon state. The caller submits the
/// returned `&[(ActualLayout, i32)]` through
/// [`FlowWM::animate_workspaces`](super::types::FlowWM::animate_workspaces),
/// which performs the visible-rect → window-rect translation, flattens border
/// overlays, and arms float-tracking suppression.
///
/// # Why this is separate from the workspace-switch path
///
/// This is structurally the same merge+offset loop that
/// `switch_workspace_layout` performs inline, lifted out as a pure function so
/// the load path can share the math **without** reusing the switch path's
/// bystander-teleport step. That teleport is correct for a real switch
/// (bystanders are already parked off-screen, so the snap is invisible) but
/// wrong at restore time, where every window begins on-screen — reusing it
/// would visibly snap on-screen windows off-screen. The full rationale is
/// restated briefly at the call site in `apply_loadout`.
#[must_use]
pub(super) fn build_seating_batches(
    workspaces: &[Workspace],
    target_id: WorkspaceId,
    parking: &WorkspaceParking,
) -> Vec<(ActualLayout, i32)> {
    let mut batches = Vec::new();
    for ws in workspaces {
        let scroll_actual = ws.scrolling.actual_layout();
        let float_actual = ws.floating.to_actual_layout();
        // Skip workspaces with nothing to seat — an empty workspace has no
        // windows to park, and emitting an empty batch entry would only add
        // noise (the animator drops empty target lists anyway).
        if scroll_actual.entries.is_empty() && float_actual.entries.is_empty() {
            continue;
        }
        let y_offset = parking.offset(ws.id, target_id);
        // Merge scrolling + floating into one entry so a single workspace's
        // tiles and floats animate together at the same parking offset.
        let mut entries = scroll_actual.entries.clone();
        entries.extend(float_actual.entries.iter().cloned());
        batches.push((ActualLayout { entries }, y_offset));
    }
    batches
}

impl FlowWM {
    /// Save the current workspace arrangement to a loadout file.
    ///
    /// `path` overrides the config-default location; `None` resolves to
    /// `config_dir / config.loadout.default_path`. Returns [`SocketResponse::Ok`]
    /// on success, [`SocketResponse::Error`] on failure.
    pub(super) fn dispatch_loadout_save(&self, path: Option<PathBuf>) -> SocketResponse {
        let resolved =
            path.unwrap_or_else(|| self.config_dir.join(&self.config.loadout.default_path));

        match self.build_and_write_loadout(&resolved) {
            Ok(()) => SocketResponse::Ok,
            Err(e) => {
                log::warn!("loadout save to {resolved:?} failed: {e}");
                SocketResponse::Error {
                    message: format!("loadout save failed: {e}"),
                }
            }
        }
    }

    /// Best-effort save to the config-default path, used by save-on-stop.
    ///
    /// Never blocks shutdown — errors are logged as warnings and discarded.
    pub(super) fn try_save_loadout_default(&self) -> Result<(), String> {
        let default = self.config_dir.join(&self.config.loadout.default_path);
        self.build_and_write_loadout(&default)
    }

    /// Load a saved loadout and apply it to live windows (IPC dispatch).
    ///
    /// Thin wrapper over [`Self::apply_loadout`] that maps the `Result` to a
    /// [`SocketResponse`] for the IPC layer.
    pub(super) fn dispatch_loadout_load(&mut self, path: Option<PathBuf>) -> SocketResponse {
        match self.apply_loadout(path) {
            Ok(()) => SocketResponse::Ok,
            Err(e) => {
                log::warn!("loadout load failed: {e}");
                SocketResponse::Error { message: e }
            }
        }
    }

    /// Best-effort auto-restore of the config-default loadout at startup.
    ///
    /// Called from the daemon's boot sequence (see `main::run`) right after
    /// [`FlowWM::new`](Self::new) finishes initializing the registry and
    /// layout, and before the IPC event loop starts. Runs the same code path
    /// as `dispatch_loadout_load`.
    ///
    /// Never blocks startup: a missing file (first run), an unmatched-window
    /// abort, or a parse failure are all logged and silently continue — the
    /// just-built init layout remains in place. Performing restore here
    /// (rather than via an IPC round-trip from the CLI) sidesteps the startup
    /// pipe race entirely.
    pub fn try_restore_loadout_default(&mut self) {
        let default = self.config_dir.join(&self.config.loadout.default_path);
        if !default.exists() {
            log::debug!("loadout restore: no loadout file at {default:?}, starting fresh");
            return;
        }
        if let Err(e) = self.apply_loadout(Some(default)) {
            log::info!("loadout restore skipped: {e}");
        }
    }

    /// Core load algorithm: resolve a loadout file onto live windows.
    ///
    /// Shared by [`Self::dispatch_loadout_load`] (IPC) and
    /// [`Self::try_restore_loadout_default`] (startup) so both paths behave
    /// identically. See the [module docs](self) for the no-partial algorithm.
    ///
    /// # Errors
    ///
    /// Returns `Err(String)` if the file cannot be read, parsed, or if any
    /// loadout slot cannot be matched to a live window (whole load aborted,
    /// zero state touched).
    fn apply_loadout(&mut self, path: Option<PathBuf>) -> Result<(), String> {
        let resolved =
            path.unwrap_or_else(|| self.config_dir.join(&self.config.loadout.default_path));

        // 1. Read + parse
        let raw = std::fs::read_to_string(&resolved)
            .map_err(|e| format!("loadout load: failed to read {resolved:?}: {e}"))?;
        let file: LoadoutFile = serde_json::from_str(&raw)
            .map_err(|e| format!("loadout load: failed to parse {resolved:?}: {e}"))?;

        // 2. Version guard: reject any non-current schema up front. A legacy
        //    pre-`HWND` file cannot be migrated (HWND can't be synthesized),
        //    so it is rejected with a reason rather than silently misread.
        if file.version != LoadoutFile::CURRENT_VERSION {
            return Err(format!(
                "unsupported file version {} (expected {}) — rejecting {resolved:?}",
                file.version,
                LoadoutFile::CURRENT_VERSION
            ));
        }

        // 3. Phase B: resolve every loadout slot's HWND against the live set
        //    (NO mutation of daemon state). Collect live managed windows by
        //    HWND first (skip Ignored — they belong to no loadout), then pop
        //    each slot's stored HWND from that set. `HWND` is the sole identity
        //    key — a singleton entry per live window — so matching is a trivial
        //    membership check; the first slot whose HWND is not live aborts the
        //    entire load (no partial application).
        let mut live: HashSet<isize> = HashSet::new();
        for win in self.registry.windows() {
            if matches!(win.state, WindowState::Ignored(_)) {
                continue;
            }
            live.insert(win.hwnd.0 as isize);
        }
        let mut claimed: HashSet<isize> = HashSet::new();

        struct ResolvedWorkspace {
            monitor: u32,
            workspace_id: u32,
            columns: Vec<(u32, Vec<WindowId>)>,
            focus: Option<WindowId>,
            viewport_offset: i32,
            floating: Vec<(WindowId, Rect)>,
        }

        let mut resolved_workspaces: Vec<ResolvedWorkspace> = Vec::new();
        for ws_snap in &file.workspaces {
            let mut columns = Vec::new();

            for col_snap in &ws_snap.scrolling.columns {
                let mut rows = Vec::new();
                for row_snap in &col_snap.rows {
                    rows.push(resolve_hwnd(&mut live, &mut claimed, &row_snap.window)?);
                }
                columns.push((col_snap.width_px, rows));
            }

            // Resolve floating entries by HWND.
            let mut floating = Vec::new();
            for float_entry in &ws_snap.floating {
                let hwnd = resolve_hwnd(&mut live, &mut claimed, &float_entry.window)?;
                let rect = rect_json_to_rect(&float_entry.rect);
                floating.push((hwnd, rect));
            }

            // Focus resolves directly by stored HWND: it must be one of the
            // tiled windows already assigned to a column above. (The focus
            // window was popped from `live` as a column row, so we look it up
            // among the resolved columns rather than re-querying `live`.)
            let focus = ws_snap.scrolling.focus.as_ref().and_then(|focus_ref| {
                let focus_hwnd = focus_ref.hwnd;
                columns
                    .iter()
                    .flat_map(|(_, rows)| rows.iter())
                    .find(|wid| wid.0 == focus_hwnd)
                    .copied()
            });

            resolved_workspaces.push(ResolvedWorkspace {
                monitor: ws_snap.monitor,
                workspace_id: ws_snap.workspace_id,
                columns,
                focus,
                viewport_offset: ws_snap.scrolling.viewport_offset,
                floating,
            });
        }

        // 4. Leftover tiling windows: live windows still in `live` (not claimed
        //    by any slot) that are **active** tiles. These are appended as
        //    new columns on the active workspace below. Floating leftovers are
        //    handled per-workspace by `replace_all`; Ignored windows never
        //    entered `live`; a minimized or hidden tile (one registered
        //    minimized at startup, or minimized since the save) has no seat
        //    to take until it comes back — the restore hook seats it then.
        let mut leftovers: Vec<WindowId> = Vec::new();
        for win in self.registry.windows() {
            if !matches!(win.state, WindowState::Tiling(TilingState::Active { .. })) {
                continue;
            }
            let hwnd = win.hwnd.0 as isize;
            if live.contains(&hwnd) {
                leftovers.push(WindowId(hwnd));
            }
        }
        // Floats of a snapshot that names a workspace no monitor has (see
        // phase D): seated on the fallback monitor's active workspace below,
        // clamped into its work area, instead of nowhere.
        let mut float_leftovers: Vec<(WindowId, Rect)> = Vec::new();

        // Where every window really is right now (parking offsets of its
        // current workspace applied), captured before phase D rewrites the
        // registry's tiled rects and before the seating pass changes any
        // active workspace: the seating submit below compares these with the
        // new positions to decide which moves must be instant (a slide
        // across a neighbouring monitor, horizontal or stacked).
        let prev_rects = self.snapshot_stacked_prev_rects();

        // 4a. Width presets: each monitor goes back to the preset it was
        //     saved on (base width only; the columns below carry theirs).
        self.restore_monitor_presets(&file.monitors);

        // 4b. Purge: every window the file references leaves every workspace
        //     on every monitor before any snapshot is applied, so a window
        //     cannot end up in two spaces when the file's monitor layout and
        //     the live one disagree (a one-monitor file on a two-monitor
        //     desktop used to leave monitor 1's copy in place).
        let referenced: Vec<WindowId> = resolved_workspaces
            .iter()
            .flat_map(|rw| {
                rw.columns
                    .iter()
                    .flat_map(|(_, rows)| rows.iter().copied())
                    .chain(rw.floating.iter().map(|(wid, _)| *wid))
            })
            .collect();
        self.purge_windows_from_all_spaces(&referenced);

        // 5. Phase D: apply per-workspace
        let mut merged_snapshots = 0usize;
        for rw in resolved_workspaces {
            let ws_id = WorkspaceId(rw.workspace_id);
            // Workspace ids repeat per monitor (1..=10 on each), so the
            // snapshot's monitor index disambiguates. A snapshot from a
            // monitor that is not attached is MERGED into the first monitor
            // that has the workspace instead of overwriting it.
            let in_range = (rw.monitor as usize) < self.monitors.len();
            let found = if in_range {
                self.monitors[rw.monitor as usize]
                    .find_workspace_index(ws_id)
                    .map(|wsi| (rw.monitor as usize, wsi))
            } else {
                self.monitors.iter().enumerate().find_map(|(mi, monitor)| {
                    monitor.find_workspace_index(ws_id).map(|wsi| (mi, wsi))
                })
            };
            let Some((monitor_idx, ws_idx)) = found else {
                // The purge above already took these windows out of every
                // space; skipping the snapshot outright would seat them
                // nowhere. Hand them to the leftover pass instead.
                log::warn!(
                    "loadout load: workspace {} (monitor {}) not found on any monitor; its {} tile(s) and {} float(s) go to the active workspace",
                    rw.workspace_id,
                    rw.monitor,
                    rw.columns.iter().map(|(_, rows)| rows.len()).sum::<usize>(),
                    rw.floating.len()
                );
                leftovers.extend(rw.columns.iter().flat_map(|(_, rows)| rows.iter().copied()));
                float_leftovers.extend(rw.floating.iter().copied());
                continue;
            };
            let merge = !in_range;
            if merge {
                merged_snapshots += 1;
            }
            let work_area = self.monitors[monitor_idx].work_area();
            // Columns arriving from a monitor that is no longer attached
            // take the receiving monitor's current preset width (widths
            // follow the monitor); without presets they keep their own.
            let columns = match (merge, self.monitor_preset_px(monitor_idx)) {
                (true, Some(px)) => rw.columns.into_iter().map(|(_, rows)| (px, rows)).collect(),
                _ => rw.columns,
            };

            // We need &mut access to both the workspace (for set_layout +
            // float adds) and the registry (for slot/rect sync). Use index-
            // based access with a temporary scope so the ws borrow is dropped
            // before we touch the registry.
            let (app, floats_to_sync): (AppliedLayout, Vec<(WindowId, Rect)>) = {
                let ws = &mut self.monitors[monitor_idx].workspaces_mut()[ws_idx];
                let plan = if merge {
                    let vl = ws.scrolling.virtual_layout();
                    merge_plan(
                        columns_of(vl),
                        ws.scrolling.last_focused_window(),
                        vl.viewport_offset,
                        columns,
                        rw.focus,
                        rw.floating,
                        work_area,
                    )
                } else {
                    WorkspacePlan {
                        columns,
                        focus: rw.focus,
                        viewport_offset: rw.viewport_offset,
                        floats: rw.floating,
                    }
                };
                let app = ws
                    .scrolling
                    .set_layout(plan.columns, plan.focus, plan.viewport_offset);
                // Floats are added, not swapped in wholesale: the purge above
                // already removed every referenced float, and a float this
                // workspace holds that the file does not mention stays put.
                for (wid, rect) in &plan.floats {
                    ws.floating.add(*wid, *rect);
                }
                (app, plan.floats)
            };

            // ws borrow dropped — safe to touch the registry now.
            self.registry
                .update_tiling_slots_from_layout(&app.virtual_layout);
            self.registry.update_tiled_rects(&app.actual_layout);
            for col in &app.virtual_layout.columns {
                for row in &col.rows {
                    self.remember_monitor(row.window_id, monitor_idx);
                }
            }
            // Mirror each float placement into the registry so border color and
            // float-location tracking are correct (mirrors register_float).
            for (wid, rect) in &floats_to_sync {
                if let Some(window) = self.registry.get_window_mut(HWND(wid.0 as *mut _)) {
                    window.state = WindowState::Floating(FloatingState::Active { rect: *rect });
                }
                self.remember_monitor(*wid, monitor_idx);
            }
        }
        if merged_snapshots > 0 {
            log::warn!(
                "loadout load: {merged_snapshots} workspace snapshot(s) were saved on a monitor that is not attached ({} attached); merged into the first monitor's matching workspaces",
                self.monitors.len()
            );
        }

        // 6. Leftover tiling windows: append as columns on the active
        //    workspace — but only those seated nowhere. A window the file does
        //    not mention that still sits in a workspace the file did not
        //    touch (e.g. monitor 1 when restoring a one-monitor file) stays
        //    where it is instead of being seated twice.
        let active_idx = self.active_monitor;
        for wid in leftovers {
            if self.monitor_index_containing(wid).is_some() {
                continue;
            }
            let applied = self.active_scrolling_mut().add_window(wid);
            self.remember_monitor(wid, active_idx);
            // Sync registry for the added window.
            self.registry
                .update_tiling_slots_from_layout(&applied.virtual_layout);
            self.registry.update_tiled_rects(&applied.actual_layout);
        }
        let fallback_work_area = self.monitors[active_idx].work_area();
        for (wid, rect) in float_leftovers {
            if self.monitor_index_containing(wid).is_some() {
                continue;
            }
            let rect = rect.clamped_into(fallback_work_area);
            self.monitors[active_idx]
                .active_workspace_mut()
                .floating
                .add(wid, rect);
            if let Some(window) = self.registry.get_window_mut(HWND(wid.0 as *mut _)) {
                window.state = WindowState::Floating(FloatingState::Active { rect });
            }
            self.remember_monitor(wid, active_idx);
        }

        // 7. Resolve the seating target. Preferred: the workspace that holds
        //    the OS foreground window — read-only (`GetForegroundWindow`, never
        //    `SetForegroundWindow`), so seating it keeps the focused window
        //    on-screen by construction and avoids the startup foreground lock.
        //    The foreground is read here, AFTER the layout apply, so
        //    `workspace_containing_window` reports where the foreground window
        //    now sits after the loadout seated it. First fallback is the
        //    loadout's saved active workspace (`saved_active_target`, itself
        //    falling back to workspace 1) — used when the foreground is absent
        //    (desktop focus) or belongs to no managed workspace.
        let foreground = crate::registry::win32::get_foreground_window().map(WindowId);
        // Seat every monitor: each picks its own target workspace (the one
        // holding the foreground window if it lives there, else the one the
        // file marks active) and parks its other workspaces vertically.
        let window_gap = self.active_scrolling().padding().window_gap;
        for mi in 0..self.monitors.len() {
            let fg_here = foreground
                .filter(|&wid| self.monitors[mi].find_workspace_containing(wid).is_some());
            let snaps_here: Vec<WorkspaceSnapshot> = file
                .workspaces
                .iter()
                .filter(|ws| ws.monitor as usize == mi)
                .cloned()
                .collect();
            let target_id =
                resolve_seating_target(self.monitors[mi].workspaces(), fg_here, &snaps_here);
            if self.monitors[mi].set_active_workspace(target_id).is_none() {
                log::warn!(
                    "loadout load: seating target workspace {} not found on monitor {mi}; \
                     leaving its freshly-tiled layout in place",
                    target_id.0
                );
                continue;
            }
            let parking = WorkspaceParking::new(
                self.monitors[mi].active_scrolling().monitor(),
                self.monitors[mi].screen_rect(),
                window_gap,
            );
            let batches =
                build_seating_batches(self.monitors[mi].workspaces(), target_id, &parking);
            self.animate_workspaces_on(mi, &batches, PrevRects::Snapshot(&prev_rects));
        }
        self.rebuild_float_hwnds();
        // A window the loadout turned into a float (or a tile) changes its
        // place in the floating layer.
        self.sync_all_top_layer();
        log::info!("loadout load: restored layout from {resolved:?}");
        Ok(())
    }

    /// Remove every window in `ids` from every workspace (strip and floating
    /// space) on every monitor. The layouts re-project immediately; the
    /// caller animates the result (the loadout seating pass does).
    pub(super) fn purge_windows_from_all_spaces(&mut self, ids: &[WindowId]) {
        for monitor in &mut self.monitors {
            for ws in monitor.workspaces_mut() {
                for &wid in ids {
                    if ws.scrolling.virtual_layout().find_window(wid).is_some() {
                        let _ = ws.scrolling.remove_window(wid);
                    }
                    ws.floating.remove(wid);
                }
            }
        }
    }

    // ── Save implementation detail ────────────────────────────────────

    /// Build a [`LoadoutFile`] from the current live state and write it to
    /// `path` as pretty-printed JSON.
    fn build_and_write_loadout(&self, path: &PathBuf) -> Result<(), String> {
        let workspaces = self.snapshot_workspaces();
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            workspaces,
            monitors: self.snapshot_monitors(),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| format!("serde serialization failed: {e}"))?;
        std::fs::write(path, json).map_err(|e| format!("failed to write {path:?}: {e}"))?;
        Ok(())
    }

    /// One [`MonitorSnapshot`] per monitor: the column width preset it is
    /// on (`None` without presets).
    fn snapshot_monitors(&self) -> Vec<MonitorSnapshot> {
        (0..self.monitors.len())
            .map(|mi| MonitorSnapshot {
                index: mi as u32,
                width_preset: self.monitor_preset_label(mi),
            })
            .collect()
    }

    /// Put every monitor named by the file back on the width preset it was
    /// saved on, when presets are configured and the saved label is one of
    /// them. Only the base width (the ladder) changes — the file's columns
    /// carry their own widths. A label that is no longer configured is
    /// logged and skipped.
    fn restore_monitor_presets(&mut self, snapshots: &[MonitorSnapshot]) {
        let Some(columns) = self.columns_config() else {
            return;
        };
        let targets: Vec<(usize, usize)> = snapshots
            .iter()
            .filter_map(|snap| {
                let mi = snap.index as usize;
                if mi >= self.monitors.len() {
                    return None;
                }
                let label = snap.width_preset.as_deref()?;
                let wanted = match crate::config::width_preset::WidthPreset::parse(label) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("loadout load: monitor {mi} width preset {label:?} ignored: {e}");
                        return None;
                    }
                };
                match columns.index_of(&wanted) {
                    Some(pi) => Some((mi, pi)),
                    None => {
                        log::warn!(
                            "loadout load: monitor {mi} width preset {label:?} is not configured (presets: {}); keeping {}",
                            columns.labels().join(", "),
                            self.monitor_preset_label(mi).unwrap_or_default()
                        );
                        None
                    }
                }
            })
            .collect();
        for (mi, pi) in targets {
            self.monitors[mi].set_width_preset(pi);
            let info = *self.monitors[mi].active_scrolling().monitor();
            if let Some(px) = self.preset_px_for(&info, pi) {
                for ws in self.monitors[mi].workspaces_mut() {
                    let _ = ws.scrolling.set_base_column_width(px);
                }
            }
        }
    }

    /// Walk every monitor → workspace and produce a [`WorkspaceSnapshot`] per
    /// workspace.
    fn snapshot_workspaces(&self) -> Vec<WorkspaceSnapshot> {
        let mut snapshots = Vec::new();
        for (mi, monitor) in self.monitors.iter().enumerate() {
            let active_ws_id = monitor.active_workspace_id();
            for ws in monitor.workspaces() {
                let scrolling = self.snapshot_scrolling(ws);
                let floating = self.snapshot_floating(ws);
                snapshots.push(WorkspaceSnapshot {
                    monitor: mi as u32,
                    workspace_id: ws.id.0,
                    active: ws.id == active_ws_id,
                    scrolling,
                    floating,
                });
            }
        }
        snapshots
    }

    /// Snapshot the tiling area of a single workspace.
    fn snapshot_scrolling(&self, ws: &crate::workspace::Workspace) -> ScrollingSnapshot {
        let vl = ws.scrolling.virtual_layout();
        let viewport_offset = vl.viewport_offset;

        // Focus: translate last_focused_window WindowId → WindowRef
        let focus = ws
            .scrolling
            .last_focused_window()
            .and_then(|wid| self.window_ref_for(wid));

        // A column whose rows all failed to resolve (their windows left the
        // registry) is not written: a column with no rows would come back
        // from the file as an empty column.
        let columns: Vec<ColumnSnapshot> = vl
            .columns
            .iter()
            .filter_map(|col| {
                let rows: Vec<RowSnapshot> = col
                    .rows
                    .iter()
                    .filter_map(|row| {
                        self.window_ref_for(row.window_id).map(|wr| RowSnapshot {
                            window: wr,
                            height_px: row.height,
                        })
                    })
                    .collect();
                (!rows.is_empty()).then_some(ColumnSnapshot {
                    width_px: col.width_px as u32,
                    rows,
                })
            })
            .collect();

        ScrollingSnapshot {
            viewport_offset,
            focus,
            columns,
        }
    }

    /// Snapshot the floating area of a single workspace.
    ///
    /// A fullscreen window (`set-window fullscreen`) is written as the
    /// float it comes back as after a restart: at the rect it was floating
    /// at before, else its screen rect clamped into the work area — never
    /// the taskbar-covering screen rect itself.
    fn snapshot_floating(&self, ws: &crate::workspace::Workspace) -> Vec<FloatingEntry> {
        ws.floating
            .windows()
            .iter()
            .filter_map(|entry| {
                let rect = match self.fullscreen.get(&entry.window_id.0) {
                    Some(super::fullscreen::FullscreenOrigin::Float { rect, .. }) => *rect,
                    Some(super::fullscreen::FullscreenOrigin::Tile { monitor, .. }) => {
                        let wa = self
                            .monitors
                            .get(*monitor)
                            .map(|m| m.work_area())
                            .unwrap_or(entry.rect);
                        entry.rect.clamped_into(wa)
                    }
                    None => entry.rect,
                };
                self.window_ref_for(entry.window_id)
                    .map(|wr| FloatingEntry {
                        window: wr,
                        rect: RectJson {
                            x: rect.x,
                            y: rect.y,
                            w: rect.width,
                            h: rect.height,
                        },
                    })
            })
            .collect()
    }

    /// Translate a [`WindowId`] to a [`WindowRef`] keyed by its `HWND`, with
    /// `exe`/`title` carried as diagnostics.
    ///
    /// The matcher reads only `hwnd`; `exe`/`title` are persisted so a failed
    /// restore can name the missing window. Returns `None` (with a warning) if
    /// the window is not found in the registry.
    fn window_ref_for(&self, wid: WindowId) -> Option<WindowRef> {
        let win = self.registry.get_window(HWND(wid.0 as *mut _))?;
        Some(WindowRef {
            hwnd: wid.0,
            exe: win.exe.clone(),
            title: win.title.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::workspace::{ScrollingSpace, Workspace};

    // ── rect_json_to_rect: pure conversion ───────────────────────────

    /// Positive: RectJson → Rect converts field names correctly.
    #[test]
    fn rect_json_to_rect_correct() {
        let rj = RectJson {
            x: 10,
            y: 20,
            w: 300,
            h: 400,
        };
        let r = rect_json_to_rect(&rj);
        assert_eq!(
            r,
            Rect {
                x: 10,
                y: 20,
                width: 300,
                height: 400
            }
        );
    }

    /// Positive: zero rect survives round-trip.
    #[test]
    fn rect_json_to_rect_zero() {
        let rj = RectJson {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        };
        let r = rect_json_to_rect(&rj);
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0
            }
        );
    }

    // ── saved_active_target: saved active-workspace flag ───────────────
    //
    // The loadout persists an `active` flag per workspace at save time; the
    // seating target is the id of the snapshot marked active, falling back to
    // workspace 1 when none is marked (defensive — save always marks exactly
    // one, but a hand-edited or all-false file must still resolve).

    /// Positive: the snapshot marked active resolves to its workspace id.
    #[test]
    fn saved_active_target_picks_the_marked_workspace() {
        let snaps = vec![
            WorkspaceSnapshot {
                monitor: 0,
                workspace_id: 1,
                active: false,
                scrolling: empty_scrolling_snap(),
                floating: vec![],
            },
            WorkspaceSnapshot {
                monitor: 0,
                workspace_id: 2,
                active: true,
                scrolling: empty_scrolling_snap(),
                floating: vec![],
            },
        ];
        assert_eq!(saved_active_target(&snaps), WorkspaceId(2));
    }

    /// Negative: no snapshot marked active → workspace 1 fallback.
    #[test]
    fn saved_active_target_falls_back_to_workspace_one_when_none_marked() {
        let snaps = vec![WorkspaceSnapshot {
            monitor: 0,
            workspace_id: 3,
            active: false,
            scrolling: empty_scrolling_snap(),
            floating: vec![],
        }];
        assert_eq!(saved_active_target(&snaps), WorkspaceId(1));
    }

    /// Negative: empty snapshot list → workspace 1 fallback.
    #[test]
    fn saved_active_target_falls_back_to_workspace_one_when_empty() {
        assert_eq!(saved_active_target(&[]), WorkspaceId(1));
    }

    // ── build_seating_batches: full-stack seating output ──────────────
    //
    // The core contract: for a given target, the builder returns EVERY
    // non-empty workspace (not just the active one), each merged across
    // scrolling + floating, at its correct parking offset relative to the
    // target. Empty workspaces are absent.

    /// Positive: every non-empty workspace appears at its correct offset for a
    /// target at the top of the stack. Windows on ws 1 and ws 3, target ws 1 →
    /// ws 1 at 0, ws 3 below at +Y_UNIT. Empty ws 2 is absent.
    #[test]
    fn seating_batches_cover_every_nonempty_workspace() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_empty_workspace(2),
            make_workspace_with_tile(3, WindowId(200)),
        ];
        let batches = build_seating_batches(&workspaces, WorkspaceId(1), &single_parking());

        // Two non-empty workspaces; the empty middle one is skipped.
        assert_eq!(batches.len(), 2, "empty workspace must be skipped");
        // Target workspace sits at offset 0 (visible).
        assert_eq!(batches[0].1, 0, "target workspace parks at offset 0");
        assert!(
            batches[0]
                .0
                .entries
                .iter()
                .any(|e| e.window_id == WindowId(100)),
            "target workspace's tile must be in its merged batch"
        );
        // Workspace 3 is below the target → parks at +Y_UNIT.
        assert_eq!(batches[1].1, Y_UNIT, "ws 3 parks below target at +Y_UNIT");
        assert!(
            batches[1]
                .0
                .entries
                .iter()
                .any(|e| e.window_id == WindowId(200)),
            "ws 3's tile must be in its merged batch"
        );
    }

    /// Positive: a target in the middle produces both an above (negative) and a
    /// below (positive) offset around the offset-0 target.
    #[test]
    fn seating_batches_offset_signs_around_a_middle_target() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_workspace_with_tile(2, WindowId(200)),
            make_workspace_with_tile(3, WindowId(300)),
        ];
        let batches = build_seating_batches(&workspaces, WorkspaceId(2), &single_parking());

        // Offsets in workspace-id order: ws1 above (-Y_UNIT), ws2 target (0),
        // ws3 below (+Y_UNIT).
        assert_eq!(
            batches.iter().map(|(_, off)| *off).collect::<Vec<_>>(),
            vec![-Y_UNIT, 0, Y_UNIT],
            "offsets must be [-Y_UNIT, 0, +Y_UNIT] around a middle target"
        );
    }

    /// Positive: a workspace with ONLY a floating window (no tiles) still gets
    /// a batch entry — floats are part of the merge and must be seated too.
    #[test]
    fn seating_batches_include_float_only_workspaces() {
        let mut ws = make_empty_workspace(2);
        ws.floating.add(
            WindowId(400),
            Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 100,
            },
        );
        let batches = build_seating_batches(&[ws], WorkspaceId(2), &single_parking());

        assert_eq!(
            batches.len(),
            1,
            "float-only workspace must produce a batch"
        );
        assert_eq!(batches[0].1, 0, "target workspace at offset 0");
        assert!(
            batches[0]
                .0
                .entries
                .iter()
                .any(|e| e.window_id == WindowId(400)),
            "floating window must be merged into the batch"
        );
    }

    /// Positive: a workspace holding BOTH a tile and a float merges both into a
    /// single batch entry (one entry per workspace, not one per space).
    #[test]
    fn seating_batches_merge_scrolling_and_float_into_one_entry() {
        let mut ws = make_workspace_with_tile(1, WindowId(100));
        ws.floating.add(
            WindowId(101),
            Rect {
                x: 20,
                y: 20,
                width: 80,
                height: 80,
            },
        );
        let batches = build_seating_batches(&[ws], WorkspaceId(1), &single_parking());

        assert_eq!(batches.len(), 1);
        let ids: Vec<isize> = batches[0].0.entries.iter().map(|e| e.window_id.0).collect();
        assert!(ids.contains(&100), "tile merged");
        assert!(ids.contains(&101), "float merged");
    }

    /// Negative: when every workspace is empty, no batches are produced.
    #[test]
    fn seating_batches_empty_when_all_workspaces_empty() {
        let workspaces = vec![make_empty_workspace(1), make_empty_workspace(2)];
        let batches = build_seating_batches(&workspaces, WorkspaceId(1), &single_parking());
        assert!(batches.is_empty());
    }

    /// Positive: on a monitor with a display stacked above it, the seating
    /// pass parks "above" workspaces beyond the virtual screen (the far slot)
    /// while "below" ones keep the one-height slot — the same split the
    /// workspace switch uses, so a loadout restore never seats a workspace
    /// on the neighbour.
    #[test]
    fn seating_batches_use_the_far_slot_toward_a_stacked_neighbour() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_workspace_with_tile(2, WindowId(200)),
            make_workspace_with_tile(3, WindowId(300)),
        ];
        let laptop = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: MONITOR_H,
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
            height: 1440 + MONITOR_H,
        };
        let info = MonitorInfo::in_desktop(laptop, laptop, vs, &[external]);
        let parking = WorkspaceParking::new(&info, laptop, GAP);
        let batches = build_seating_batches(&workspaces, WorkspaceId(2), &parking);
        let offsets: Vec<i32> = batches.iter().map(|(_, off)| *off).collect();
        let far_above = external.y - crate::layout::projection::PARK_MARGIN - laptop.bottom();
        assert_eq!(offsets, vec![far_above, 0, Y_UNIT]);
        // ws 1's tile, shifted by its offset, lies entirely above the
        // external display rather than on it.
        let tile = &batches[0].0.entries[0].rect;
        assert!(tile.bottom() + far_above <= external.y);
    }

    // ── workspace_containing_window: workspace lookup by window ──────────
    //
    // Seating-target resolution needs to map a foreground window back to the
    // workspace it lives in. The search must cover BOTH spaces: a tile in the
    // scrolling canvas and a float in the floating set.

    /// Positive: a tiled window resolves to its workspace's id.
    #[test]
    fn workspace_containing_finds_tiled_window() {
        let workspaces = vec![
            make_empty_workspace(1),
            make_workspace_with_tile(2, WindowId(111)),
        ];
        assert_eq!(
            workspace_containing_window(&workspaces, WindowId(111)),
            Some(WorkspaceId(2))
        );
    }

    /// Positive: a floating window resolves to its workspace's id — the search
    /// covers the floating space, not just tiles.
    #[test]
    fn workspace_containing_finds_floating_window() {
        let mut ws = make_empty_workspace(3);
        ws.floating.add(
            WindowId(222),
            Rect {
                x: 5,
                y: 5,
                width: 50,
                height: 50,
            },
        );
        let workspaces = vec![make_empty_workspace(1), ws];
        assert_eq!(
            workspace_containing_window(&workspaces, WindowId(222)),
            Some(WorkspaceId(3))
        );
    }

    /// Negative: an unmanaged / absent window resolves to none — no workspace
    /// claims it, so the foreground lookup falls back to saved-active.
    #[test]
    fn workspace_containing_returns_none_for_absent_window() {
        let workspaces = vec![make_workspace_with_tile(1, WindowId(100))];
        assert_eq!(
            workspace_containing_window(&workspaces, WindowId(999)),
            None
        );
    }

    /// Negative: empty workspace list → none.
    #[test]
    fn workspace_containing_returns_none_when_no_workspaces() {
        assert_eq!(workspace_containing_window(&[], WindowId(1)), None);
    }

    // ── resolve_seating_target: foreground-first ordering ───────────────
    //
    // Target order: foreground window's workspace → saved-active → workspace 1.

    /// Positive: when the foreground window sits in a managed workspace, that
    /// workspace wins over the saved-active flag.
    #[test]
    fn seating_target_prefers_foreground_workspace() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_workspace_with_tile(2, WindowId(200)),
        ];
        // Saved-active is workspace 1, but the foreground window lives on ws 2.
        let saved = vec![WorkspaceSnapshot {
            monitor: 0,
            workspace_id: 1,
            active: true,
            scrolling: empty_scrolling_snap(),
            floating: vec![],
        }];
        assert_eq!(
            resolve_seating_target(&workspaces, Some(WindowId(200)), &saved),
            WorkspaceId(2)
        );
    }

    /// Fallback: foreground present but unmanaged → saved-active workspace.
    #[test]
    fn seating_target_falls_back_to_saved_when_foreground_unmanaged() {
        let workspaces = vec![
            make_workspace_with_tile(1, WindowId(100)),
            make_workspace_with_tile(2, WindowId(200)),
        ];
        // Foreground window 999 is not on any workspace → saved-active ws 2.
        let saved = vec![WorkspaceSnapshot {
            monitor: 0,
            workspace_id: 2,
            active: true,
            scrolling: empty_scrolling_snap(),
            floating: vec![],
        }];
        assert_eq!(
            resolve_seating_target(&workspaces, Some(WindowId(999)), &saved),
            WorkspaceId(2)
        );
    }

    /// Fallback: no foreground (desktop has focus) → saved-active workspace.
    #[test]
    fn seating_target_falls_back_to_saved_when_no_foreground() {
        let workspaces = vec![make_workspace_with_tile(1, WindowId(100))];
        let saved = vec![WorkspaceSnapshot {
            monitor: 0,
            workspace_id: 1,
            active: true,
            scrolling: empty_scrolling_snap(),
            floating: vec![],
        }];
        assert_eq!(
            resolve_seating_target(&workspaces, None, &saved),
            WorkspaceId(1)
        );
    }

    /// Fallback: foreground unmanaged AND no saved-active → workspace 1.
    #[test]
    fn seating_target_falls_back_to_workspace_one() {
        let workspaces = vec![make_workspace_with_tile(3, WindowId(100))];
        assert_eq!(
            resolve_seating_target(&workspaces, Some(WindowId(999)), &[]),
            WorkspaceId(1)
        );
    }

    // ── Test helpers ────────────────────────────────────────────────────
    /// Test monitor height and gap — mirror `y_offset.rs` test constants so the
    /// parking unit matches: `Y_UNIT = MONITOR_H + GAP`.
    const MONITOR_H: i32 = 1080;
    const GAP: i32 = 4;
    const Y_UNIT: i32 = MONITOR_H + GAP; // 1084

    /// Parking geometry for a lone `1920×MONITOR_H` monitor: both sides free,
    /// so every offset is `±Y_UNIT` or `0`.
    fn single_parking() -> WorkspaceParking {
        WorkspaceParking::single(
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: MONITOR_H,
            },
            GAP,
        )
    }

    /// Build a [`ScrollingSpace`] sized like the canonical 1920×1080 test
    /// monitor, mirroring `workspace::monitor::tests::make_scrolling`.
    fn make_scrolling() -> ScrollingSpace {
        ScrollingSpace::new(
            MonitorInfo::single(Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }),
            960,
            320,
            100,
            100,
            Padding {
                window_gap: GAP,
                up: 0,
                down: 0,
            },
            4,
        )
    }

    /// Build a workspace that owns one tiled window.
    fn make_workspace_with_tile(id: u32, wid: WindowId) -> Workspace {
        let mut scrolling = make_scrolling();
        scrolling.add_window(wid);
        Workspace::new(WorkspaceId(id), scrolling)
    }

    /// Build an empty workspace (no tiles, no floats).
    fn make_empty_workspace(id: u32) -> Workspace {
        Workspace::new(WorkspaceId(id), make_scrolling())
    }

    /// An all-zero [`ScrollingSnapshot`] for [`saved_active_target`] tests that
    /// only care about the `active` flag.
    fn empty_scrolling_snap() -> ScrollingSnapshot {
        ScrollingSnapshot {
            viewport_offset: 0,
            focus: None,
            columns: vec![],
        }
    }

    // ── merge_plan (B4) ─────────────────────────────────────────────────

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn merge_plan_appends_columns_keeps_focus_and_viewport_and_clamps_floats() {
        let wa = rect(0, 0, 1920, 1080);
        let plan = merge_plan(
            vec![(960, vec![WindowId(1)])],
            Some(WindowId(1)),
            -300,
            vec![(640, vec![WindowId(2), WindowId(3)])],
            Some(WindowId(2)),
            vec![(WindowId(9), rect(5000, 5000, 400, 300))],
            wa,
        );
        assert_eq!(
            plan,
            WorkspacePlan {
                columns: vec![
                    (960, vec![WindowId(1)]),
                    (640, vec![WindowId(2), WindowId(3)])
                ],
                focus: Some(WindowId(1)),
                viewport_offset: -300,
                floats: vec![(WindowId(9), rect(1520, 780, 400, 300))],
            }
        );
    }

    #[test]
    fn merge_plan_takes_the_snapshot_focus_when_the_strip_has_none() {
        let plan = merge_plan(
            vec![],
            None,
            0,
            vec![(960, vec![WindowId(2)])],
            Some(WindowId(2)),
            vec![],
            rect(0, 0, 1920, 1080),
        );
        assert_eq!(plan.focus, Some(WindowId(2)));
    }

    #[test]
    fn columns_of_round_trips_a_layout() {
        // set_layout quantises widths to the slot ladder, so round-trip the
        // ids and compare widths against what the layout actually holds.
        let mut scrolling = make_scrolling();
        let _ = scrolling.set_layout(
            vec![
                (960, vec![WindowId(1), WindowId(2)]),
                (960, vec![WindowId(3)]),
            ],
            None,
            0,
        );
        let vl = scrolling.virtual_layout();
        let expected: Vec<ColumnSpec> = vec![
            (
                vl.columns[0].width_px as u32,
                vec![WindowId(1), WindowId(2)],
            ),
            (vl.columns[1].width_px as u32, vec![WindowId(3)]),
        ];
        let got = columns_of(vl);
        assert_eq!(got, expected);
        // And feeding it back reproduces the same layout.
        let _ = scrolling.set_layout(got, None, 0);
        assert_eq!(columns_of(scrolling.virtual_layout()), expected);
    }

    // ── apply_loadout end-to-end: purge + merge + clamp ─────────────────

    use crate::config::types::WindowRulesConfig;
    use crate::registry::WindowRegistry;
    use crate::registry::types::{TilingState, WindowState};
    use crate::workspace::Monitor;
    use windows::Win32::Foundation::HWND;

    /// Register a fake window in `state` (no Win32).
    fn fake_window(registry: &mut WindowRegistry, hwnd: isize, state: WindowState) {
        let info = crate::registry::win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: format!("win{hwnd}"),
            class: "C".into(),
            rect: rect(0, 0, 800, 600),
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

    /// A monitor at `x` with workspaces 1 and 2, workspace 1 active.
    fn monitor_at(x: i32) -> Monitor {
        let screen = rect(x, 0, 1920, 1080);
        let ws = |id: u32| {
            Workspace::new(
                WorkspaceId(id),
                ScrollingSpace::new(
                    MonitorInfo::single(screen),
                    960,
                    320,
                    100,
                    100,
                    Padding {
                        window_gap: GAP,
                        up: 0,
                        down: 0,
                    },
                    4,
                ),
            )
        };
        Monitor::new(screen, screen, vec![ws(1), ws(2)], 0)
    }

    fn window_ref(hwnd: isize) -> WindowRef {
        WindowRef {
            hwnd,
            exe: "app.exe".into(),
            title: format!("win{hwnd}"),
        }
    }

    fn tiled_column(hwnd: isize) -> ColumnSnapshot {
        ColumnSnapshot {
            width_px: 960,
            rows: vec![RowSnapshot {
                window: window_ref(hwnd),
                height_px: 1000,
            }],
        }
    }

    /// A two-monitor daemon: A tiled on monitor 0, B tiled and F floating
    /// on monitor 1 (all on workspace 1).
    fn two_monitor_flow() -> FlowWM {
        const A: isize = 0xA;
        const B: isize = 0xB;
        const F: isize = 0xF;
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        let tile = || WindowState::Tiling(TilingState::Active { col: 0, row: 0 });
        fake_window(&mut registry, A, tile());
        fake_window(&mut registry, B, tile());
        fake_window(
            &mut registry,
            F,
            WindowState::Floating(FloatingState::Active {
                rect: rect(2100, 100, 400, 300),
            }),
        );
        let mut m0 = monitor_at(0);
        let mut m1 = monitor_at(1920);
        m0.active_scrolling_mut().add_window(WindowId(A));
        m1.active_scrolling_mut().add_window(WindowId(B));
        m1.active_workspace_mut()
            .floating
            .add(WindowId(F), rect(2100, 100, 400, 300));
        let mut flow = FlowWM::new_for_test(registry, vec![m0, m1]);
        flow.remember_monitor(WindowId(A), 0);
        flow.remember_monitor(WindowId(B), 1);
        flow.remember_monitor(WindowId(F), 1);
        flow
    }

    fn write_loadout(file: &LoadoutFile) -> PathBuf {
        // Tests run in parallel and the clock is coarse: a per-process
        // counter keeps two tests from writing (and reading) the same file.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "flowwm-loadout-test-{}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, serde_json::to_string(file).expect("serialise"))
            .expect("write loadout");
        path
    }

    /// A snapshot naming a workspace no monitor has (workspace 42): its
    /// windows are purged from their old seats and must come back as
    /// leftovers on the active workspace, not be seated nowhere.
    #[test]
    fn apply_seats_windows_of_an_unknown_workspace_as_leftovers() {
        let mut flow = two_monitor_flow();
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![WorkspaceSnapshot {
                monitor: 1,
                workspace_id: 42,
                active: false,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 0,
                    focus: Some(window_ref(0xB)),
                    columns: vec![tiled_column(0xB)],
                },
                floating: vec![FloatingEntry {
                    window: window_ref(0xF),
                    rect: RectJson {
                        x: 5000,
                        y: 50,
                        w: 400,
                        h: 300,
                    },
                }],
            }],
        };
        let path = write_loadout(&file);
        let result = flow.apply_loadout(Some(path.clone()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(result, Ok(()));

        // B and F left monitor 1 and landed on the active monitor (0).
        assert_eq!(flow.monitor_index_containing(WindowId(0xB)), Some(0));
        assert_eq!(flow.monitor_index_containing(WindowId(0xF)), Some(0));
        assert!(
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .find_window(WindowId(0xB))
                .is_some()
        );
        let floats = flow.monitors[0].active_workspace().floating.windows();
        assert_eq!(floats.len(), 1);
        assert_eq!(floats[0].window_id, WindowId(0xF));
        // Clamped into monitor 0's work area, same size.
        let f = floats[0].rect;
        assert_eq!((f.width, f.height), (400, 300));
        assert!(f.right() <= 1920 && f.x >= 0, "{f:?}");
        assert_eq!(flow.registry.remembered_monitor(0xB), Some(0));
        assert_eq!(flow.registry.remembered_monitor(0xF), Some(0));
        assert!(matches!(
            flow.registry.get_window(HWND(0xF as *mut _)).map(|w| w.state.clone()),
            Some(WindowState::Floating(FloatingState::Active { rect })) if rect == f
        ));
    }

    /// A file naming the same window in two slots is rejected as such, not
    /// as a window that is not open.
    #[test]
    fn apply_rejects_a_window_referenced_twice() {
        let mut flow = two_monitor_flow();
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![WorkspaceSnapshot {
                monitor: 0,
                workspace_id: 1,
                active: true,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 0,
                    focus: None,
                    columns: vec![tiled_column(0xA), tiled_column(0xA)],
                },
                floating: vec![],
            }],
        };
        let path = write_loadout(&file);
        let result = flow.apply_loadout(Some(path.clone()));
        let _ = std::fs::remove_file(&path);
        let err = result.expect_err("a duplicate reference aborts the load");
        assert!(err.contains("referenced twice"), "{err}");
        assert!(!err.contains("not currently open"), "{err}");
        // No-partial: nothing moved.
        assert_eq!(flow.monitor_index_containing(WindowId(0xB)), Some(1));
        assert_eq!(
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .window_count(),
            1
        );
    }

    /// A column whose window is no longer tracked is left out of the
    /// snapshot instead of being written with no rows.
    #[test]
    fn snapshot_skips_columns_whose_windows_are_gone() {
        let mut flow = two_monitor_flow();
        // A stale column: a window id the registry never saw.
        flow.monitors[0]
            .active_scrolling_mut()
            .add_window(WindowId(0xDEAD));
        let snaps = flow.snapshot_workspaces();
        let ws1 = snaps
            .iter()
            .find(|s| s.monitor == 0 && s.workspace_id == 1)
            .expect("monitor 0 workspace 1");
        assert_eq!(ws1.scrolling.columns.len(), 1);
        assert_eq!(ws1.scrolling.columns[0].rows[0].window.hwnd, 0xA);
    }

    /// A file saved on a three-monitor desktop puts B and F on monitor 2,
    /// which is not attached: they are purged from monitor 1 and merged into
    /// monitor 0's workspace 1 after A, with F clamped into the work area.
    #[test]
    fn apply_merges_out_of_range_monitor_and_purges_the_old_seat() {
        let mut flow = two_monitor_flow();
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![
                WorkspaceSnapshot {
                    monitor: 0,
                    workspace_id: 1,
                    active: true,
                    scrolling: ScrollingSnapshot {
                        viewport_offset: 0,
                        focus: Some(window_ref(0xA)),
                        columns: vec![tiled_column(0xA)],
                    },
                    floating: vec![],
                },
                WorkspaceSnapshot {
                    monitor: 2,
                    workspace_id: 1,
                    active: true,
                    scrolling: ScrollingSnapshot {
                        viewport_offset: 0,
                        focus: Some(window_ref(0xB)),
                        columns: vec![tiled_column(0xB)],
                    },
                    floating: vec![FloatingEntry {
                        window: window_ref(0xF),
                        rect: RectJson {
                            x: 6000,
                            y: 50,
                            w: 400,
                            h: 300,
                        },
                    }],
                },
            ],
        };
        let path = write_loadout(&file);
        let result = flow.apply_loadout(Some(path.clone()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(result, Ok(()));

        // Monitor 1 no longer holds B or F anywhere.
        assert!(
            flow.monitors[1]
                .find_workspace_containing(WindowId(0xB))
                .is_none()
        );
        assert!(
            flow.monitors[1]
                .find_workspace_containing(WindowId(0xF))
                .is_none()
        );
        // Monitor 0 workspace 1: A then B (merged, not overwritten).
        let vl = flow.monitors[0].active_scrolling().virtual_layout();
        let order: Vec<WindowId> = vl
            .columns
            .iter()
            .flat_map(|c| c.rows.iter().map(|r| r.window_id))
            .collect();
        assert_eq!(order, vec![WindowId(0xA), WindowId(0xB)]);
        assert_eq!(flow.monitor_index_containing(WindowId(0xB)), Some(0));
        // F lives on monitor 0, clamped into its work area.
        let floats = flow.monitors[0].active_workspace().floating.windows();
        assert_eq!(floats.len(), 1);
        assert_eq!(floats[0].window_id, WindowId(0xF));
        assert_eq!(floats[0].rect, rect(1520, 50, 400, 300));
        // Owning-monitor memory follows the merge.
        assert_eq!(flow.registry.remembered_monitor(0xB), Some(0));
        assert_eq!(flow.registry.remembered_monitor(0xF), Some(0));
        // Exactly one seat per window.
        let seats = |wid: WindowId| {
            flow.monitors
                .iter()
                .flat_map(|m| m.workspaces())
                .filter(|ws| {
                    ws.scrolling.virtual_layout().find_window(wid).is_some()
                        || ws.floating.contains(wid)
                })
                .count()
        };
        assert_eq!(seats(WindowId(0xA)), 1);
        assert_eq!(seats(WindowId(0xB)), 1);
        assert_eq!(seats(WindowId(0xF)), 1);
    }

    /// A tile the registry holds **minimized** (registered minimized at
    /// startup, or minimized since the save) is not a leftover: it has no
    /// seat until the restore hook gives it one, so the load must not
    /// append an iconic window as a column.
    #[test]
    fn apply_does_not_seat_a_minimized_tile_as_a_leftover() {
        const M: isize = 0x3;
        let mut flow = two_monitor_flow();
        fake_window(
            &mut flow.registry,
            M,
            WindowState::Tiling(TilingState::Minimized),
        );
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![WorkspaceSnapshot {
                monitor: 0,
                workspace_id: 1,
                active: true,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 0,
                    focus: Some(window_ref(0xA)),
                    columns: vec![tiled_column(0xA)],
                },
                floating: vec![],
            }],
        };
        let path = write_loadout(&file);
        let result = flow.apply_loadout(Some(path.clone()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(result, Ok(()));

        assert_eq!(flow.monitor_index_containing(WindowId(M)), None);
        assert!(flow.registry.is_minimized(M), "state untouched");
        assert_eq!(
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .window_count(),
            1,
            "only A on monitor 0"
        );
    }

    /// A one-monitor file on a two-monitor desktop: monitor 1's unreferenced
    /// windows stay where they are (not re-seated as leftovers on monitor 0).
    #[test]
    fn apply_leaves_unreferenced_windows_on_their_monitor() {
        let mut flow = two_monitor_flow();
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![WorkspaceSnapshot {
                monitor: 0,
                workspace_id: 1,
                active: true,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 0,
                    focus: Some(window_ref(0xA)),
                    columns: vec![tiled_column(0xA)],
                },
                floating: vec![],
            }],
        };
        let path = write_loadout(&file);
        let result = flow.apply_loadout(Some(path.clone()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(result, Ok(()));

        assert_eq!(flow.monitor_index_containing(WindowId(0xA)), Some(0));
        assert_eq!(flow.monitor_index_containing(WindowId(0xB)), Some(1));
        assert_eq!(flow.monitor_index_containing(WindowId(0xF)), Some(1));
        assert_eq!(
            flow.monitors[0]
                .active_scrolling()
                .virtual_layout()
                .window_count(),
            1
        );
        assert_eq!(
            flow.monitors[1]
                .active_scrolling()
                .virtual_layout()
                .window_count(),
            1
        );
    }

    /// A file that moves B from monitor 1 to monitor 0 (both attached) also
    /// removes it from monitor 1 — the purge is unconditional.
    #[test]
    fn apply_purges_a_window_that_changed_monitor() {
        let mut flow = two_monitor_flow();
        let file = LoadoutFile {
            version: LoadoutFile::CURRENT_VERSION,
            monitors: vec![],
            workspaces: vec![WorkspaceSnapshot {
                monitor: 0,
                workspace_id: 2,
                active: true,
                scrolling: ScrollingSnapshot {
                    viewport_offset: 0,
                    focus: Some(window_ref(0xB)),
                    columns: vec![tiled_column(0xB)],
                },
                floating: vec![],
            }],
        };
        let path = write_loadout(&file);
        let result = flow.apply_loadout(Some(path.clone()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(result, Ok(()));

        assert_eq!(
            flow.monitors[0].find_workspace_containing(WindowId(0xB)),
            Some(WorkspaceId(2))
        );
        assert!(
            flow.monitors[1]
                .find_workspace_containing(WindowId(0xB))
                .is_none()
        );
        assert_eq!(flow.registry.remembered_monitor(0xB), Some(0));
    }
}
