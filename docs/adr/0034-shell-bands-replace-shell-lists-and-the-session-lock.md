# Shell bands replace the shell lists; the session lock is a signal

**Status:** Accepted (stage 40, 2026-11-13). Removes what the band gate of
[ADR-0033](0033-window-bands-and-foreground-pushes.md) already covers from
the bundled rules and `top_layer::SHELL_CLASSES`, and supersedes in part
[ADR-0030](0030-lock-screen-surfaces-are-ignored.md) (its lock-screen rules,
shell classes and `drop_hidden_lock_surface`). Rule precedence (user,
learned, built-in, `default_action`) is unchanged.

## Context

ADR-0033 added the z-order band as a backstop behind the class and exe
lists and kept every list entry, because the lock-screen frames it could
measure were band 1. Those were the stage 36 stand-ins, made by a normal
process; the real frames were never measured, because the guest's
`NoLockScreen` policy sends Win+L straight to the credential screen. The
user's rule is to prefer a signal Windows exposes over exe and class lists.
Two such signals were left unused:

- the band of the real shell surfaces not yet measured (Task View, the jump
  lists, the emoji panel, the lock screen itself);
- the session lock: `WTSRegisterSessionNotification` delivers
  `WM_WTSSESSION_CHANGE` with `WTS_SESSION_LOCK` / `WTS_SESSION_UNLOCK`
  (documented, wtsapi32, no registry).

## The probe (VM guest, Windows 11 22621.6060)

`guest\probe40-lock.ps1` in the VM harness: a hidden form registered for
session notifications and an out-of-context WinEvent hook on the same
thread, logging every top-level CREATE / SHOW / HIDE / CLOAK / UNCLOAK /
FOREGROUND with the window's band, in milliseconds. The guest's
`NoLockScreen` policy was switched off for the run, so Win+L showed the real
lock screen.

| window | exe / class | band |
|---|---|---|
| lock screen frames | explorer / `LockScreenBackstopFrame`, `LockScreenInputOcclusionFrame`, `LockScreenControllerProxyWindow` | 17 |
| lock screen | LockApp / `Windows.UI.Core.CoreWindow` | 17 |
| fingerprint / face feedback | explorer / `BioFeedbackUX XAML Host` | 14 |
| Task View (Win+Tab) | explorer / `XamlExplorerHostIslandWindow` | 5 |
| Alt+Tab | explorer / `XamlExplorerHostIslandWindow` | 16 |
| `ForegroundStaging`, `ThumbnailDeviceHelperWnd` | explorer | 16 |
| quick settings, notification center, cast | ShellExperienceHost / `Windows.UI.Core.CoreWindow` | 4 |
| jump list (Win+Alt+1) | ShellExperienceHost / `Windows.UI.Core.CoreWindow` | 6 (1 while still cloaked) |
| search (Win+S) | SearchHost / `Windows.UI.Core.CoreWindow` | 13 |
| emoji panel (Win+.) | explorer / `ApplicationFrameWindow` | 3 (1 while still cloaked) |
| light-dismiss overlay | explorer / `Shell_LightDismissOverlay` | 3 |
| edge input windows | explorer / `EdgeUiInputWndClass` | 7 |
| Win+X menu, tray overflow, snap layouts flyout | explorer / `Xaml_WindowedPopupClass`, `TopLevelWindowForOverflowXamlIsland` | 1 |
| clipboard history / emoji host windows | TextInputHost / `Windows.UI.Core.CoreWindow` | 1 (cloaked) |
| Widgets board | Widgets / `WindowsDashboard` | 1 |
| Magnifier | Magnify / `MagUIClass` | 2 |
| taskbar | explorer / `Shell_TrayWnd` | 1 (6 for one foreground event while a flyout closed) |

On the host (Windows 11 build 26300) the installed stage 39 daemon logged
explorer's `Xaml_WindowedPopupClass` popups in band 7.

Two corrections to ADR-0033:

- The real lock-screen frames are band 17, not 1; only the stand-ins were 1.
- A window can change band. The jump list's CoreWindow and the emoji
  panel's frame are created cloaked in band 1 and moved to their shell band
  before they are uncloaked. flow registers a window only once it is
  visible and uncloaked (`creation_gates`), and every such window was in its
  final band by its UNCLOAK event, so reading the band once at registration
  holds for them.

**The lock order** (Win+L, then Ctrl+Alt+Del and the password), first run:

| ms | event |
|---|---|
| 0 | `LockScreenBackstopFrame` CREATE / SHOW, band 17 |
| +83 | `LockScreenInputOcclusionFrame` CREATE, band 17 |
| +222 | LockApp's CoreWindow UNCLOAKED, band 17 |
| +254 | `WTS_SESSION_LOCK` |
| curtain raised | LockApp's CoreWindow CLOAKED (not hidden, not destroyed) |
| 0 (unlock) | both frames HIDE |
| +126 | both frames and `LockScreenControllerProxyWindow` DESTROY |
| +142 | `WTS_SESSION_UNLOCK` |

Two more locks in the stage 40 suite put `WTS_SESSION_LOCK` 38 ms *before*
the first frame and 391 ms after it; the unlock came 138 ms after the frames
hid. The order of the lock notification and the lock surfaces is not fixed,
so the notification cannot be what keeps them out (ADR-0030's option (c)
was rejected on that ground, rightly). Their band (17) does, whatever the
order. The signal is the right tool for what the band cannot see: an
ordinary window that opens while nobody can see the desktop.

## Considered Options

- **Which list entries go.** The criterion is the measured band of every
  instance of the entry that passes the creation gates.
  - **(a) Remove an entry when every visible instance is in a band above 2;
    keep it when any is band 1 or 2.** Chosen; the table below.
  - (b) Keep every entry as a second line of defence. Rejected: two
    mechanisms for one fact, and the lists drift (ADR-0033's own *Context*).
- **The fail-open case.** Without the `GetWindowBand` export, or when the
  call fails, the band is `null` and the removed entries are no longer
  caught: Start, search, the notification center and the lock screen would
  be classified by `default_action`.
  - **(a) Accept it, no fallback list.** Chosen. The export exists since
    Windows 8, answered for every window on both builds measured (22621 in
    the guest, 26300 on the host), and flow targets Windows 11. A missing
    export is visible: `flow query all` shows `band: null` for every window;
    the lists come back from this ADR's table then.
  - (b) Keep the removed entries behind `band == None`. Rejected: no suite
    showed a need, and a list nobody runs drifts unseen.
- **What the lock signal does.**
  - **(a) Hold arrivals while locked, release them at the unlock.** Chosen.
    A window that arrives while the session is locked is registered
    `Ignored(SessionLocked)`: nothing is placed, focused or raised for a
    desktop nobody sees. At the unlock the held windows leave the registry
    and go through the creation pipeline as new windows (seated, focused,
    or dropped when hidden by then), and every `Ignored(ShellBand)` window
    that is hidden, cloaked or iconic is dropped. The last part covers
    LockApp's window, which is cloaked at the curtain and never destroyed,
    without naming it; it replaces `drop_hidden_lock_surface`.
  - (b) Pause tiling while locked. Rejected: tiling off also re-tiles and
    releases every window, and the lock surfaces arrive before the signal.
  - (c) Re-examine the windows registered just before the lock
    notification. Rejected: a time window is a guess, and the band already
    covers the lock surfaces.
- **Frozen while locked.** The foreground push does nothing
  (`set_foreground_window_impl`, a debug line), the backstops of the
  floating layer and of book mode leave the z-order alone, and nothing is
  pre-placed. A tile closed during the lock still leaves its strip; the
  strip is not frozen against its own windows going away.
- **Disconnect and connect** (`WTS_CONSOLE_DISCONNECT`,
  `WTS_REMOTE_DISCONNECT` and their connects). Not handled, logged at debug:
  a disconnect does not always come with a lock, and the matching connect is
  not an unlock, so it is not the same path. Not measured in the VM.

## Decision

The per-entry outcome, from the probe above and ADR-0033's:

| entry | measured band | outcome | why |
|---|---|---|---|
| `SHELL_CLASSES` `Shell_TrayWnd`, `Shell_SecondaryTrayWnd` | 1 | kept | the taskbars are desktop band |
| `SHELL_CLASSES` `Progman`, `WorkerW` | 1 | kept | the desktop is desktop band |
| `SHELL_CLASSES` `FlowBorderOverlay`, `FlowSessionSink` | 1 | kept | flowd's own windows |
| `SHELL_CLASSES` `Windows.UI.Core.CoreWindow` | 4, 6, 13, 17 when shown; 1 only cloaked or never shown | removed | every shell instance that can take the foreground is in a shell band |
| `SHELL_CLASSES` `LockScreenBackstopFrame`, `LockScreenInputOcclusionFrame` | 17 | removed | lock band |
| `SHELL_CLASSES` `XamlExplorerHostIslandWindow` | 5, 16 | removed | Task View, Alt+Tab |
| `SHELL_CLASSES` `ForegroundStaging` | 16 | removed | Alt+Tab band |
| rule `class = "Shell_TrayWnd"`, `"Shell_SecondaryTrayWnd"` | 1 | kept | desktop band |
| rule `class = "Windows.UI.Composition.DesktopWindowContentBridge"` | 1 | kept | an app helper, not a shell surface |
| rule `exe = "StartMenuExperienceHost.exe"` | 6 | removed | Start band |
| rule `class = "Xaml_WindowedPopupClass"` | 1 in the guest, 7 on the host | kept | Win+X menu, tray overflow, snap layouts are desktop band on 22621; WinUI 3 apps use the class too |
| rule `exe = "SearchUI.exe"` (Windows 10) | not measured | removed | Windows 10's search sits in the search band like SearchHost (13); flow targets Windows 11 |
| rule `exe = "SearchHost.exe"` | 6, 13 | removed | |
| rules `exe = "ShellExperienceHost.exe"` (with and without the CoreWindow class) | 4, 6 | removed | |
| rule `exe = "LockApp.exe"` | 17 | removed | |
| rule `exe = "explorer.exe", class_regex = '^LockScreen'` | 17 | removed | |
| rules `OSKMainClass`, `Magnify.exe`, `Narrator.exe` | 2 (Narrator not measured) | kept | the UIAccess band is managed by design |
| every other bundled rule (dialogs, explorer windows, Task Manager, Chromium, apps) | 1 | kept | application windows |
| `classification::is_lock_surface`, `WindowRegistry::drop_hidden_lock_surface` | | removed | the unlock drops hidden shell-band windows |

The session lock:

- `daemon::session::SessionSink` registers for session notifications when
  it is created and withdraws on drop (`WTSUnRegisterSessionNotification`);
  a failed registration is one warning, and the daemon works as before.
  `WTS_SESSION_LOCK` / `WTS_SESSION_UNLOCK` are logged
  (`session sink: the session was locked` / `unlocked`) and reported through
  `registry::hooks::report_session_lock`, which wakes the main loop.
- The main loop applies the change at the start of `process_hook_events`,
  before the hook events are drained.
- Locked: `register_window_from_info` gives a window that passes the
  elevated and shell-band gates `Ignored(SessionLocked)`; the log line is
  `window 0x.. (exe "class") arrived while the session is locked; held until
  the unlock`; `flow query all` shows `Ignored::SessionLocked`; `set-window`
  on it is refused (`... arrived while the session was locked; flow
  classifies it at the unlock`).
- Unlocked: one info line, `session unlocked: N held window(s), C
  classified, D dropped; S hidden shell surface(s) dropped`.
- A daemon started while the session is already locked treats it as
  unlocked; it starts at logon.

## Consequences

- The bundled rules name no shell host any more; a new shell surface in a
  shell band needs nothing, one in the desktop band (the Widgets board is
  one) still needs a rule.
- Learned rules keyed on CoreWindow are no longer filtered out; the band
  gate runs ahead of every rule, so they cannot reach a shell-band window.
- A user rule `exe = "explorer.exe"` without a class no longer reaches the
  lock frames (ADR-0030's documented gap is closed): the band outranks it.
- Windows a normal process names like the lock surfaces (the stage 36
  stand-ins) are ordinary windows now. Created during a lock they are held
  like any other arrival.
- Without the band export the removed surfaces are classified by
  `default_action` (see *Considered Options*).
- Not measured: the real lock surfaces on the host build; check the daemon
  log after the first lock there (`... is in shell band 17; left
  unmanaged`).

## Tests

Unit tests: `registry/classification.rs`
(`embedded_default_rules_leave_band_covered_shell_surfaces_to_the_band`),
`registry/core.rs` (`a_window_arriving_during_the_lock_is_held`,
`the_unlock_takes_held_windows_and_drops_hidden_shell_surfaces`,
`the_session_lock_flag_round_trips`), `daemon/hooks.rs`
(`the_unlock_releases_held_windows_and_keeps_the_tiles`),
`daemon/session.rs` (`only_lock_and_unlock_change_the_lock_state`),
`registry/hooks.rs` (`a_session_lock_report_is_taken_once`). VM:
`test-stage40.ps1` in the VM harness (A1-A2 the removed entries, B1-B5 a
real lock with stand-ins created while locked, O the probe's ordering), and
`test-stage36.ps1` with the expectations ADR-0034 changes.
