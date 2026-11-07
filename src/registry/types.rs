//! Per-window state types tracked by the registry.
//!
//! The vocabulary types used throughout the window registry, representing the
//! complete lifecycle of a managed window: initial classification → state
//! transitions → removal. Every tracked window exists in exactly one
//! [`WindowState`] at any time, which determines how flow interacts with it.
//!
//! # Send safety
//!
//! All types in this module are `Send` (and most are `Send + Sync`). The tricky
//! case is `HWND`: Win32's `HWND` wraps a raw pointer (`*mut c_void`) and is
//! `!Send` by default. We work around this by storing `HWND` inside [`Window`]
//! with a manual `unsafe impl Send`, and converting `HWND` to `isize` when
//! passing window ids across thread boundaries (in
//! [`HookEvent`](super::HookEvent)).
//!
//! The manual `Send` impl is sound because all Win32 API calls using the handle
//! happen on the IPC thread (which owns the registry); `HWND` is an opaque
//! handle value, not a real pointer — Rust code never reads through it.
//!
//! # Relationship to layout types
//!
//! The registry uses [`WindowState`] to track *what* a window is doing (tiling,
//! floating, ignored). The layout engine uses
//! [`WindowId`](crate::common::WindowId) and column/row positions to track
//! *where* a window is placed. The two systems connect through
//! [`VirtualSlot`], which stores the layout engine's column/row assignment
//! inside the registry's [`Window`] struct.
//!
//! # Serde
//!
//! All types implement `Serialize` and `Deserialize` for the query API. `HWND`
//! fields use a custom `#[serde(with)]` module that serializes to `isize`
//! (HWND pointers aren't meaningful in JSON).
//!
//! See the developer guide's *Window Registry* chapter
//! (`docs/src/dev-guide/window-registry.md`) for the lifecycle state-machine
//! diagram.

use std::path::PathBuf;

use crate::borders::Border;
use crate::common::{InvisibleBounds, Rect, Size};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HWND;

// ── HWND serde helper ───────────────────────────────────────────────

/// Serde helper for serializing/deserializing `HWND` as `isize`.
///
/// Win32 window handles are stored as opaque pointers, but their underlying
/// value is an `isize`. This module provides `#[serde(with)]` support for
/// fields of type `HWND`.
mod hwnd_serde {
    use super::HWND;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes an `HWND` as its underlying `isize` value.
    pub fn serialize<S: Serializer>(hwnd: &HWND, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_i64(hwnd.0 as isize as i64)
    }

    /// Deserializes an `isize` into an `HWND`.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<HWND, D::Error> {
        let val: i64 = Deserialize::deserialize(d)?;
        Ok(HWND(val as isize as *mut _))
    }
}

// ── Window ──────────────────────────────────────────────────────────

