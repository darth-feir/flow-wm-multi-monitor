# Invisible frame borders are re-measured when a window is placed

**Status:** Accepted (stage 35, 2026-11-06). Amends how
`Window::invisible_bounds` is kept (`docs/src/dev-guide/window-registry.md`,
*The `InvisibleBounds` Concept*); the ring geometry is unchanged (it hugs the
slot, `docs/src/dev-guide/borders.md`).

## Context

On the host, SQL Server Management Studio 21/22 showed a dark strip of 5 to
6 px between the border ring and its content on the left, right and bottom
(1 px on top). Measured read-only while the daemon ran (P/Invoke and a pixel
scan):

| | rect | inset to window rect (L/T/R/B) |
|---|---|---|
| SSMS main window, class `HwndWrapper[DefaultDomain;;<guid>]`, style `0x16CF0000` (caption + thick frame) | 994,18 908x1044 | - |
| `DWMWA_EXTENDED_FRAME_BOUNDS` | 999,18 898x1039 | 5 / 0 / 5 / 5 |
| client area | 1000,18 896x1038 | 6 / 0 / 6 / 6 |
| `flow query all`: `invisible_bounds` | 0,0,0,0 | - |
| `tiled_rect` (slot, ring included) | 992,16 912x1048 | - |
| `visible_rect` = `window_rect` | 994,18 908x1044 | - |

Pixels: ring (#4E1522) at x 992-993, a near-black strip (#0D0D12) at x
994-999, content from x 1000. The daemon had placed the raw window rect on
the slot's visible area. SSMS paints its invisible-border zone dark instead
of leaving it transparent, so the missing compensation shows as a gap.
Firefox on the same host (inset 8) had no gap.

Cause: the invisible borders were measured once, when the window was
registered (or pre-placed, ADR-0023), and kept for the life of the record.
WPF / VS-shell windows apply their chrome after the window exists, so at
that moment DWM still reports frame == window rect, and 0 was stored for
good. Only a maximize restore (`refresh_geometry`) and the app's own full
screen (ADR-0020) re-measured.

## Considered Options

- **(a) Re-measure whenever the daemon places the window.** Chosen.
- **(b) A rule field (`invisible_bounds = [5, 0, 5, 5]` per class).**
  Rejected: the measurement is available and correct once the chrome is
  there; a rule would be one more thing to keep in step with the app's DPI
  and version.
- **(c) Re-measure on a delay after registration (one retry at 500 ms).**
  Rejected: the moment the chrome lands is the app's, not ours; a window
  that changes its chrome later (a theme switch, a frame toggle) would
  still be wrong.
- **(d) Hook `WM_NCCALCSIZE` / frame changes.** Not available: out of
  process the daemon sees no message for `SWP_FRAMECHANGED`, and
  `EVENT_OBJECT_LOCATIONCHANGE` is filtered to floats and the dragged tile.

## Decision

- **Where.** `daemon/frame_bounds.rs`. Before a placement uses the stored
  value, the daemon re-measures every window that placement moves: every
  layout batch (`animate_layouts_on`, the preview submit, the workspace
  batches, both teleports), the dragged window at drag start, and a float
  whose rect is read back on `LOCATIONCHANGE` (`store_float_rect`). A changed
  value is stored in the registry and the same placement uses it.
- **Idle windows.** The periodic foreground sync (every
  `foreground_sync_interval_ms`, 250 ms by default, next to the stage 31b
  corner recheck) re-measures the focused window. On a change it stores the
  new borders and moves only that window: a tile to its last commanded slot
  (inset for the ring as in a layout), a float to its stored visible rect.
  One `SetWindowPos`, no layout animation.
- **Guards.** Only an active tile or float is measured, never one the app
  holds in its own full screen, never the window a drag owns, and only while
  the animator is idle (a window caught mid-tween reads as a moved frame).
  The window must be visible, not iconic, not maximized and not cloaked
  (DWM reports the icon rect for an iconic window and the monitor overhang
  for a maximized one). The measurement reads `GetWindowRect` before and
  after the DWM query and is dropped when the two differ; negative edges
  clamp to zero as before; an edge above 32 px is taken as a bad reading.
  A failed or dropped reading keeps the stored value (registration still
  stores zero on failure).
- **Visible.** `flow query all` reports the stored (refreshed) value. A
  change logs at debug:
  `window 0x.. (exe) invisible bounds changed L.. T.. R.. B.. -> L.. T.. R.. B..; re-placed`
  (`; rect re-read` from the float read-back, `; drag start` from a drag).

## Consequences

- SSMS / Visual Studio content fills its slot after the next layout event,
  or within one foreground-sync tick while it has the focus.
- Cost: per measured window `IsWindowVisible`, `IsIconic`, `IsZoomed`,
  `DwmGetWindowAttribute(CLOAKED)`, two `GetWindowRect` and one
  `DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS)`. Layout batches are per
  event, not per frame, and most run while the animator is still busy with
  the previous batch, where nothing is measured; a drag preview measures
  the strip's windows per preview step. Not measured as a hot spot; no
  cache was built.
- An unfocused late-chrome window stays off until the next layout event
  touches its strip; the recheck covers only the focused window.
- A window that drops its chrome (frame == rect again) goes back to zero
  the same way.

## Tests

Unit tests in `daemon/frame_bounds.rs` (fresh bounds used by the next
placement, a failed reading keeps the stored value, non-measurable and
minimized windows are skipped, the recheck re-places a tile into its slot and
a float at its visible rect, an unchanged window is not moved) and
`registry/win32.rs` (a failed re-measurement is `None`). VM: `test-stage35.ps1`
in the VM harness, with `guest\testwin35.ps1`: a WinForms window with
`WS_CAPTION | WS_THICKFRAME` that first answers `WM_NCCALCSIZE` with 0
(frame == window rect, inset 0/0/0/0) and on command stops doing so and
applies `SWP_FRAMECHANGED` (inset 7/0/7/7, window rect unchanged).
