//! Keymap-driven drag sessions: `flow dispatch drag begin` / `drag end`.
//!
//! A hook hotkey such as `#LButton` swallows the button-down, after which no
//! way of starting Windows' own move loop survives, so the keymap moves the
//! window itself (`WinMove` while the button is held) and tells the daemon
//! where the gesture begins and ends. `drag begin` puts the focused window
//! into exactly the state a `MoveSizeStart` would — a tile enters
//! `Classifying` (border follows, drop zone from the `LOCATIONCHANGE`
//! stream), a float enters [`DragMode::Float`](super::drag::DragMode::Float)
//! (its rect stored live, the animator off it) — and `drag end` runs the
//! `MoveSizeEnd` path (commit or snap back; a float's final rect recorded
//! once more). The decision logic is pure ([`decide_begin`],
//! [`native_event_is_shadowed`], [`ends_manual_drag`]); the Win32 reads go
//! through [`DragProbe`] so the whole begin → moves → end sequence runs
//! against a scripted desktop in the tests below.
//!
//! (`docs/src/dev-guide/tile-drag.md`, *Keymap-driven sessions*)

use windows::Win32::Foundation::HWND;

use crate::ipc::message::{SocketMessage, SocketResponse};
use crate::registry::hooks::clear_dragged_hwnd;
use crate::registry::types::{FloatingState, TilingState, WindowState};

use super::drag::{DragProbe, Win32Drag};
use super::types::FlowWM;

/// What a session drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ManualDragKind {
    /// A tiled window: the tile-drag state machine is live.
    Tile,
    /// A floating window: `DragMode::Float` is live, its rect is stored on
    /// every move.
    Float,
}

impl ManualDragKind {
    /// The word the IPC reply carries (`"tile"` / `"float"`).
    #[must_use]
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ManualDragKind::Tile => "tile",
            ManualDragKind::Float => "float",
        }
    }
}

/// An open session: `drag begin` answered, `drag end` not yet received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ManualDrag {
    /// The window the keymap is moving.
    pub(super) hwnd: isize,
    /// Tile or float.
    pub(super) kind: ManualDragKind,
}

/// The focused window as `drag begin` sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DragTarget {
    /// An active tile.
    Tile,
    /// An active float.
    Float,
    /// A fullscreen window (a float the daemon places itself).
    Fullscreen,
    /// Minimized.
    Minimized,
    /// Tray-hidden / cloaked.
    Hidden,
    /// Ignored by the rules or the OS state.
    Ignored,
    /// Not in the registry at all.
    Untracked,
}

/// Why `drag begin` was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BeginRefusal {
    /// `flow dispatch tiling off` is in effect.
    TilingOff,
    /// A native or keymap drag is already in progress.
    DragActive,
    /// Nothing has the focus.
    NoFocus,
    /// The focused window is not a tile or a float.
    NotDraggable(DragTarget),
}

impl BeginRefusal {
    /// The error message the CLI prints.
    #[must_use]
    pub(super) fn message(&self) -> String {
        match self {
            BeginRefusal::TilingOff => super::tiling::TILING_OFF_MESSAGE.to_owned(),
            BeginRefusal::DragActive => "a drag is already in progress".to_owned(),
            BeginRefusal::NoFocus => "no focused window to drag".to_owned(),
            BeginRefusal::NotDraggable(t) => {
                let what = match t {
                    DragTarget::Fullscreen => "fullscreen",
                    DragTarget::Minimized => "minimized",
                    DragTarget::Hidden => "hidden",
                    DragTarget::Ignored => "not managed (ignored)",
                    DragTarget::Untracked => "not managed",
                    DragTarget::Tile | DragTarget::Float => "not draggable",
                };
                format!("focused window is {what}: nothing to drag")
            }
        }
    }
}

/// Decide whether `drag begin` opens a session and of which kind.
///
/// `drag_active` covers both a native title-bar drag (`drag_state`) and an
/// open session; `focused` is the focused window's state, `None` when
/// nothing has the focus.
pub(super) fn decide_begin(
    tiling_paused: bool,
    drag_active: bool,
    focused: Option<DragTarget>,
) -> Result<ManualDragKind, BeginRefusal> {
    if tiling_paused {
        return Err(BeginRefusal::TilingOff);
    }
    if drag_active {
        return Err(BeginRefusal::DragActive);
    }
    match focused {
        None => Err(BeginRefusal::NoFocus),
        Some(DragTarget::Tile) => Ok(ManualDragKind::Tile),
        Some(DragTarget::Float) => Ok(ManualDragKind::Float),
        Some(other) => Err(BeginRefusal::NotDraggable(other)),
    }
}

