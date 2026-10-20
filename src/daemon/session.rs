//! Session-end and console-control signals.
//!
//! The daemon has no window of its own except the border overlays — and
//! those only exist while there are windows to decorate. `WM_QUERYENDSESSION`
//! / `WM_ENDSESSION` (logoff, shutdown) are delivered per top-level window,
//! so a daemon with no overlay would never hear about a logoff and would
//! leave parked windows off-screen for the apps that persist their own
//! rects. [`SessionSink`] is a permanent, invisible top-level window created
//! on the main thread during the `hooks` stage; its window procedure turns
//! those messages (and `WM_CLOSE`) into
//! [`request_shutdown`](crate::registry::hooks::request_shutdown), which the
//! event loop treats like a `Stop` command.
//!
//! A *message-only* window (`HWND_MESSAGE`) would not do: those are
//! excluded from broadcasts and never receive session messages.
//!
//! [`install_console_ctrl_handler`] covers console runs (`flowd` started in
//! a terminal): Ctrl-C, Ctrl-Break, closing the console, logoff and shutdown
//! all request the same graceful shutdown. For the last three Windows kills
//! the process the moment the handler returns, so the handler waits
//! (bounded) for the main thread to finish its rescue first.

use std::sync::OnceLock;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    SetConsoleCtrlHandler,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, SPI_SETWORKAREA,
    WINDOW_EX_STYLE, WM_CLOSE, WM_DISPLAYCHANGE, WM_ENDSESSION, WM_QUERYENDSESSION,
    WM_SETTINGCHANGE, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BOOL, PCWSTR, w};

use crate::registry::hooks::{request_shutdown, request_topology_refresh, wait_shutdown_complete};

/// Window class of the session sink.
const SINK_CLASS_NAME: PCWSTR = w!("FlowSessionSink");

/// Class atom once registered (idempotent registration).
static SINK_CLASS_ATOM: OnceLock<u16> = OnceLock::new();

/// Longest a console control handler blocks waiting for the main thread's
/// rescue before letting Windows terminate the process anyway. Windows
/// itself allows roughly five seconds for `CTRL_CLOSE_EVENT`.
pub const CONSOLE_HANDLER_GRACE: Duration = Duration::from_millis(4_000);

/// The invisible top-level window that receives session-end messages.
///
/// Created on the main thread (which pumps messages) so the window
/// procedure runs there. Destroyed on drop.
pub struct SessionSink(HWND);

impl SessionSink {
    /// Register the class (once) and create the hidden window.
    ///
    /// The window is a zero-size `WS_POPUP` tool window that is never shown,
    /// never activated, and never appears in Alt-Tab — but it *is* a
    /// top-level window, so `WM_QUERYENDSESSION` / `WM_ENDSESSION` reach it.
    ///
    /// # Errors
    ///
    /// Returns a string if class registration or `CreateWindowExW` fails.
    pub fn create() -> Result<Self, String> {
        ensure_class_registered()?;
        // SAFETY: GetModuleHandleW(NULL) is the current module; always valid.
        let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }
            .map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
        // SAFETY: the class is registered; all pointers are valid static
        // strings; no parent, menu or lparam.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0),
                SINK_CLASS_NAME,
                w!("FlowWM session sink"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW(session sink) failed: {e}"))?;
        log::debug!("session sink window created ({:?})", hwnd);
        Ok(Self(hwnd))
    }

    /// A sink with no window, for unit tests that build a `FlowWM` without
    /// touching Win32.
    #[cfg(test)]
    pub(crate) fn test_dummy() -> Self {
        Self(HWND::default())
    }

    /// The sink's window handle (null for a test dummy).
    #[must_use]
    pub fn hwnd(&self) -> HWND {
        self.0
    }
}

