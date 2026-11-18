//! Windows that stop answering are parked out of the layout (ADR-0035).
//!
//! Windows reports a window whose thread has not pumped messages for five
//! seconds as hung (`IsHungAppWindow`). Every synchronous call that moves,
//! raises or activates a window waits on that thread, so such a window is
//! taken out of the daemon's hands: a tile leaves its strip (its restore
//! seat recorded, the slot handed to the next window), a float leaves the
//! floating layer, and both are left exactly where they are. When the
//! window answers again it comes back: a tile to its seat, a float to the
//! layer. The poll runs on the foreground sync tick and before every
//! command; the Win32 wrappers check the same signal before each call
//! (`registry::win32::is_hung`), which covers the moment in between.

use crate::common::WindowId;
use crate::registry::win32 as registry_win32;

use super::FlowWM;

impl FlowWM {
    /// Park every active tile and float that Windows reports hung, and
    /// bring back every parked one that answers again.
    pub(super) fn reconcile_hung(&mut self) {
        // Nothing is moved for a desktop nobody sees (ADR-0034).
        if self.registry.session_locked() {
            return;
        }
        for (hwnd, parked) in self.registry.responsiveness_watch() {
            let hung = registry_win32::is_hung(hwnd);
            if hung && !parked {
                self.park_hung(hwnd);
            } else if !hung && parked {
                self.unpark_hung(hwnd);
            }
        }
    }

    /// A foreground or other event for a parked window that answers again
    /// brings it back now rather than on the next tick.
    pub(super) fn unpark_if_answering(&mut self, hwnd: isize) {
        let parked = self
            .registry
            .responsiveness_watch()
            .iter()
            .any(|&(h, p)| h == hwnd && p);
        if parked && !registry_win32::is_hung(hwnd) {
            self.unpark_hung(hwnd);
        }
    }

    pub(super) fn park_hung(&mut self, hwnd: isize) {
        let window = WindowId(hwnd);
        // A keymap drag session on it ends (the window cannot follow).
        self.abandon_manual_drag_for(hwnd);
        let was_tile = self.registry.is_tiling_active(hwnd);
        // Tiling off: the registry records it, nothing else moves.
        if self.tiling_paused {
            let _ = self.registry.mark_hung(hwnd);
            return;
        }
        if was_tile {
            // Read while it still has a seat; it goes back there.
            self.capture_restore_seat(window, "not responding");
        }
        if !self.registry.mark_hung(hwnd) {
            return;
        }
        // Its riders stay where they are with it (ADR-0038).
        self.note_anchor_hung(hwnd);
        if was_tile {
            self.remove_hung_tile(window);
            self.forget_submitted(hwnd);
        }
        // Out of the floating layer (it is no longer an active float).
        self.sync_top_layer(hwnd);
        self.refresh_border_for(hwnd);
    }

    /// Take a parked tile out of its strip and close the gap. Unlike a
    /// minimize the foreground is not pushed: Windows' `Ghost` stand-in for
    /// the frozen window holds it, and the user may be waiting on that
    /// window. Only the strip's focus moves to the successor, so the next
    /// focus command starts there.
    fn remove_hung_tile(&mut self, window: WindowId) {
        let idx = self.monitor_index_for_window(window);
        let active_id = self.monitors[idx].active_workspace_id();
        let home = self.monitors[idx]
            .find_workspace_containing(window)
            .unwrap_or(active_id);
        if home != active_id {
            self.remove_from_parked_workspace(idx, home, window);
            return;
        }
        let applied = self.monitors[idx]
            .active_scrolling_mut()
            .remove_window(window);
        if self.registry.focused() == Some(window) {
            match self.monitors[idx].active_scrolling().last_focused_window() {
                Some(successor) => {
                    self.registry.set_focused(successor.0);
                    self.refresh_border_for(successor.0);
                    log::debug!(
                        "window {:#x} not responding: the strip's focus moves to {:#x}",
                        window.0,
                        successor.0
                    );
                }
                None => self.drop_tracked_focus(),
            }
        }
        self.animate_layout_on(idx, &applied);
    }

    pub(super) fn unpark_hung(&mut self, hwnd: isize) {
        if !self.registry.mark_responding(hwnd) || self.tiling_paused {
            return;
        }
        if self.registry.is_tiling_active(hwnd) {
            // Back into its seat; the view stays where the user left it.
            self.readd_tile_to_seat(hwnd);
        }
        self.sync_top_layer(hwnd);
        self.refresh_border_for(hwnd);
    }
}