/// Whether a native `MoveSizeStart` / `MoveSizeEnd` for `hwnd` belongs to
/// the session's own window and must be ignored: the keymap owns that
/// window's drag lifecycle until `drag end`.
#[must_use]
pub(super) fn native_event_is_shadowed(session: Option<ManualDrag>, hwnd: isize) -> bool {
    session.is_some_and(|s| s.hwnd == hwnd)
}

/// The commands that take the desktop away from an open session and
/// therefore end it first (so the busy gate cannot refuse them): `tiling
/// off|on|toggle` and `flow stop`.
#[must_use]
pub(super) fn ends_manual_drag(msg: &SocketMessage) -> bool {
    matches!(
        msg,
        SocketMessage::Stop | SocketMessage::SetTiling { .. } | SocketMessage::ToggleTiling
    )
}

impl FlowWM {
    /// Whether a native drag event for `hwnd` is shadowed by an open session
    /// (see [`native_event_is_shadowed`]).
    #[must_use]
    pub(super) fn native_drag_event_shadowed(&self, hwnd: isize) -> bool {
        let shadowed = native_event_is_shadowed(self.manual_drag, hwnd);
        if shadowed {
            log::debug!("drag session: native move-size event for {hwnd} ignored");
        }
        shadowed
    }

    /// How `drag begin` sees `hwnd`.
    fn drag_target_of(&self, hwnd: isize) -> DragTarget {
        let Some(window) = self.registry.get_window(HWND(hwnd as *mut _)) else {
            return DragTarget::Untracked;
        };
        match &window.state {
            WindowState::Tiling(TilingState::Active { .. }) if self.is_fullscreen(hwnd) => {
                DragTarget::Fullscreen
            }
            WindowState::Tiling(TilingState::Active { .. }) => DragTarget::Tile,
            WindowState::Floating(FloatingState::Active { .. }) => DragTarget::Float,
            WindowState::Tiling(TilingState::Minimized)
            | WindowState::Floating(FloatingState::Minimized) => DragTarget::Minimized,
            WindowState::Tiling(TilingState::Hidden)
            | WindowState::Floating(FloatingState::Hidden) => DragTarget::Hidden,
            WindowState::Ignored(_) => DragTarget::Ignored,
        }
    }

    /// `flow dispatch drag begin`: answered with `Data { "drag": "tile" |
    /// "float", "hwnd" }` or an error naming the refusal.
    pub(super) fn dispatch_drag_begin(&mut self) -> SocketResponse {
        match self.begin_manual_drag_with(&Win32Drag) {
            Ok(session) => SocketResponse::Data {
                payload: serde_json::json!({
                    "drag": session.kind.as_str(),
                    "hwnd": session.hwnd,
                }),
            },
            Err(refusal) => SocketResponse::Error {
                message: refusal.message(),
            },
        }
    }

    /// `flow dispatch drag end`: `Data { "ended": true, "drag": kind }`, or
    /// `Data { "ended": false }` when no session is open (a no-op).
    pub(super) fn dispatch_drag_end(&mut self) -> SocketResponse {
        match self.end_manual_drag_with(&Win32Drag) {
            Some(session) => SocketResponse::Data {
                payload: serde_json::json!({ "ended": true, "drag": session.kind.as_str() }),
            },
            None => SocketResponse::Data {
                payload: serde_json::json!({ "ended": false }),
            },
        }
    }

