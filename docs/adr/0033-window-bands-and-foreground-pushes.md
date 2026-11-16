# Window bands mark shell surfaces; a foreground push injects no input

> Corrected in part by [ADR-0034](0034-shell-bands-replace-shell-lists-and-the-session-lock.md): the real lock-screen frames are band 17 (the band 1 below was the stand-ins), a window can change band before it is first shown, and the class and exe entries the band covers are removed.

**Status:** Accepted (stage 39, 2026-11-12). Adds a hard gate beside the
elevated-window gate of [ADR-0025](0025-elevated-windows-are-unmanaged.md)
and a backstop behind the shell classes and lock-screen rules of
[ADR-0030](0030-lock-screen-surfaces-are-ignored.md). Changes how
`registry::win32::set_foreground_window_impl` handles a refused push. Rule
precedence (user, learned, built-in, `default_action`) is unchanged.

## Context

Two things led here.

- The shell surfaces flow must leave alone (Start and search, the
  notification center and its toasts, Alt+Tab, the lock screen) were known
  only by name: `top_layer::SHELL_CLASSES`, the bundled rules for
  `Windows.UI.Core.CoreWindow` and the lock-screen exe and class rules of
  ADR-0030. Each new surface Windows ships (a renamed host, a new flyout)
  needed another class list entry, found only after it misbehaved on the
  host.
- Since stage 10 (commit ac80c0e) the keymap has banned `TrayTip`, because
  in the VM a Windows 11 toast "held the foreground lock": `WinActivate` and
  the daemon's `SetForegroundWindow` failed with errors 5 and 6 while it was
  up, and focus-dependent tests acted on the wrong window. That build still
  passed a process id where `AttachThreadInput` wants a thread id (fixed
  later, see the comment in `set_foreground_window_impl`).

Since Windows 8 every top-level window sits in a z-order band. Ordinary
windows are in `ZBID_DESKTOP` (1); bands never mix, and a normal process
cannot create a window outside the desktop band. The band is readable only
through `GetWindowBand(HWND, *DWORD) -> BOOL`, an export of user32 that
Microsoft does not document.

## The probe (VM guest, Windows 11 22621.6060, an ordinary unsigned process)

`GetWindowBand` is exported and answers for every window, including windows
of an elevated process.

| window | exe / class | band |
|---|---|---|
| a live toast ("New notification") | ShellExperienceHost / `Windows.UI.Core.CoreWindow` | 4 |
| notification center, quick settings | ShellExperienceHost / `Windows.UI.Core.CoreWindow` | 4 |
| Start (search box) | SearchHost / `Windows.UI.Core.CoreWindow` | 6 |
| Start menu | StartMenuExperienceHost / `Windows.UI.Core.CoreWindow` | 6 |
| Alt+Tab ("Task Switching", also while hidden) | explorer / `XamlExplorerHostIslandWindow` | 16 |
| explorer helper (`ThumbnailDeviceHelperWnd`, cloaked) | explorer | 16 |
| taskbars | explorer / `Shell_TrayWnd`, `Shell_SecondaryTrayWnd` | 1 |
| desktop | explorer / `Progman`, `WorkerW` | 1 |
| an ordinary app window, flowd's border overlays | powershell, flowd | 1 |
| Calculator, Settings (UWP) | ApplicationFrameHost / `ApplicationFrameWindow` | 1 |
| Task Manager (elevated, not on top) | Taskmgr / `TaskManagerWindow` | 1 |
| On-Screen Keyboard (UIAccess) | osk / `OSKMainClass` | 2 |
| stage 36 lock-screen stand-ins | a normal process | 1 (any window a normal process makes) |

Not measured: Task Manager with "always on top" (it is elevated; a
medium-integrity test process cannot flip the setting), and the real lock
surfaces: locking this guest goes straight to the credential screen on the
Winlogon desktop, with no LockApp window on the user's desktop to read. The
published reverse engineering puts LockApp in `ZBID_LOCK` (17) and above.
`EnumWindows` from a desktop process does not list the band 4 and 6 windows
at all (it does list Alt+Tab); the daemon meets them through its WinEvent
hooks and `GetForegroundWindow`, and reads their band there.

What the data says: every shell surface whose band differs from 1 is
exactly a window flow must never manage, and no application window seen
sits above band 2. The taskbar, the desktop and the explorer lock frames
are band 1, so the class lists stay; the band is a backstop, not a
replacement.

## Considered Options

- **Which bands are managed.**
  - **(a) `ZBID_DEFAULT` (0), desktop (1) and UIAccess (2); everything above
    is a shell surface.** Chosen. UIAccess windows are real tools a user may
    arrange (On-Screen Keyboard, Magnifier, an AutoHotkey script started with
    UI access). Band 3 (`IMMERSIVE_IHM`, the text input host) and up are
    shell. Task Manager "always on top" moves to `SYSTEM_TOOLS` (16), the
    Alt+Tab band; it is elevated on an admin account and already unmanaged
    there (ADR-0025), so nothing is lost.
  - (b) Desktop only. Rejected: it would drop UIAccess tools.
  - (c) An explicit list of shell bands. Rejected: Windows adds bands; "above
    UIAccess" stays right for them.