/// Per-window state tracked by the registry.
///
/// This is the authoritative record for every window the daemon manages.
/// Each [`Window`] is identified by its Win32 `HWND` and carries all metadata
/// needed for classification, layout assignment, and recovery.
///
/// # Field Design Rationale
///
/// - **`hwnd`** — The Win32 window handle. Serialized as `isize` for JSON.
///   Used as the HashMap key in [`WindowRegistry`](super::core::WindowRegistry).
///
/// - **`exe`** / **`title`** / **`class`** / **`process_path`** — Window
///   metadata used by the [`classification`](super::classification) module
///   to match against config rules. These are snapshotted at registration
///   time and not updated (window titles can change, but re-classification
///   is not currently supported).
///
/// - **`state`** — The current lifecycle state. Updated by
///   [`WindowRegistry`](super::core::WindowRegistry) methods on each hook event.
///   See [module-level docs](super) for the state machine diagram.
///
/// - **`pre_manage_rect`** — The window's position and size when flow first
///   saw it. Used by `flow restore` to return windows to their pre-managed
///   positions if the daemon exits or is stopped.
///
/// - **`last_natural_size`** — The window's preferred size, updated on
///   explicit user resize. Used when the layout engine needs to determine
///   a window's natural proportions.
///
/// - **`last_virtual_slot`** — The registry's column/row of the tile when
///   it was last minimized or hidden. Not where it goes back (that is
///   `restore_seat`); read as the placeholder slot of the `Active` state on
///   restore/show and by the hotplug reconcile to order its casualties.
///
/// - **`tiled_rect`** — The window's current tiled position as computed by
///   the layout engine's projection layer. Updated after every mutation.
///   `None` if the window has never been positioned by the layout engine.
///   This is distinct from `pre_manage_rect` (the position *before* flow
///   managed the window) — `tiled_rect` is always the *current* position.
///
/// - **`border`** — The optional overlay border drawn around this window.
///   Owned directly by the `Window` so its lifecycle is coupled to the
///   window's own (dropped when the window leaves the registry). Skipped
///   during serde because it is a live Win32 resource, not query state.
///   See `docs/src/dev-guide/borders.md`.
///
/// # Send Safety
///
/// `Window` contains `HWND` (a raw pointer), but we treat it as an opaque
/// handle value. See the [module-level Send Safety section](super#send-safety)
/// for the full safety argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    /// Win32 window handle.
    #[serde(with = "hwnd_serde")]
    pub hwnd: HWND,

    /// Executable name (e.g. `"code.exe"`).
    pub exe: String,

    /// Window title bar text.
    pub title: String,

    /// Win32 window class name.
    pub class: String,

    /// Full path to the executable.
    pub process_path: PathBuf,

    /// Current lifecycle state (tiling, floating, or ignored).
    pub state: WindowState,

    /// Position and size of the window before flow ever touched it.
    ///
    /// Used by `flow restore` to return windows to their pre-managed positions
    /// if the daemon dies.
    pub pre_manage_rect: Rect,

    /// Preferred unmanaged size, updated on explicit user resize.
    pub last_natural_size: Size,

    /// The registry slot at the last minimize or hide (see the struct
    /// docs: placeholder on restore, hotplug casualty order).
    pub last_virtual_slot: Option<VirtualSlot>,

    /// When the minimize hook last moved this window to a `Minimized`
    /// state; cleared on restore. The monitor-hotplug reconcile uses it to
    /// tell a window Windows minimized *with a display change* (and should
    /// restore) from one the user minimized earlier (and should leave).
    /// Process-local, so never serialised.
    #[serde(skip)]
    pub minimized_at: Option<std::time::Instant>,

    /// The visible rect an active float had when it was minimized or
    /// hidden; the restore and re-show put it back into
    /// `FloatingState::Active` (falling back to `pre_manage_rect`), so the
    /// float — and its border overlay — come back where they were, not
    /// where the window was before flow managed it. Process-local.
    #[serde(skip)]
    pub last_float_rect: Option<Rect>,

    /// Current tiled position computed by the layout engine's projection layer.
    ///
    /// Updated after every layout mutation. `None` if the window has never
    /// been positioned by the layout engine (e.g., it was just classified).
    /// This is the *live* position — distinct from `pre_manage_rect` which
    /// is the snapshot from *before* flow managed the window.
    pub tiled_rect: Option<Rect>,

    /// Invisible border sizes for this window.
    ///
    /// Measured at registration time by comparing `GetWindowRect`
    /// (full rect) against DWM extended frame bounds (visible rect), and
    /// re-measured whenever the daemon places the window (late chrome,
    /// ADR-0028).
    /// Used by the daemon to translate between the layout engine's visible-rect
    /// coordinates and Win32's window-rect coordinates for `SetWindowPos`.
    ///
    /// Defaults to [`InvisibleBounds::zero()`] if the measurement failed.
    pub invisible_bounds: InvisibleBounds,

    /// Overlay border drawn around this window, if any.
    ///
    /// Owned directly by the `Window` so the border's Win32 lifecycle is
    /// coupled to the window's own — when the `Window` is dropped, its
    /// `Border` is dropped, which destroys the overlay HWND.
    ///
    /// The daemon positions this via [`Border::set_geometry`] using the
    /// *visible* content rect (after removing `invisible_bounds`); for tiled
    /// windows it is flattened into the animator's target list so it moves in
    /// lockstep with the window itself.
    ///
    /// Skipped during serde because it is a live Win32 resource, not
    /// query-relevant state.
    #[serde(skip)]
    pub border: Option<Border>,

    /// Index (into the daemon's monitor list) of the monitor whose workspace
    /// last owned this window — the *owning monitor memory*.
    ///
    /// Recorded whenever the daemon seats the window in a monitor's
    /// scrolling or floating space (creation, move-to-monitor, float
    /// re-home, loadout restore). Consulted first when a minimized / hidden /
    /// maximized window comes back: a window parked beyond the virtual
    /// screen has an on-screen rect that says nothing about its monitor, so
    /// the rect alone would re-home it to the active monitor. `None` until
    /// the window has been seated once.
    #[serde(default)]
    pub monitor: Option<usize>,

    /// Where this tile sat when it was **minimized or hidden** — the
    /// *restore seat*, kept next to the owning monitor memory: the restore
    /// and re-show hooks put the window back there instead of appending it
    /// (ADR-0027). Set by the minimize and hide hooks for a seated tile,
    /// taken (cleared) by the next re-add.
    /// Process-local, so never serialised; dropped with the record.
    #[serde(skip)]
    pub restore_seat: Option<RestoreSeat>,

    /// The window rect (full `GetWindowRect` space, like `pre_manage_rect`)
    /// the window had the **first time it entered a tiled layout** this
    /// run; `None` until then. Recorded once (from `pre_manage_rect` or, for
    /// a `set-window tile`, the float rect) and never refreshed, so a
    /// float → tile → float → tile cycle keeps the first anchor. `flow
    /// dispatch tiling off` moves each tile back here.
    #[serde(default)]
    pub pre_tiling_rect: Option<Rect>,

    /// Whether the daemon **un-maximized this window to manage it** — by
    /// the `adopt_maximized` config key (startup scan, creation, `tiling
    /// on`) or by a `set-window tile|float|cycle` on an `Ignored(Maximized)`
    /// window. The window is handed back maximized wherever the daemon
    /// gives windows back: `tiling off`, the shutdown rescue (`flow stop`,
    /// session end) and the fail-back. Never cleared for the life of the
    /// record: the user had it maximized, so "put everything back as it
    /// was" means maximized. `false` for every other window.
    #[serde(default)]
    pub adopted_from_maximized: bool,

    /// Whether the window was **registered while minimized** (the startup
    /// scan, the `tiling on` re-scan or a creation event met it iconic)
    /// and its geometry still comes from its `WINDOWPLACEMENT`: the
    /// `pre_manage_rect` is the restore rect, `last_natural_size` its
    /// size, and `invisible_bounds` is zero because DWM cannot measure an
    /// iconic window. Cleared by the first
    /// [`restore_window`](super::core::WindowRegistry::restore_window),
    /// which re-measures all three from the now-visible frame before the
    /// daemon seats the window. `false` for every window met visible.
    #[serde(default)]
    pub geometry_from_placement: bool,

    /// Whether the daemon currently **holds this window in the floating
    /// layer** — the topmost band of the z-order (`HWND_TOPMOST`), where
    /// `[floating] always_on_top` keeps every active float and every
    /// explicit-rule-ignored window so a dialog is never hidden behind the
    /// tiles. Set when the window enters the layer and cleared when it
    /// leaves (tiled, minimized, hidden, fullscreen, `tiling off`, `flow
    /// stop`, the option turned off); see `daemon::top_layer`. Reported as
    /// `on_top` by `flow query all`. `false` while the option is off.
    #[serde(default)]
    pub on_top: bool,

    /// The `WS_EX_TOPMOST` flag the window had **before** the daemon put it
    /// in the floating layer, meaningful only while [`on_top`](Self::on_top)
    /// is set. A window that was topmost on its own (some apps' "always on
    /// top" option) is left topmost when it leaves the layer; one the daemon
    /// raised is lowered again (`HWND_NOTOPMOST`). The daemon never clears a
    /// flag it did not set.
    #[serde(default)]
    pub was_topmost: bool,
}