    /// Open a session on the focused window (the OS foreground, else the
    /// registry's focus): the window enters the drag state machine exactly
    /// as on `MoveSizeStart` (`Classifying` for a tile, `Float` for a float).
    pub(super) fn begin_manual_drag_with<P: DragProbe>(
        &mut self,
        probe: &P,
    ) -> Result<ManualDrag, BeginRefusal> {
        let focused = match self.os_foreground_for_dispatch() {
            Some(fg) => {
                self.registry.set_focused(fg);
                Some(fg)
            }
            None => self.registry.focused().map(|w| w.0),
        };
        let drag_active = self.drag_state.is_some() || self.manual_drag.is_some();
        let target = focused.map(|h| self.drag_target_of(h));
        let kind = decide_begin(self.tiling_paused, drag_active, target)?;
        let hwnd = focused.expect("decide_begin accepted a focused window");
        self.on_drag_start_with(probe, hwnd);
        if self.drag_state.is_none() {
            // The registry said tile or float but the drag machine refused:
            // the window changed under us. Nothing to drive.
            return Err(BeginRefusal::NotDraggable(match kind {
                ManualDragKind::Tile => DragTarget::Tile,
                ManualDragKind::Float => DragTarget::Float,
            }));
        }
        let session = ManualDrag { hwnd, kind };
        self.manual_drag = Some(session);
        log::info!("drag session: begin {} {hwnd}", kind.as_str());
        Ok(session)
    }

    /// Close the session the `MoveSizeEnd` way: a tile commits its drop
    /// zone (or snaps back); a float records its final rect once more.
    /// `None` when no session is open.
    pub(super) fn end_manual_drag_with<P: DragProbe>(&mut self, probe: &P) -> Option<ManualDrag> {
        let session = self.manual_drag.take()?;
        self.on_drag_end_with(probe, session.hwnd);
        log::info!(
            "drag session: end {} {}",
            session.kind.as_str(),
            session.hwnd
        );
        Some(session)
    }

    /// [`end_manual_drag_with`](Self::end_manual_drag_with) on the live desktop.
    pub(super) fn end_manual_drag(&mut self) -> Option<ManualDrag> {
        self.end_manual_drag_with(&Win32Drag)
    }

    /// Drop the session without committing anything: the drag state and
    /// the `DRAGGED_HWND` bridge are released, the window is left where it
    /// is. Used when the window disappears mid-session, by `tiling off` and
    /// by the fail-back. No-op without a session.
    pub(super) fn abandon_manual_drag(&mut self) {
        let Some(session) = self.manual_drag.take() else {
            return;
        };
        if self
            .drag_state
            .as_ref()
            .is_some_and(|d| d.dragged_hwnd() == session.hwnd)
        {
            self.drag_state = None;
            clear_dragged_hwnd();
        }
        log::info!(
            "drag session: abandoned {} {}",
            session.kind.as_str(),
            session.hwnd
        );
    }

