//! Win32 backend: individual `SetWindowPos` calls + `DwmFlush` implementation.
//!
//! Each window in an animation batch is repositioned via its own
//! [`SetWindowPos`] call rather than the batch `DeferWindowPos` API.
//! This avoids the UIPI problem where a single higher-integrity window
//! (e.g. an admin-elevation UAC prompt) causes the *entire* deferred batch
//! to fail with `ERROR_ACCESS_DENIED`.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowRect, SET_WINDOW_POS_FLAGS, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOSENDCHANGING, SWP_NOSIZE,
    SWP_NOZORDER, SetWindowPos,
};

use crate::animation::backend::WindowBackend;
use crate::animation::types::{AnimationError, Rect, Result, WindowRef};

/// Whether `rect` lies entirely outside `screen` (a parking slot). Same
/// test as `MonitorInfo::is_parked`; an empty overlap counts as outside.
fn outside(rect: Rect, screen: Rect) -> bool {
    rect.x >= screen.x + screen.w
        || rect.x + rect.w <= screen.x
        || rect.y >= screen.y + screen.h
        || rect.y + rect.h <= screen.y
}

/// The `SetWindowPos` flags for placing a managed window at `next`, given
/// where it is now (`current`, `None` when unreadable) and the virtual
/// screen. See ADR-0019.
///
/// - `SWP_NOZORDER | SWP_NOACTIVATE` always.
/// - `SWP_NOSIZE` when `next` keeps the current size: a pure move carries
///   no size. The size is still sent when the window leaves a parking slot
///   for the screen, so the app validates a size set while it was parked.
/// - `SWP_NOSENDCHANGING` when the size changes and `next` is parked, so the
///   app cannot rewrite the parked position.
/// - An on-screen resize keeps the plain flags (min/max tracking applies).
#[must_use]
pub(crate) fn swp_flags(
    current: Option<Rect>,
    next: Rect,
    virtual_screen: Rect,
) -> SET_WINDOW_POS_FLAGS {
    let base = SWP_NOZORDER | SWP_NOACTIVATE;
    let next_parked = outside(next, virtual_screen);
    let same_size = current.is_some_and(|c| {
        c.w == next.w && c.h == next.h && (next_parked || !outside(c, virtual_screen))
    });
    if same_size {
        base | SWP_NOSIZE
    } else if next_parked {
        base | SWP_NOSENDCHANGING
    } else {
        base
    }
}