// SAFETY: `Window` contains `HWND` (a raw pointer), but we treat it as an
// opaque handle value. All Win32 API calls using this handle happen on the
// thread that owns the `MutexGuard<WindowRegistry>` — we never dereference
// HWND on a different thread. Safe to send across thread boundaries.
unsafe impl Send for Window {}

impl Window {
    /// Creates a new [`Window`] entry for initial registration.
    ///
    /// `last_virtual_slot` starts as `None`; it is populated when the window
    /// is assigned a virtual layout position. `last_natural_size` defaults to
    /// the size component of `pre_manage_rect`.
    ///
    /// # Arguments
    ///
    /// * `hwnd` — Win32 window handle.
    /// * `exe` — Executable name (e.g. `"code.exe"`).
    /// * `title` — Window title bar text.
    /// * `class` — Win32 window class name.
    /// * `process_path` — Full path to the executable.
    /// * `pre_manage_rect` — Window's position/size before flow manages it.
    /// * `initial_state` — Classified lifecycle state.
    /// * `invisible_bounds` — Per-edge invisible border sizes, measured by
    ///   comparing `GetWindowRect` against DWM extended frame bounds.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        hwnd: HWND,
        exe: String,
        title: String,
        class: String,
        process_path: PathBuf,
        pre_manage_rect: Rect,
        initial_state: WindowState,
        invisible_bounds: InvisibleBounds,
    ) -> Self {
        let last_natural_size = Size {
            w: pre_manage_rect.width,
            h: pre_manage_rect.height,
        };
        Self {
            hwnd,
            exe,
            title,
            class,
            process_path,
            state: initial_state,
            pre_manage_rect,
            last_natural_size,
            last_virtual_slot: None,
            last_float_rect: None,
            minimized_at: None,
            tiled_rect: None,
            invisible_bounds,
            border: None,
            monitor: None,
            restore_seat: None,
            pre_tiling_rect: None,
            adopted_from_maximized: false,
            geometry_from_placement: false,
            on_top: false,
            was_topmost: false,
        }
    }
}

