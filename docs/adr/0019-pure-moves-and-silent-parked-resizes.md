# A move carries no size, and a resize into a parking slot is not offered to the app

**Status:** Accepted — changes the `SetWindowPos` flags of every path that
places a managed window (the animator's frames, the teleports, fail-back and
`flow rescue`); before, each one sent `SWP_NOZORDER | SWP_NOACTIVATE` with
the size every time.

Every `SetWindowPos` the daemon issues for a managed window goes through
one helper, `animation::backend::win32::place_window`, which reads the
window's current rect and asks the pure `swp_flags(current, next,
virtual_screen)` for the flags:

- `SWP_NOZORDER | SWP_NOACTIVATE`, always, as before;
- plus `SWP_NOSIZE` when `next` keeps the current size (a pure move) —
  except when the window is leaving a parking slot for the screen;
- else plus `SWP_NOSENDCHANGING` when the size changes and `next` lies
  entirely outside the virtual screen (a parking slot, ADR-0007);
- else (an on-screen resize, or the first placement back on screen) the
  plain flags, so the app's `WM_WINDOWPOSCHANGING` / `WM_GETMINMAXINFO`
  handling still applies.

## Context

SQL Server Management Studio 21 (a Visual Studio-shell WPF window) never
parked right. Measured on the host against the real window: a pure move to
`x = 1936` (`SWP_NOSIZE`) stays there, but the same move with its own size
passed explicitly — what the animator sent on every frame — makes the VS
shell's `WM_WINDOWPOSCHANGING` handler rewrite `x` to `-cx`, so the window's
right edge sits at 0: on the monitor left of the primary. Moves to the left
parking slot stick, and so does a sized move sent with
`SWP_NOSENDCHANGING`, because the app never sees the message. Under the
daemon the park tween ran to the last frame and the window jumped onto the
other screen while `flow query` still reported the parking slot. A bare WPF
window, Firefox, Excel and Alacritty do not do this; it is the shell's own
handling.

## Considered Options

- **A per-app rule (`[[rules]]` flag that parks the window by hiding it).**
  Rejected: it needs the user to find out which apps misbehave, and hiding
  is a separate roadmap item (ADR-0007).
- **`SWP_NOSENDCHANGING` on every call.** Rejected: on screen an app's min /
  max tracking size must still apply, or a window with a minimum width
  would be squeezed below it and draw clipped.
- **Re-assert the position after the frame (read back, move again).**
  Rejected: a second `SetWindowPos` per window per frame, and the window
  would still flash on the other monitor.
- **Track the last size the daemon sent, per window, in the backend.**
  Rejected for a read of the real rect: the backend stays stateless (and the
  trait mockable), the teleport path shares it with no shared map, and a
  size the app refused (min width) is seen as a size change and resent,
  which is today's behaviour on screen.
- **Flags from the current rect and the target (chosen).** A pure move
  cannot be rewritten into a different position by an app that only
  reacts to sized moves; a resize while parked cannot be answered at all;
  an on-screen resize is unchanged.

## Consequences

- **Parked windows stay parked.** The VS-shell quirk needs a size with a
  target past the right edge; the park move is a pure move and a parked
  resize skips the message, so neither path can trigger it.
- **A size set while parked is validated on the way back.** `SWP_NOSENDCHANGING`
  also skips `WM_GETMINMAXINFO`, so a width preset narrower than an app's
  minimum is applied as is while it is parked. The first placement back on
  screen therefore always carries the size with the plain flags, and the
  app enforces its minimum there, as it would have before.
- **Pure moves send no size.** Tween frames that only move no longer
  carry the size, which also spares apps a size check on every frame.
- **One `GetWindowRect` per placed window.** Cheap next to the
  `SetWindowPos` itself; when it fails the size is sent (plain on screen,
  `SWP_NOSENDCHANGING` when parked).
- **Not changed.** The border overlays go through the same helper with the
  animator's batches; their own `set_geometry` / z-order calls in
  `borders/border.rs` already use `SWP_NOSENDCHANGING` and are untouched.
  The top-layer z-order calls (`SWP_NOMOVE | SWP_NOSIZE`) and
  `SetWindowPlacement` paths do not position windows and are untouched.
- **Tests.** `animation::backend::win32` tests the flag decision: a pure
  move to the right slot is `NOSIZE`, on-screen frames are `NOSIZE`, a
  parked resize (right and left) is `NOSENDCHANGING`, an on-screen resize
  is plain, leaving a slot resends the size, an unreadable rect sends the
  size, and the park-zone edges. `registry::win32` pins the mask
  `set_window_rect` uses. The VM driver (`test-stage25.ps1`) runs a WinForms
  stand-in that reproduces the quirk exactly, proves it against the old
  build, and covers park left/right, a width change while parked, a
  workspace switch, tiling off/on, a cross-screen drag, a min-width window
  and `flow stop` while parked.
