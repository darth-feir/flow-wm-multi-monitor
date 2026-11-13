//! `flow start` notices for a start nobody watches.
//!
//! `flow start` prints its warnings and its failure on stderr. Started from
//! the Startup-folder shortcut or from `flow-resident.ahk`, it runs in a
//! console of its own that is hidden and gone when it exits, so the same
//! news is sent as a notification instead: a notification-area balloon
//! (`Shell_NotifyIconW` with `NIF_INFO`), which Windows 10/11 show as a
//! toast. No AppUserModelID, no registry. Windows drops such a notice with
//! its icon, so it is not kept in the notification center; the log keeps
//! the record. See `docs/adr/0032-a-bad-binds-table-is-visible-at-start.md`.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Console::{GetConsoleProcessList, GetConsoleWindow};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_TIP, NIIF_ERROR, NIIF_WARNING, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, IDI_ERROR, IDI_WARNING, LoadIconW, MSG,
    PM_REMOVE, PeekMessageW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
};
use windows::core::w;

/// Title of every notice.
pub const NOTICE_TITLE: &str = "FlowWM";

/// How long the notification-area icon is kept after the notice is sent:
/// the toast lasts as long as its icon, so this covers Windows' default
/// display time (5 s) with room to spare. `flow start` has nothing else to
/// do by then.
pub const NOTICE_LINGER: Duration = Duration::from_secs(10);

/// Longest notice text the shell takes (`szInfo` holds 255 UTF-16 units).
pub const NOTICE_MAX_UNITS: usize = 255;

/// What the notice reports: its icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// The daemon started, with something to fix.
    Warning,
    /// `flow start` failed.
    Error,
}

/// `true` when someone can read this process's stderr: it is attached to a
/// console that another process (the shell that ran it) shares. `false`
/// with no console at all, or with a console of its own (the hidden one
/// Windows creates for a console program started from a shortcut or by
/// `Run(..., "Hide")`), which nobody sees and which closes when it exits.
/// Whether stderr is a terminal cannot tell these apart: a console of its
/// own is a terminal too.
#[must_use]
pub fn stderr_is_watched() -> bool {
    // SAFETY: no arguments; returns a null handle when there is no console.
    let console = unsafe { GetConsoleWindow() };
    if console.is_invalid() {
        return false;
    }
    let mut pids = [0u32; 4];
    // SAFETY: `pids` is a valid buffer of its stated length; the return is
    // the number of processes attached (0 on failure).
    let attached = unsafe { GetConsoleProcessList(&mut pids) };
    attached > 1
}

/// `head` + `middle` + `tail` in at most [`NOTICE_MAX_UNITS`] UTF-16 units:
/// `middle` is shortened (ending in `...`) so the head and the tail (the
/// log path) survive.
#[must_use]
pub fn fit_notice(head: &str, middle: &str, tail: &str) -> String {
    let units = |s: &str| s.encode_utf16().count();
    let whole = format!("{head}{middle}{tail}");
    if units(&whole) <= NOTICE_MAX_UNITS {
        return whole;
    }
    let room = NOTICE_MAX_UNITS.saturating_sub(units(head) + units(tail) + 3);
    let mut cut = String::new();
    for c in middle.chars() {
        if units(&cut) + c.len_utf16() > room {
            break;
        }
        cut.push(c);
    }
    let out: Vec<u16> = format!("{head}{cut}...{tail}")
        .encode_utf16()
        .take(NOTICE_MAX_UNITS)
        .collect();
    String::from_utf16_lossy(&out)
}

/// Copy `text` into a fixed UTF-16 field, NUL-terminated, truncated to fit.
fn fill(field: &mut [u16], text: &str) {
    let max = field.len().saturating_sub(1);
    for (slot, unit) in field.iter_mut().zip(text.encode_utf16().take(max)) {
        *slot = unit;
    }
}

/// Send `text` as a notice titled [`NOTICE_TITLE`] from a notification-area
/// icon of this process, keep the icon for [`NOTICE_LINGER`], then remove
/// it. Blocks the calling thread for that time.
///
/// # Errors
///
/// A line naming the call that failed (the window, the icon or the notice).
pub fn notify(kind: NoticeKind, text: &str) -> Result<(), String> {
    // A hidden window of a system class owns the icon; nothing is shown.
    // SAFETY: a plain top-level window with static class and title strings.
    let hwnd: HWND = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("flow start notice"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    }
    .map_err(|e| format!("notice window: {e}"))?;
    let (icon_id, info_flags) = match kind {
        NoticeKind::Warning => (IDI_WARNING, NIIF_WARNING),
        NoticeKind::Error => (IDI_ERROR, NIIF_ERROR),
    };
    // SAFETY: a system icon, no module handle.
    let icon = unsafe { LoadIconW(None, icon_id) }.unwrap_or_default();
    let mut data = NOTIFYICONDATAW {
        cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_TIP | NIF_INFO,
        hIcon: icon,
        dwInfoFlags: info_flags,
        ..Default::default()
    };
    fill(&mut data.szTip, NOTICE_TITLE);
    fill(&mut data.szInfoTitle, NOTICE_TITLE);
    fill(&mut data.szInfo, text);
    // SAFETY: `data` is a fully initialised NOTIFYICONDATAW of its own size.
    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &raw const data) }.as_bool();
    let result = if added {
        // Keep the window answering while the shell shows the notice.
        let until = Instant::now() + NOTICE_LINGER;
        let mut msg = MSG::default();
        while Instant::now() < until {
            // SAFETY: `msg` is a valid MSG; this thread's queue only.
            while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                // SAFETY: a message just taken from this thread's queue.
                unsafe {
                    let _ = TranslateMessage(&raw const msg);
                    DispatchMessageW(&raw const msg);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let remove = NOTIFYICONDATAW {
            cbSize: data.cbSize,
            hWnd: hwnd,
            uID: 1,
            ..Default::default()
        };
        // SAFETY: removes the icon added above.
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &raw const remove) };
        Ok(())
    } else {
        Err("notice: Shell_NotifyIconW(NIM_ADD) failed".to_owned())
    };
    // SAFETY: the window created above, on this thread.
    let _ = unsafe { DestroyWindow(hwnd) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_notice_is_kept_whole() {
        assert_eq!(fit_notice("a: ", "b", ". c"), "a: b. c");
    }

    #[test]
    fn a_long_notice_shortens_the_middle_and_keeps_the_log_path() {
        let tail = ". Check the log at C:\\Users\\x\\.config\\flow\\logs\\flowd-2026-11-12.log";
        let out = fit_notice("flow start failed: ", &"x".repeat(400), tail);
        assert_eq!(out.encode_utf16().count(), NOTICE_MAX_UNITS);
        assert!(out.starts_with("flow start failed: xxx"), "{out}");
        assert!(out.ends_with(&format!("...{tail}")), "{out}");
    }

    #[test]
    fn fill_truncates_and_leaves_the_nul() {
        let mut zeroed = [0u16; 4];
        fill(&mut zeroed, "abcdef");
        assert_eq!(zeroed, [97, 98, 99, 0]);
    }
}