// ── CreationSkip ────────────────────────────────────────────────────

/// The Win32 pre-filter that kept a window out of the registry.
///
/// Returned by
/// [`WindowRegistry::creation_gates`](super::core::WindowRegistry::creation_gates),
/// the gate sequence shared by the startup scan and
/// [`handle_created`](super::core::WindowRegistry::handle_created). Each
/// variant is one gate, in the order they run (cheapest first), so a
/// rejected window is diagnosable from the log and — for `set-window` on
/// an unknown focused window — from the error the CLI prints. Being iconic
/// is **not** a gate: a minimized window is registered in its `Minimized`
/// state from its restore geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreationSkip {
    /// The window is already in the registry (duplicate `CREATE`, or the
    /// scan got there first).
    AlreadyTracked,
    /// `IsWindowVisible` is false (created hidden, or not shown yet).
    NotVisible,
    /// The title is empty (splash screen, container, late-titling app).
    EmptyTitle,
    /// `WS_EX_TOOLWINDOW` without `WS_EX_APPWINDOW`, or DWM-cloaked.
    NotAltTabVisible,
    /// Owned by another window (dialog, popup) or a `WS_CHILD` control.
    OwnedOrChild,
    /// Every gate passed but the window's metadata could not be read
    /// (`get_window_info` failed — the window is gone or inaccessible).
    InfoUnavailable(String),
}