    /// [`abandon_manual_drag`](Self::abandon_manual_drag) when `hwnd` is the
    /// session's window (it was destroyed, minimized or hidden).
    pub(super) fn abandon_manual_drag_for(&mut self, hwnd: isize) {
        if self.manual_drag.is_some_and(|s| s.hwnd == hwnd) {
            self.abandon_manual_drag();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::common::{Rect, WindowId};
    use crate::config::types::WindowRulesConfig;
    use crate::layout::types::{MonitorInfo, Padding};
    use crate::registry::WindowRegistry;
    use crate::registry::win32 as registry_win32;
    use crate::workspace::{Monitor, ScrollingSpace, Workspace, WorkspaceId};

    // ── pure decisions ────────────────────────────────────────────────

    #[test]
    fn begin_opens_a_tile_or_float_session() {
        assert_eq!(
            decide_begin(false, false, Some(DragTarget::Tile)),
            Ok(ManualDragKind::Tile)
        );
        assert_eq!(
            decide_begin(false, false, Some(DragTarget::Float)),
            Ok(ManualDragKind::Float)
        );
    }

    #[test]
    fn begin_is_refused_in_order_tiling_off_then_drag_active_then_focus() {
        assert_eq!(
            decide_begin(true, true, Some(DragTarget::Tile)),
            Err(BeginRefusal::TilingOff)
        );
        assert_eq!(
            decide_begin(false, true, Some(DragTarget::Tile)),
            Err(BeginRefusal::DragActive)
        );
        assert_eq!(decide_begin(false, false, None), Err(BeginRefusal::NoFocus));
        for t in [
            DragTarget::Fullscreen,
            DragTarget::Minimized,
            DragTarget::Hidden,
            DragTarget::Ignored,
            DragTarget::Untracked,
        ] {
            assert_eq!(
                decide_begin(false, false, Some(t)),
                Err(BeginRefusal::NotDraggable(t))
            );
        }
    }

    #[test]
    fn refusal_messages_name_the_cause() {
        assert_eq!(
            BeginRefusal::DragActive.message(),
            "a drag is already in progress"
        );
        assert_eq!(BeginRefusal::NoFocus.message(), "no focused window to drag");
        assert!(
            BeginRefusal::NotDraggable(DragTarget::Untracked)
                .message()
                .contains("not managed")
        );
        assert!(BeginRefusal::TilingOff.message().contains("tiling is off"));
    }

    #[test]
    fn native_events_for_the_session_window_are_shadowed() {
        let s = Some(ManualDrag {
            hwnd: 7,
            kind: ManualDragKind::Tile,
        });
        assert!(native_event_is_shadowed(s, 7));
        assert!(!native_event_is_shadowed(s, 8));
        assert!(!native_event_is_shadowed(None, 7));
    }

    #[test]
    fn stop_and_tiling_commands_end_a_session_other_commands_do_not() {
        assert!(ends_manual_drag(&SocketMessage::Stop));
        assert!(ends_manual_drag(&SocketMessage::SetTiling {
            enabled: false
        }));
        assert!(ends_manual_drag(&SocketMessage::ToggleTiling));
        assert!(!ends_manual_drag(&SocketMessage::FocusLeft));
        assert!(!ends_manual_drag(&SocketMessage::DragEnd));
        assert!(!ends_manual_drag(&SocketMessage::Ping));
    }

    // ── a scripted desktop for the begin → moves → end sequences ──────

    /// The tests below drive the process-wide `DRAGGED_HWND` bridge, so
    /// they take turns.
    static DRAG_GLOBAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        DRAG_GLOBAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    const A: isize = 0x1001;
    const B: isize = 0x1002;
    const C: isize = 0x1003;
    const F: isize = 0x2001;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[derive(Default)]
    struct ScriptedDesktop {
        rects: RefCell<HashMap<isize, Rect>>,
        cursor: Cell<(i32, i32)>,
        iconic: HashSet<isize>,
    }

    impl ScriptedDesktop {
        fn place(&self, hwnd: isize, rect: Rect) {
            self.rects.borrow_mut().insert(hwnd, rect);
        }
    }

    impl DragProbe for ScriptedDesktop {
        fn window_rect(&self, hwnd: isize) -> Option<Rect> {
            self.rects.borrow().get(&hwnd).copied()
        }
        fn cursor_pos(&self) -> Option<(i32, i32)> {
            Some(self.cursor.get())
        }
        fn is_iconic(&self, hwnd: isize) -> bool {
            self.iconic.contains(&hwnd)
        }
    }

    fn register(registry: &mut WindowRegistry, hwnd: isize, rect: Rect, state: WindowState) {
        let info = registry_win32::WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            title: format!("W{hwnd:x}"),
            class: "TestClass".into(),
            rect,
            exe: "test.exe".into(),
            process_path: "C:\\test.exe".into(),
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

    fn tile() -> WindowState {
        WindowState::Tiling(TilingState::Active { col: 0, row: 0 })
    }

    /// One 1920x1080 monitor, 960 px columns: tiles A, B (on screen) and C
    /// (parked right), float F at (100,100,400,300). A has the focus.
    fn fixture(desktop: &ScriptedDesktop) -> FlowWM {
        let rules = WindowRulesConfig::default();
        let mut registry = WindowRegistry::new(&rules, &rules);
        for hwnd in [A, B, C] {
            register(&mut registry, hwnd, r(0, 0, 800, 600), tile());
        }
        let float_rect = r(100, 100, 400, 300);
        register(
            &mut registry,
            F,
            float_rect,
            WindowState::Floating(FloatingState::Active { rect: float_rect }),
        );
        let screen = r(0, 0, 1920, 1080);
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let mut scrolling =
            ScrollingSpace::new(MonitorInfo::single(screen), 960, 320, 100, 100, padding, 4);
        for hwnd in [A, B, C] {
            scrolling.add_window(WindowId(hwnd));
        }
        registry.update_tiling_slots_from_layout(scrolling.virtual_layout());
        registry.update_tiled_rects(scrolling.actual_layout());
        for entry in &scrolling.actual_layout().entries {
            desktop.place(entry.window_id.0, entry.rect);
        }
        desktop.place(F, float_rect);
        let mut ws = Workspace::new(WorkspaceId(1), scrolling);
        ws.floating.add(WindowId(F), float_rect);
        let monitor = Monitor::new(screen, screen, vec![ws], 0);
        for hwnd in [A, B, C, F] {
            registry.set_monitor(hwnd, 0);
        }
        registry.set_focused(A);
        let mut flow = FlowWM::new_for_test(registry, vec![monitor]);
        // No overlay windows on the developer's desktop.
        flow.config.borders.enabled = false;
        flow
    }

    fn column_order(flow: &FlowWM) -> Vec<isize> {
        flow.monitors[0]
            .active_scrolling()
            .virtual_layout()
            .columns
            .iter()
            .flat_map(|c| c.rows.iter().map(|row| row.window_id.0))
            .collect()
    }

    fn tile_rect(flow: &FlowWM, hwnd: isize) -> Rect {
        flow.monitors[0]
            .active_scrolling()
            .actual_layout()
            .find(WindowId(hwnd))
            .expect("tile projected")
            .rect
    }

    fn stored_float_rect(flow: &FlowWM, hwnd: isize) -> Rect {
        flow.monitors[0]
            .active_workspace()
            .floating
            .windows()
            .iter()
            .find(|e| e.window_id == WindowId(hwnd))
            .expect("float seated")
            .rect
    }

    // ── tile: begin → location changes → end reorders the columns ─────

    #[test]
    fn tile_session_begin_moves_end_reorders_the_strip() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = fixture(&desktop);
        assert_eq!(column_order(&flow), vec![A, B, C]);

        let session = flow.begin_manual_drag_with(&desktop).expect("tile session");
        assert_eq!(session.kind, ManualDragKind::Tile);
        assert_eq!(session.hwnd, A);
        assert!(flow.drag_state.is_some(), "the tile drag machine is live");
        assert_eq!(crate::registry::hooks::dragged_hwnd(), A);

        // The keymap moves A over B's column: the cursor lands in the
        // outer third of B's right side (inside the column, outside the
        // twelfth-of-the-screen monitor-edge scroll band), the window rect
        // follows the cursor.
        let b = tile_rect(&flow, B);
        let a0 = tile_rect(&flow, A);
        let cursor = (b.right() - 200, b.y + b.height / 2);
        desktop.cursor.set(cursor);
        for step in 1..=5 {
            let dx = (cursor.0 - (a0.x + a0.width / 2)) * step / 5;
            desktop.place(A, r(a0.x + dx, a0.y, a0.width, a0.height));
            flow.on_drag_move_with(&desktop, A);
        }
        assert!(
            matches!(
                flow.drag_state,
                Some(super::super::drag::DragMode::Translate(_))
            ),
            "a pure translation classifies as a title-bar reorder"
        );
        // Still frozen: nothing commits before the end.
        assert_eq!(column_order(&flow), vec![A, B, C]);

        let ended = flow.end_manual_drag_with(&desktop).expect("session ended");
        assert_eq!(ended.hwnd, A);
        assert!(flow.drag_state.is_none());
        assert!(flow.manual_drag.is_none());
        assert_eq!(crate::registry::hooks::dragged_hwnd(), 0);
        assert_eq!(column_order(&flow), vec![B, A, C], "A dropped after B");

        // A second end is a no-op.
        assert_eq!(flow.end_manual_drag_with(&desktop), None);
    }

    #[test]
    fn begin_is_refused_while_a_session_or_a_native_drag_is_open() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = fixture(&desktop);
        flow.begin_manual_drag_with(&desktop).expect("first");
        assert_eq!(
            flow.begin_manual_drag_with(&desktop),
            Err(BeginRefusal::DragActive)
        );
        flow.end_manual_drag_with(&desktop);
        // A native title-bar drag of B is in progress.
        flow.registry.set_focused(B);
        flow.on_drag_start_with(&desktop, B);
        assert!(flow.drag_state.is_some());
        assert_eq!(
            flow.begin_manual_drag_with(&desktop),
            Err(BeginRefusal::DragActive)
        );
        flow.on_drag_end_with(&desktop, B);
    }

    #[test]
    fn begin_is_refused_for_an_unmanaged_focus_and_with_tiling_off() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = fixture(&desktop);
        flow.registry.set_focused(0x999);
        assert!(
            flow.registry.focused().is_some_and(|w| w.0 == A),
            "unknown focus ignored"
        );
        flow.tiling_paused = true;
        assert_eq!(
            flow.begin_manual_drag_with(&desktop),
            Err(BeginRefusal::TilingOff)
        );
        flow.tiling_paused = false;
        // A minimized tile cannot be dragged.
        if let Some(w) = flow.registry.get_window_mut(HWND(A as *mut _)) {
            w.state = WindowState::Tiling(TilingState::Minimized);
        }
        assert_eq!(
            flow.begin_manual_drag_with(&desktop),
            Err(BeginRefusal::NotDraggable(DragTarget::Minimized))
        );
        assert!(flow.manual_drag.is_none());
    }