/// The virtual screen (bounding box of all monitors) in screen coordinates.
fn virtual_screen() -> Rect {
    // SAFETY: GetSystemMetrics is a pure query over system constants.
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// Read a window's rect (window-rect space, as `SetWindowPos` takes it).
fn read_rect(hwnd: HWND) -> Option<Rect> {
    let mut rect = RECT::default();
    // SAFETY: GetWindowRect with a possibly-stale HWND fails cleanly.
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(Rect {
        x: rect.left,
        y: rect.top,
        w: rect.right - rect.left,
        h: rect.bottom - rect.top,
    })
}

/// Place a managed window at `rect` (window-rect space) with the flags
/// [`swp_flags`] picks. Every daemon path that positions a managed window
/// (the animator's frames, the teleports, fail-back, rescue) goes through
/// here, so the flag rules hold everywhere.
///
/// # Errors
///
/// The `SetWindowPos` error (stale handle, UIPI denial).
pub(crate) fn place_window(hwnd_val: isize, rect: Rect) -> windows::core::Result<()> {
    let hwnd = HWND(hwnd_val as _);
    let flags = swp_flags(read_rect(hwnd), rect, virtual_screen());
    // SAFETY: SetWindowPos with a possibly-stale HWND fails cleanly;
    // hwnd_insert_after None + SWP_NOZORDER keeps the z-order.
    unsafe { SetWindowPos(hwnd, None, rect.x, rect.y, rect.w, rect.h, flags) }
}

/// Win32 implementation of [`WindowBackend`] using individual
/// [`SetWindowPos`] calls for per-window moves and [`DwmFlush`] for
/// frame-paced animation.
///
/// # Why not `DeferWindowPos`?
///
/// The Win32 `BeginDeferWindowPos` → `DeferWindowPos` × N → `EndDeferWindowPos`
/// batch API is atomic: if **any** window in the batch is owned by a
/// higher-integrity process, the UIPI (User Interface Privilege Isolation)
/// mechanism rejects the *entire* batch with `ERROR_ACCESS_DENIED`. This
/// means a single elevated admin window prevents every other window in the
/// batch from being moved.
///
/// By calling [`SetWindowPos`] individually for each window we ensure that a
/// single UIPI denial only logs a warning and does not block the remaining
/// windows in the same animation frame.
#[allow(dead_code)] // Not yet consumed by other crate modules; will be used by compositor.
pub struct Win32Backend;

#[allow(dead_code)] // Not yet consumed by other crate modules; will be used by compositor.
impl Win32Backend {
    /// Create a new [`Win32Backend`].
    pub fn new() -> Self {
        Self
    }
}

impl Default for Win32Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowBackend for Win32Backend {
    /// Query the current screen rectangle of the given window.
    ///
    /// Calls [`GetWindowRect`] and converts the Win32 `{left, top, right, bottom}`
    /// representation to the crate's `{x, y, w, h}` form.
    fn get_window_rect(&self, window: WindowRef) -> Result<Rect> {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(HWND(window.0 as _), &mut rect) }
            .map_err(|e| AnimationError::Backend(e.message().to_string()))?;
        Ok(Rect {
            x: rect.left,
            y: rect.top,
            w: rect.right - rect.left,
            h: rect.bottom - rect.top,
        })
    }

    /// Reposition every window in `updates` using individual [`SetWindowPos`]
    /// calls.
    ///
    /// # Resilience vs. the batch API
    ///
    /// The previous implementation used `BeginDeferWindowPos` → `DeferWindowPos`
    /// × N → `EndDeferWindowPos`. That API is atomic: if *any* window is
    /// protected by UIPI (e.g. an elevated admin process), the whole batch
    /// fails with `ERROR_ACCESS_DENIED` and no windows move at all.
    ///
    /// With per-window [`SetWindowPos`]:
    /// - Each failure is logged at `warn` level but does **not** prevent the
    ///   remaining windows from being moved.
    /// - The first error encountered is returned so callers can still react
    ///   to partial failures (e.g. retry on the next frame).
    /// - Every window in `updates` is attempted, regardless of earlier errors.
    ///
    /// # Z-order preservation
    ///
    /// The `hwnd_insert_after` parameter is `None` combined with the
    /// [`SWP_NOZORDER`] flag, which means the existing Z-order of each
    /// window is left unchanged. The size flags come from [`swp_flags`]
    /// (ADR-0019).
    fn apply_batch(&self, updates: &[(WindowRef, Rect)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let mut first_error = None;
        for (window, rect) in updates {
            let result = place_window(window.0, *rect);

            if let Err(e) = result {
                log::warn!("SetWindowPos failed for window {:?}: {e}", window);
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }

        // Return the first error encountered, if any.
        // Individual failures are logged but don't prevent other windows from being moved.
        match first_error {
            Some(e) => Err(AnimationError::Backend(e.message().to_string())),
            None => Ok(()),
        }
    }

    /// Block until the Desktop Window Manager completes its current composition
    /// cycle, synchronising the animation loop with the display refresh rate.
    fn dwm_flush(&self) -> Result<()> {
        unsafe { DwmFlush() }.map_err(|e| AnimationError::Backend(e.message().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DISPLAY2 at -1920..0, DISPLAY1 at 0..1920, both 1080 tall.
    const VS: Rect = Rect {
        x: -1920,
        y: 0,
        w: 3840,
        h: 1080,
    };
    const BASE: SET_WINDOW_POS_FLAGS = SET_WINDOW_POS_FLAGS(SWP_NOZORDER.0 | SWP_NOACTIVATE.0);

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    /// A pure move to the right park slot carries no size (the SSMS case).
    #[test]
    fn park_right_move_is_nosize() {
        let f = swp_flags(Some(r(1850, 8, 908, 1044)), r(1936, 8, 908, 1044), VS);
        assert_eq!(f, BASE | SWP_NOSIZE);
    }

    /// Every on-screen tween frame at the same size is a pure move.
    #[test]
    fn on_screen_move_is_nosize() {
        let f = swp_flags(Some(r(994, 8, 908, 1044)), r(1359, 8, 908, 1044), VS);
        assert_eq!(f, BASE | SWP_NOSIZE);
    }

    /// A resize landing in a park slot skips WM_WINDOWPOSCHANGING.
    #[test]
    fn parked_resize_is_nosendchanging() {
        let f = swp_flags(Some(r(1936, 8, 908, 1044)), r(1936, 8, 608, 1044), VS);
        assert_eq!(f, BASE | SWP_NOSENDCHANGING);
        let left = swp_flags(Some(r(-3000, 8, 908, 1044)), r(-2600, 8, 608, 1044), VS);
        assert_eq!(left, BASE | SWP_NOSENDCHANGING);
    }

    /// An on-screen resize keeps the plain flags so min/max tracking runs.
    #[test]
    fn on_screen_resize_is_plain() {
        let f = swp_flags(Some(r(100, 8, 908, 1044)), r(100, 8, 300, 1044), VS);
        assert_eq!(f, BASE);
    }

    /// Leaving a park slot for the screen re-sends the size even when it is
    /// unchanged, so a size set while parked is validated by the app.
    #[test]
    fn leaving_park_sends_size() {
        let f = swp_flags(Some(r(1936, 8, 608, 1044)), r(1700, 8, 608, 1044), VS);
        assert_eq!(f, BASE);
    }

    /// An unreadable current rect sends the size (plain on screen,
    /// NOSENDCHANGING when parked).
    #[test]
    fn unknown_current_sends_size() {
        assert_eq!(swp_flags(None, r(100, 8, 300, 300), VS), BASE);
        assert_eq!(
            swp_flags(None, r(1936, 8, 300, 300), VS),
            BASE | SWP_NOSENDCHANGING
        );
    }

    /// Vertical parking (inactive workspace) counts as parked; a rect
    /// touching the edge from outside is parked, one pixel in is not.
    #[test]
    fn park_zone_edges() {
        assert!(outside(r(0, 1080, 100, 100), VS));
        assert!(outside(r(1920, 0, 100, 100), VS));
        assert!(outside(r(-2020, 0, 100, 100), VS));
        assert!(!outside(r(1919, 0, 100, 100), VS));
        assert!(!outside(r(-2019, 0, 100, 100), VS));
    }
}