impl std::fmt::Display for CreationSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyTracked => f.write_str("already tracked"),
            Self::NotVisible => f.write_str("not visible"),
            Self::EmptyTitle => f.write_str("empty title"),
            Self::NotAltTabVisible => {
                f.write_str("not Alt+Tab visible (tool window or DWM-cloaked)")
            }
            Self::OwnedOrChild => {
                f.write_str("has an owner or parent (dialog, popup or child control)")
            }
            Self::InfoUnavailable(e) => write!(f, "window info unavailable: {e}"),
        }
    }
}

// ── WindowState ────────────────────────────────────────────────────

/// Lifecycle state of a managed window.
///
/// Every window tracked by the registry is in exactly one of these states.
/// The state determines how the layout engine and compositor interact with
/// the window.
///
/// # State Transitions
///
/// Transitions are triggered by WinEvent hooks and applied by
/// [`WindowRegistry`](super::core::WindowRegistry):
///
/// | From | Event | To |
/// |------|-------|----|
/// | (new) | classification | `Tiling(Active)` / `Floating(Active)` / `Ignored(...)` |
/// | `Tiling(Active)` | `MinimizeStart` | `Tiling(Minimized)` |
/// | `Tiling(Minimized)` | `MinimizeEnd` | `Tiling(Active)`; the daemon re-seats it from its restore seat |
/// | `Tiling(Active)` | `EVENT_OBJECT_HIDE` | `Tiling(Hidden)`; the daemon records its restore seat |
/// | `Tiling(Hidden)` | `EVENT_OBJECT_SHOW` | `Tiling(Active)`; the daemon re-seats it from its restore seat |
/// | `Tiling(Hidden)` | Destroyed | removed from registry |
/// | `Floating(Active)` | `MinimizeStart` | `Floating(Minimized)` |
/// | `Floating(Minimized)` | `MinimizeEnd` | `Floating(Active)` with original rect |
/// | `Floating(Active)` | `EVENT_OBJECT_HIDE` | `Floating(Hidden)` |
/// | `Floating(Hidden)` | `EVENT_OBJECT_SHOW` | `Floating(Active)` with the rect it was hidden at |
///
/// The column a tile goes back to is the daemon's
/// [`Window::restore_seat`] (ADR-0027), not `last_virtual_slot`: the
/// `col`/`row` the registry writes on `MinimizeEnd`/`SHOW` are a
/// placeholder until the daemon re-syncs the slots from the layout.
///
/// Note: tile ↔ float transitions are implemented via `flow dispatch set-window`
/// (see `docs/src/dev-guide/floating-space.md`). An `Ignored` window leaves
/// that state through the OS-driven recovery, the `adopt_maximized` config
/// key or `set-window`; the one direction that remains unimplemented is
/// tiling → `Ignored(Maximized)`. See `docs/src/dev-guide/window-registry.md`,
/// *State Transitions*.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WindowState {
    /// Window is participating in the tiling layout.
    Tiling(TilingState),
    /// Window is floating (user-dragged or rule-assigned).
    Floating(FloatingState),
    /// Window is ignored by flow (maximized, fullscreen, or explicit rule).
    Ignored(IgnoredReason),
}

// ── TilingState ────────────────────────────────────────────────────

/// Sub-state for windows participating in the tiling layout.
///
/// Tiled windows can be actively positioned at a grid slot, minimized or
/// hidden. A minimized or hidden tile is out of the layout; the daemon's
/// [`Window::restore_seat`] says where it goes back (ADR-0027).
///
/// # Relationship to Layout Engine
///
/// The `col` and `row` values in [`Active`](TilingState::Active) correspond to
/// the layout engine's column/row indices. These are updated when the layout
/// engine assigns a new position (via mutations like swap, focus, add).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TilingState {
    /// Window is actively tiled at the given virtual position.
    Active {
        /// Column index in the virtual layout.
        col: usize,
        /// Row index within the column.
        row: usize,
    },
    /// Window is minimized (preserving its virtual-slot assignment).
    Minimized,
    /// Window is hidden from the user (tray-hidden / cloaked / `SW_HIDE`) but
    /// still tracked. The daemon records its restore seat on hide and puts
    /// it back there on re-show, as for a minimize (ADR-0027).
    ///
    /// Distinct from [`Minimized`](Self::Minimized): a minimize is an
    /// explicit user action via the taskbar/minimize button, whereas
    /// `Hidden` captures tray-hide (Discord/Steam "close" button) and DWM
    /// cloaking. Both remove the window from the live layout but are
    /// tracked separately for future heuristics.
    Hidden,
}