impl Drop for SessionSink {
    fn drop(&mut self) {
        if self.0.is_invalid() {
            return;
        }
        // SAFETY: we created this window on this (main) thread and own it.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// Register `FlowSessionSink` once per process.
fn ensure_class_registered() -> Result<(), String> {
    if SINK_CLASS_ATOM.get().is_some() {
        return Ok(());
    }
    // SAFETY: see `SessionSink::create`.
    let hinstance = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_err(|e| format!("GetModuleHandleW failed: {e}"))?;
    let wcex = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(sink_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: SINK_CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: fully-populated class struct with static strings.
    let atom = unsafe { RegisterClassExW(&wcex) };
    if atom == 0 {
        // Another caller may have won the race; treat "already exists" as
        // success by re-checking the lock-free cell.
        if SINK_CLASS_ATOM.get().is_some() {
            return Ok(());
        }
        return Err("RegisterClassExW(FlowSessionSink) returned 0".to_owned());
    }
    let _ = SINK_CLASS_ATOM.set(atom);
    Ok(())
}

/// Window procedure of the session sink.
///
/// Every session-end path funnels into [`request_shutdown`]; the main loop
/// then runs the rescue. `WM_QUERYENDSESSION` answers `TRUE` so a logoff is
/// never vetoed, and `WM_ENDSESSION` with `wParam == TRUE` is the definitive
/// "session is ending" notice. `WM_DISPLAYCHANGE` (a monitor attached,
/// detached or re-configured) and `WM_SETTINGCHANGE` with `SPI_SETWORKAREA`
/// (taskbar moved, resized or set to auto-hide) report a display-topology
/// change through
/// [`request_topology_refresh`](crate::registry::hooks::request_topology_refresh);
/// the main loop debounces and reconciles (see `daemon::hotplug`).
///
/// # Safety
///
/// Called by Windows; must match the `WindowProc` signature.
unsafe extern "system" fn sink_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLOSE => {
            log::info!("session sink: WM_CLOSE");
            request_shutdown();
            LRESULT(0)
        }
        WM_QUERYENDSESSION => {
            log::info!("session sink: WM_QUERYENDSESSION (lparam={:#x})", lparam.0);
            request_shutdown();
            LRESULT(1)
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                log::info!("session sink: WM_ENDSESSION — session is ending");
                request_shutdown();
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            log::info!(
                "session sink: WM_DISPLAYCHANGE ({}x{}, {} bpp) — display topology changed",
                (lparam.0 & 0xffff) as u32,
                ((lparam.0 >> 16) & 0xffff) as u32,
                wparam.0
            );
            request_topology_refresh();
            LRESULT(0)
        }
        WM_SETTINGCHANGE if is_work_area_change(wparam) => {
            log::info!("session sink: WM_SETTINGCHANGE(SPI_SETWORKAREA) — work area changed");
            request_topology_refresh();
            LRESULT(0)
        }
        // SAFETY: pass-through with unchanged arguments.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Console control handler: Ctrl-C / Ctrl-Break / console close / logoff /
/// shutdown all request a graceful shutdown.
///
/// Runs on a thread Windows creates for the purpose, so it only touches
/// the atomic shutdown flags. For the events after which Windows terminates
/// the process immediately on return (`CLOSE`, `LOGOFF`, `SHUTDOWN`), it
/// blocks up to [`CONSOLE_HANDLER_GRACE`] for the main thread to finish the
/// rescue. Returning `TRUE` claims the event (for Ctrl-C this also stops the
/// default handler from exiting the process).
///
/// # Safety
///
/// Called by Windows; must match `PHANDLER_ROUTINE`.
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    let name = match ctrl_type {
        CTRL_C_EVENT => "CTRL_C",
        CTRL_BREAK_EVENT => "CTRL_BREAK",
        CTRL_CLOSE_EVENT => "CTRL_CLOSE",
        CTRL_LOGOFF_EVENT => "CTRL_LOGOFF",
        CTRL_SHUTDOWN_EVENT => "CTRL_SHUTDOWN",
        _ => return BOOL(0),
    };
    log::info!("console control event {name}: requesting shutdown");
    request_shutdown();
    if console_event_is_terminal(ctrl_type) {
        let done = wait_shutdown_complete(CONSOLE_HANDLER_GRACE);
        log::info!(
            "console control event {name}: teardown {}",
            if done {
                "complete"
            } else {
                "did not finish in time"
            }
        );
    }
    BOOL(1)
}

/// `true` when a `WM_SETTINGCHANGE` announces a work-area change
/// (`wParam == SPI_SETWORKAREA`): the taskbar or an appbar moved, resized
/// or toggled auto-hide, so every monitor's work area must be re-read.
#[must_use]
pub fn is_work_area_change(wparam: WPARAM) -> bool {
    wparam.0 == SPI_SETWORKAREA.0 as usize
}

/// `true` for the console events after which Windows terminates the
/// process as soon as the handler returns (close, logoff, shutdown).
#[must_use]
pub fn console_event_is_terminal(ctrl_type: u32) -> bool {
    matches!(
        ctrl_type,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    )
}

/// Install [`console_ctrl_handler`] for this process. Harmless when the
/// daemon has no console (detached by `flow start`): the handler simply
/// never fires.
///
/// # Errors
///
/// Returns the `SetConsoleCtrlHandler` error as a string.
pub fn install_console_ctrl_handler() -> Result<(), String> {
    // SAFETY: registering a valid `extern "system"` handler.
    unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), true) }
        .map_err(|e| format!("SetConsoleCtrlHandler failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_console_events_are_close_logoff_shutdown() {
        assert!(console_event_is_terminal(CTRL_CLOSE_EVENT));
        assert!(console_event_is_terminal(CTRL_LOGOFF_EVENT));
        assert!(console_event_is_terminal(CTRL_SHUTDOWN_EVENT));
        assert!(!console_event_is_terminal(CTRL_C_EVENT));
        assert!(!console_event_is_terminal(CTRL_BREAK_EVENT));
        assert!(!console_event_is_terminal(999));
    }

    #[test]
    fn work_area_change_is_only_spi_setworkarea() {
        assert!(is_work_area_change(WPARAM(SPI_SETWORKAREA.0 as usize)));
        assert!(!is_work_area_change(WPARAM(0)));
        assert!(!is_work_area_change(WPARAM(0x0011)));
    }

    #[test]
    fn test_dummy_sink_has_no_window_and_drops_quietly() {
        let sink = SessionSink::test_dummy();
        assert!(sink.hwnd().is_invalid());
        drop(sink);
    }
}