    #[test]
    fn a_destroyed_session_window_abandons_the_session_cleanly() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = fixture(&desktop);
        flow.begin_manual_drag_with(&desktop).expect("tile session");
        flow.on_window_destroyed(A);
        assert!(flow.manual_drag.is_none());
        assert!(flow.drag_state.is_none());
        assert_eq!(crate::registry::hooks::dragged_hwnd(), 0);
        assert_eq!(column_order(&flow), vec![B, C]);
        assert_eq!(flow.end_manual_drag_with(&desktop), None);
        // And a fresh session can open on the new focus.
        flow.registry.set_focused(B);
        assert!(flow.begin_manual_drag_with(&desktop).is_ok());
        flow.abandon_manual_drag();
        assert!(flow.drag_state.is_none());
    }

    // ── float: begin → location changes → end keeps the rect ─────────

    #[test]
    fn float_session_enters_the_float_drag_state_and_stores_the_rect_live() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = fixture(&desktop);
        flow.registry.set_focused(F);
        let start = r(100, 100, 400, 300);
        assert_eq!(stored_float_rect(&flow, F), start);

        let session = flow
            .begin_manual_drag_with(&desktop)
            .expect("float session");
        assert_eq!(session.kind, ManualDragKind::Float);
        assert!(
            matches!(
                flow.drag_state,
                Some(super::super::drag::DragMode::Float(_))
            ),
            "a float enters the one drag state, as a tile does"
        );
        assert_eq!(crate::registry::hooks::dragged_hwnd(), F);
        assert!(flow.native_drag_event_shadowed(F));
        assert!(!flow.native_drag_event_shadowed(A));

        // The keymap moves the window; run.rs routes the LOCATIONCHANGE to
        // the drag path, which stores the rect live (ADR-0016).
        for step in 1..=3 {
            let here = r(100 + 100 * step, 100, 400, 300);
            desktop.place(F, here);
            flow.on_drag_move_with(&desktop, F);
            assert_eq!(stored_float_rect(&flow, F), here, "stored on every move");
        }
        // A floating-space animation meanwhile (another float appearing)
        // leaves the dragged window alone: the animator excludes whatever
        // `drag_state` names, tile or float.
        let batch = flow.monitors[0]
            .active_workspace()
            .floating
            .to_actual_layout();
        let targets = flow.workspace_targets(
            0,
            &[(batch, 0)],
            super::super::animation::PrevRects::Registry,
        );
        assert!(
            targets.targets.iter().all(|t| t.window_ref.0 != F),
            "the dragged float is not re-submitted"
        );

        let ended = flow.end_manual_drag_with(&desktop).expect("session ended");
        assert_eq!(ended.kind, ManualDragKind::Float);
        assert!(flow.drag_state.is_none());
        assert_eq!(crate::registry::hooks::dragged_hwnd(), 0);
        let moved = r(400, 100, 400, 300);
        assert_eq!(stored_float_rect(&flow, F), moved, "final rect kept");
        let mirrored = flow
            .registry
            .get_window(HWND(F as *mut _))
            .map(|w| matches!(w.state, WindowState::Floating(FloatingState::Active { rect }) if rect == moved));
        assert_eq!(mirrored, Some(true), "registry mirror follows");
        // Sync writes flow as before.
        desktop.place(F, r(500, 100, 400, 300));
        flow.store_float_rect_with(&desktop, F);
        assert_eq!(stored_float_rect(&flow, F), r(500, 100, 400, 300));
    }

    /// The last `WinMove` of a session can still be in the hook queue when
    /// `drag end` is read from the pipe: the end stores the rect once more.
    #[test]
    fn float_session_end_records_a_move_the_hook_queue_still_holds() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = fixture(&desktop);
        flow.registry.set_focused(F);
        flow.begin_manual_drag_with(&desktop)
            .expect("float session");
        desktop.place(F, r(700, 150, 400, 300));
        // No on_drag_move for this placement: the LOCATIONCHANGE is "late".
        flow.end_manual_drag_with(&desktop).expect("ended");
        assert_eq!(stored_float_rect(&flow, F), r(700, 150, 400, 300));
    }

    /// A native title-bar drag of a float takes the same path as a session:
    /// `MoveSizeStart` enters `Float`, the moves store live, `MoveSizeEnd`
    /// clears the state with no commit.
    #[test]
    fn native_float_drag_enters_the_float_state_and_stores_live() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = fixture(&desktop);
        flow.on_drag_start_with(&desktop, F);
        assert!(matches!(
            flow.drag_state,
            Some(super::super::drag::DragMode::Float(_))
        ));
        assert!(flow.manual_drag.is_none(), "a native drag is not a session");
        assert_eq!(crate::registry::hooks::dragged_hwnd(), F);
        // `drag begin` is refused meanwhile, like during any native drag.
        assert_eq!(
            flow.begin_manual_drag_with(&desktop),
            Err(BeginRefusal::DragActive)
        );
        desktop.place(F, r(250, 260, 400, 300));
        flow.on_drag_move_with(&desktop, F);
        assert_eq!(stored_float_rect(&flow, F), r(250, 260, 400, 300));
        flow.on_drag_end_with(&desktop, F);
        assert!(flow.drag_state.is_none());
        assert_eq!(crate::registry::hooks::dragged_hwnd(), 0);
        assert_eq!(stored_float_rect(&flow, F), r(250, 260, 400, 300));
        assert_eq!(column_order(&flow), vec![A, B, C], "the strip is untouched");
    }

    #[test]
    fn float_session_with_no_movement_keeps_the_size() {
        // Regression for the 4 px shrink: begin + end with the window
        // untouched records the same rect, whatever the invisible frame.
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = fixture(&desktop);
        flow.registry.set_focused(F);
        let bounds = crate::common::InvisibleBounds {
            left: 7,
            top: 0,
            right: 7,
            bottom: 7,
        };
        if let Some(w) = flow.registry.get_window_mut(HWND(F as *mut _)) {
            w.invisible_bounds = bounds;
        }
        let visible = r(100, 100, 400, 300);
        desktop.place(F, bounds.visible_to_window(visible));
        for _ in 0..3 {
            flow.begin_manual_drag_with(&desktop)
                .expect("float session");
            flow.store_float_rect_with(&desktop, F);
            flow.end_manual_drag_with(&desktop).expect("ended");
            assert_eq!(stored_float_rect(&flow, F), visible);
        }
    }

    // ── cross-screen tile drops (ADR-0018) ────────────────────────────

    const D: isize = 0x1004;

    /// The single-monitor fixture plus a second 1920x1080 monitor to its
    /// right (x 1920..3840) holding tile D, or nothing when `empty`.
    fn two_monitor_fixture(desktop: &ScriptedDesktop, empty: bool) -> FlowWM {
        let mut flow = fixture(desktop);
        let screen = r(1920, 0, 1920, 1080);
        let padding = Padding {
            window_gap: 4,
            up: 0,
            down: 0,
        };
        let mut scrolling =
            ScrollingSpace::new(MonitorInfo::single(screen), 960, 320, 100, 100, padding, 4);
        if !empty {
            register(&mut flow.registry, D, r(0, 0, 800, 600), tile());
            scrolling.add_window(WindowId(D));
            flow.registry
                .update_tiling_slots_from_layout(scrolling.virtual_layout());
            flow.registry.update_tiled_rects(scrolling.actual_layout());
            for entry in &scrolling.actual_layout().entries {
                desktop.place(entry.window_id.0, entry.rect);
            }
            flow.registry.set_monitor(D, 1);
        }
        let ws = Workspace::new(WorkspaceId(1), scrolling);
        flow.monitors
            .push(Monitor::new(screen, screen, vec![ws], 0));
        flow
    }

    fn order_on(flow: &FlowWM, mi: usize) -> Vec<isize> {
        flow.monitors[mi]
            .active_scrolling()
            .virtual_layout()
            .columns
            .iter()
            .flat_map(|c| c.rows.iter().map(|row| row.window_id.0))
            .collect()
    }

    /// Drive a tile session for A through `path` (cursor points; the
    /// window follows each one), then end it.
    fn drag_a_through(flow: &mut FlowWM, desktop: &ScriptedDesktop, path: &[(i32, i32)]) {
        flow.begin_manual_drag_with(desktop).expect("tile session");
        let a0 = tile_rect(flow, A);
        for &(x, y) in path {
            desktop.cursor.set((x, y));
            desktop.place(A, r(x - a0.width / 2, y - 20, a0.width, a0.height));
            flow.on_drag_move_with(desktop, A);
        }
        flow.end_manual_drag_with(desktop).expect("session ended");
    }

    #[test]
    fn a_tile_dropped_on_another_monitor_joins_that_strip_at_the_zone() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = two_monitor_fixture(&desktop, false);
        // D spans x 1924..2884; its right outer third starts at 2884 - 316.
        drag_a_through(&mut flow, &desktop, &[(900, 500), (1500, 500), (2800, 500)]);
        assert_eq!(order_on(&flow, 0), vec![B, C], "A left its own strip");
        assert_eq!(order_on(&flow, 1), vec![D, A], "A landed right of D");
        assert_eq!(flow.monitor_index_for_window(WindowId(A)), 1);
        assert_eq!(flow.remembered_monitor(WindowId(A)), Some(1));
        assert_eq!(flow.active_monitor, 1, "the target monitor is active");
        assert_eq!(
            flow.monitors[1].active_scrolling().last_focused_window(),
            Some(WindowId(A))
        );
    }

    #[test]
    fn a_cross_screen_drop_into_the_middle_third_stacks_by_window_half() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = two_monitor_fixture(&desktop, false);
        // D's middle third, top half: A goes above D.
        drag_a_through(&mut flow, &desktop, &[(1500, 500), (2400, 200)]);
        let cols = &flow.monitors[1].active_scrolling().virtual_layout().columns;
        assert_eq!(cols.len(), 1);
        let ids: Vec<isize> = cols[0].rows.iter().map(|r| r.window_id.0).collect();
        assert_eq!(ids, vec![A, D]);
    }

    #[test]
    fn a_drag_that_returns_to_its_own_screen_does_not_cross() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = two_monitor_fixture(&desktop, false);
        let a = tile_rect(&flow, A);
        // Over D, then back to A's own tile (its middle third, top half).
        drag_a_through(
            &mut flow,
            &desktop,
            &[(2400, 500), (2800, 500), (a.x + a.width / 2, a.y + 100)],
        );
        assert_eq!(order_on(&flow, 0), vec![A, B, C]);
        assert_eq!(order_on(&flow, 1), vec![D]);
        assert_eq!(flow.monitor_index_for_window(WindowId(A)), 0);
    }

    #[test]
    fn a_release_off_every_monitor_snaps_back() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = two_monitor_fixture(&desktop, false);
        drag_a_through(&mut flow, &desktop, &[(2400, 500), (2400, 1500)]);
        assert_eq!(order_on(&flow, 0), vec![A, B, C]);
        assert_eq!(order_on(&flow, 1), vec![D]);
    }

    #[test]
    fn a_tile_dropped_on_an_empty_monitor_becomes_its_first_column() {
        let _turn = serialized();
        let desktop = ScriptedDesktop::default();
        let mut flow = two_monitor_fixture(&desktop, true);
        drag_a_through(&mut flow, &desktop, &[(1500, 500), (2800, 500)]);
        assert_eq!(order_on(&flow, 0), vec![B, C]);
        assert_eq!(order_on(&flow, 1), vec![A]);
    }
}