// ── FloatingState ──────────────────────────────────────────────────

/// Sub-state for floating (non-tiled) windows.
///
/// Floating windows are positioned freely by the user. flow does not manage
/// their position — it only tracks whether they are active (visible) or
/// minimized. When restored from minimize, the window returns to its
/// `pre_manage_rect` position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FloatingState {
    /// Window is visible and floating at the given rect.
    Active {
        /// Current screen rectangle of the floating window.
        rect: Rect,
    },
    /// Window is minimized to the taskbar.
    Minimized,
    /// Window is hidden from the user (tray-hidden / cloaked / `SW_HIDE`) but
    /// still tracked. The original position is preserved in
    /// [`Window::pre_manage_rect`] so it can be restored on re-show.
    ///
    /// Distinct from [`Minimized`](Self::Minimized): a minimize is an
    /// explicit user action via the taskbar/minimize button, whereas
    /// `Hidden` captures tray-hide (Discord/Steam "close" button) and DWM
    /// cloaking. Both remove the window from the live layout but are
    /// tracked separately for future heuristics.
    Hidden,
}

// ── IgnoredReason ──────────────────────────────────────────────────

/// Reason why a window is ignored by flow.
///
/// Ignored windows are excluded from all layout operations. The reason is
/// tracked for diagnostic purposes (e.g., logs can show *why*
/// a window is not being tiled).
///
/// # Override Priority
///
/// Maximized and fullscreen checks happen **before** config rule evaluation
/// in [`classify_with_state_pipeline`](super::classification::classify_with_state_pipeline).
/// This means a window that is maximized will always be `Ignored(Maximized)`,
/// even if a config rule says to tile it. This is intentional — maximized and
/// fullscreen windows have their own window management behavior that conflicts
/// with tiling.
///
/// `Elevated` wins over everything: no rule, maximize or fullscreen state
/// can make flow manage a window it is not allowed to move.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IgnoredReason {
    /// Window is maximized (`WS_MAXIMIZE` style).
    Maximized,
    /// Window is in exclusive or borderless fullscreen mode.
    Fullscreen,
    /// Window matched an explicit `ignore` rule in the config.
    ExplicitRule,
    /// The window's process is elevated (or its token unreadable) and flowd
    /// is not: UIPI refuses every move, so it is left unmanaged. Decided
    /// once at arrival and never re-evaluated (ADR-0025).
    Elevated,
}

// ── RestoreSeat ─────────────────────────────────────────────────────

/// The seat a tile left when it was minimized or hidden ([`Window::restore_seat`]):
/// the monitor and workspace of its strip, its column and row there,
/// whether it had the column to itself, and the column's width. Read by
/// the daemon's restore and re-show hooks (ADR-0027).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestoreSeat {
    /// Index of the monitor whose strip held the tile.
    pub monitor: usize,
    /// Id of the workspace whose strip held the tile.
    pub workspace: u32,
    /// Column index in that strip.
    pub col: usize,
    /// Row index within the column.
    pub row: usize,
    /// The tile was the only row of its column.
    pub alone: bool,
    /// The column's width in pixels.
    pub width_px: i32,
}

// ── VirtualSlot ─────────────────────────────────────────────────────