- **Where the check sits.**
  - **(a) A hard gate at arrival beside the elevated gate, plus the shell
    tests of the floating layer and `set-window`.** Chosen: the band is an OS
    fact like elevation, read once (`get_window_info`); the layer and the
    dispatch paths ask `is_shell_surface(class, band)`.
  - (b) A built-in rule. Rejected: rules match exe, class and title; a band
    is not a rule key, and a user rule would outrank it.
- **A user override.** None. A window in a shell band is drawn above every
  desktop window whatever flow does, Windows refuses to hand it the
  foreground from a desktop process, and `SetWindowPos` cannot move it into
  a strip slot that means anything; managing it could only misplace it.
  The gate fails open instead: a build without the export, or a call that
  fails, gives `band: null` and the class and exe rules decide exactly as
  before.
- **The click fix (SendInput before `SetForegroundWindow`).** GlazeWM's
  `focus()` and komorebi send one `INPUT_MOUSE` with every field zero but a
  tag in `dwExtraInfo` (no move, no button) before `SetForegroundWindow`,
  so the window manager is "the process that received the last input
  event", one of the documented conditions for the call.
  - (a) Before every push. Rejected: injected input resets the session's
    idle timer (`GetLastInputInfo`), so every automatic push (a new window,
    a successor after a close) would delay the screen saver and the idle
    lock; and the VM showed it buys nothing in the cases measured (below).
  - (b) Only as a retry: when the push was refused with a desktop-band
    window in front and the target is still not in front, send the empty
    event (tag `FLOW`, `0x464C4F57`) and call `SetForegroundWindow` once
    more. Tried and dropped. Stage 39 shipped it to the VM; it never fired
    in any run (no `retrying after an own input event` line in the T, Q and
    L cases or the stage 38 and 21 suites), because every refusal the VM
    produced had a shell surface in front, which the shell-band gate
    already reports, and with a desktop window in front `AttachThreadInput`
    already succeeds. Unproven code that injects input and touches the idle
    timer was not worth keeping (stage 39b).
  - **(c) None: no injected input; `AttachThreadInput` is the push path.**
    Chosen. It is the path that works in every case the VM produced.

## What the VM showed about the toast and the foreground

- With a toast up and an ordinary window in front, the stage 38 build moved
  the foreground correctly in every case of the matrix (focus-column left
  and right, focus-monitor, `set-window`, a new window, Mod+Shift+V, the
  successor after a close): the toast is never the foreground window
  (`WS_EX_NOACTIVATE`-like: `GetForegroundWindow` stays on the app). The
  failure of stage 10 does not reproduce; it was most likely the
  `AttachThreadInput` thread-id bug, then fixed.
- With a band 4 or 6 surface in front (notification center, Start),
  `AttachThreadInput` to it fails with error 5 and `SetForegroundWindow`
  returns FALSE. A test process tried every variant from the background:
  nothing, the empty mouse event, `MOUSEEVENTF_MOVE` 0,0, an Alt tap, and
  `AttachThreadInput`; all were refused, and all succeeded with an
  ordinary window in front. Windows keeps a shell flyout in front until the
  user dismisses it; flow cannot and should not override that. Such a
  refusal is now logged at debug with `(foreground held by shell band N)`
  instead of a warning.

## Decision

- `registry::win32::window_band` reads the band through `GetWindowBand`,
  resolved once with `GetModuleHandleW` + `GetProcAddress`; `WindowInfo` and
  the registry's `Window` carry `band: Option<u32>`; `flow query all` reports
  `band` (a number, or `null` when unknown).
- `registry::classification::is_shell_band`: a known band above 2.
  `register_window_from_info` gives such a window
  `Ignored(ShellBand(n))` after the elevated gate and before every rule; the
  log line is `window 0x.. (exe "class") is in shell band n; left
  unmanaged`; `flow query all` shows the state `Ignored::ShellBand`.
- `top_layer::is_shell_surface(class, band)` (shell class or shell band)
  replaces the class-only test where a window or handle is at hand: the
  floating layer's membership and foreground classification, the dispatch
  foreground (`os_foreground_for_dispatch`) and the `set-window` target.
  Learned-rule filtering keeps the class test (a rule has no band).
  `set-window` on an `Ignored(ShellBand)` window is refused: `window 0x..
  (exe) is a shell surface (z-order band n); flow leaves it to Windows`.
- `set_foreground_window_impl`: with a shell band in front, failures are
  reported at debug with `(foreground held by shell band n)`. flowd injects
  no input: `AttachThreadInput` to the foreground thread is the push path,
  and a refusal with a desktop window in front stays a warning.
- The keymap's `TrayTip` ban is lifted as a hard rule: a toast does not take
  the foreground and does not block the daemon's pushes or `WinActivate`
  with an ordinary window in front. `Osd()` stays, because a toast is the
  wrong tool for a 2-second status line (it lingers and can queue), not
  because it breaks focus.

## Consequences

- An unknown future shell surface in a shell band is left alone without a
  new class entry. One in band 1 still needs a class or exe rule.
- `GetWindowBand` is undocumented. If a Windows build drops it or changes
  its meaning, the gate turns itself off (`band: null`) and the rules work
  as before; the export name is the only thing flow depends on.
- The band is read once at arrival, like elevation; a window never changes
  band during its life.
- flowd never calls `SendInput`, so it never resets the session's idle
  timer. If a refusal with a desktop window in front ever shows up on a
  host (an elevated or AppContainer foreground refusing the attach), the
  retry of option (b) is the first thing to measure again.