/// The registry's column/row of a tile when it was last minimized or
/// hidden ([`Window::last_virtual_slot`]).
///
/// Where the tile goes back is the daemon's [`RestoreSeat`], not this: the
/// registry reads the slot as the placeholder `Active { col, row }` on
/// restore/show (re-synced from the layout once the daemon seats it), and
/// the hotplug reconcile orders the tiles Windows minimized with a display
/// change by it.
///
/// # Default on Restore
///
/// If `last_virtual_slot` is `None` when a tiled window is restored (which
/// shouldn't happen in normal operation), the registry defaults to `(0, 0)`.
/// This ensures the window always gets a valid position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualSlot {
    /// Column index in the virtual layout.
    pub col: usize,
    /// Row index within the column.
    pub row: usize,
}

// ── VisibilityChange ────────────────────────────────────────────────

/// Outcome of a visibility reconciliation pass on a tracked window.
///
/// Returned by [`WindowRegistry::reconcile_visibility`](super::core::WindowRegistry::reconcile_visibility).
/// The daemon layer uses this to decide whether to add/remove the window
/// from the live layout. The [`Unchanged`](VisibilityChange::Unchanged) case
/// is what makes reconciliation **idempotent** against duplicate
/// `EVENT_OBJECT_HIDE` events (e.g. an ordinary minimize also fires HIDE,
/// but the window is already `Minimized` → `Unchanged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityChange {
    /// Window transitioned Active → Hidden (was visible, now not). The caller
    /// should remove it from the live layout.
    Hidden,
    /// Window transitioned Hidden → Active (was hidden, now visible). The
    /// caller should re-add it to the live layout.
    Shown,
    /// No state transition needed: either the window is untracked, already in
    /// the matching state, or already Minimized/Ignored.
    Unchanged,
}

// ── ReclassifyResult ────────────────────────────────────────────────

/// Outcome of an OS-state re-classification pass on a tracked window.
///
/// Returned by [`WindowRegistry::reclassify_os_state`](super::core::WindowRegistry::reclassify_os_state)
/// in response to [`EVENT_OBJECT_STATECHANGE`](super::HookEvent::StateChange)
/// (and to a foreground / show / move-size-end event on an OS-ignored
/// window, which covers an un-maximize whose `STATECHANGE` never arrived).
/// The daemon layer uses this to decide whether to add the window to the live
/// layout: only [`ReclassifyResult::Recovered`] with `now_tiling == true`
/// triggers a layout insertion. With `adopt_maximized` on, a still-maximized
/// window is un-maximized by the registry and reported as `Recovered` too.
///
/// # Why a dedicated result type?
///
/// The daemon must distinguish several "no-op" cases from a genuine recovery:
///
/// - `Untracked` — the `STATECHANGE` was for a window flow does not manage
///   (let `NAMECHANGE`/`CREATE` handle it). Cheap HashMap miss; do nothing.
/// - `NotApplicable` — the window is tracked but not in an OS-ignored state
///   (e.g. it is already tiling, or ignored by an explicit rule). Do nothing.
/// - `Unchanged` — the window is still `Ignored(Maximized|Fullscreen)`; the
///   user has not restored it yet. Do nothing.
/// - `Recovered` — the OS state genuinely changed; the classifier was
///   re-run and the window's stored state was updated. The `now_tiling` flag
///   tells the daemon whether the new state warrants a layout insertion.
///
/// This makes the `STATECHANGE` handler both cheap (early-return on the common
/// no-op cases) and explicit (no spurious layout churn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclassifyResult {
    /// The window is not tracked by flow. No action.
    Untracked,
    /// The window is tracked but was not ignored due to an OS state
    /// (maximized/fullscreen) — e.g. it is already tiling, floating, or
    /// ignored by an explicit rule. OS-state recovery does not apply.
    NotApplicable,
    /// The window is still ignored due to its current OS state
    /// (still maximized or fullscreen). No transition occurred.
    Unchanged,
    /// The OS state changed and the classifier was re-run; the stored state
    /// was updated. When `now_tiling` is true the daemon should add the window
    /// to the live layout.
    Recovered {
        /// Whether the window's new state is `Tiling` (and thus belongs in the
        /// layout). False means it moved to a different ignored/floating state.
        now_tiling: bool,
    },
}
