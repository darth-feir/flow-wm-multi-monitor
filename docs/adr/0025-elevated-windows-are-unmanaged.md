# Elevated windows are unmanaged

**Status:** Accepted (stage 31b, 2026-11-03). Adds an ignore reason that outranks
every classification rule and the maximized / fullscreen overrides, and
keeps it out of the floating layer ([ADR-0013](0013-floating-layer-follows-adr-0003.md),
[ADR-0024](0024-owned-windows-ride-the-floating-layer.md)). The other stage 31b
change, the square ring for a window without chrome
(`docs/src/dev-guide/borders.md`, *Corner preference*), is amended by
[ADR-0027](0027-restore-seat-and-frameless-corners.md): a window whose client
area covers the whole window is square too.

## Context

On 2026-11-03 an Alacritty started as administrator (Mod+Shift+T,
`admin = true`) was seated as a column on the host. Every `SetWindowPos`
the animator sent it failed (`apply_batch error for instant targets:
backend error: Access is denied`), so the column never moved: it stayed
where Windows had opened it, overlapping its neighbours, while the strip
reserved a slot for it and the log filled with warnings.

flowd runs at medium integrity. User Interface Privilege Isolation (UIPI)
lets a lower-integrity process neither move, resize, re-order nor activate
a higher-integrity window (`SetWindowPos`, `ShowWindow`,
`SetForegroundWindow` and most messages are refused). Nothing flow does to
such a window can take effect.

## Considered Options

- **(a) Detect elevation at arrival and record the window as
  `Ignored(Elevated)`.** Chosen.
- **(b) Seat it and tolerate the failures.** Rejected: the strip keeps a
  slot the window never fills, the neighbours are placed around a window
  that is elsewhere, and every frame warns.
- **(c) An ignore rule in `flow-rules.toml` for elevated apps.** Rejected:
  elevation is a property of the process, not of the exe (the same
  Alacritty runs elevated and not), and rules cannot see it.
- **(d) Compare integrity levels instead of `TokenElevation`.** Not needed:
  for windows of the user's own session the two agree, and the
  elevation query is the one Task Manager shows; a process flow cannot
  inspect at all is treated as elevated anyway.
- **(e) Make it a floating-layer member like an explicit-rule ignore.**
  Rejected: the layer cannot lift it (`HWND_TOPMOST` is refused too), so
  while it holds the foreground the raised floats would cover it.

## Decision

- **The gate.** `get_window_info` reads the window's process token:
  `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`,
  `OpenProcessToken(TOKEN_QUERY)`, `GetTokenInformation(TokenElevation)`.
  This works across integrity levels for the same user. An elevated
  process, or one whose token cannot be read (another user, a protected or
  system process: flow could not move it either), sets
  `WindowInfo::is_elevated` (`win32::elevation_blocks`). A window without a
  process (gone) is not elevated.
- **Skipped when flowd is elevated** (someone started the daemon as
  administrator, `self_is_elevated`, read once): it can move elevated
  windows, so they are managed as usual.
- **Registration.** `register_window_from_info` records such a window
  `Ignored(Elevated)` before any rule or OS-state override, iconic or not,
  and logs once at info: `window 0x.. (exe) is elevated; left unmanaged
  (UIPI)`. `flow query all` shows `Ignored::Elevated` (mode `ignored`).
- **Never re-evaluated.** A process's elevation is fixed for its life; the
  maximized / fullscreen recovery only re-classifies `Ignored(Maximized |
  Fullscreen)`, and reload never reclassifies tracked windows.
- **Nothing is done to it.** No seat, no border, no pre-placement (stage
  30b checks the gate before its synchronous move), no learned rule:
  `set-window` on it is refused (`window 0x.. (exe) is elevated; flow
  cannot move it (UIPI)`), so nothing is recorded; `minimize-window` refuses
  ignored windows.
- **Foreign to the floating layer.** `Ignored(Elevated)` is not a layer
  member (`in_top_layer`) and not a window flow owns in the foreground
  (`owns_foreground`): when it takes the foreground the layer drops under it
  as under any other app, and its owned dialogs never become riders (their
  anchor does not own the foreground). So the layer never sends it a z-order
  call that UIPI would refuse.

## Consequences

- An elevated terminal or Task Manager (which runs elevated on an admin
  account) opens where Windows puts it and stays there; the user moves it.
  The strip does not reserve a slot for it and no warnings are logged.
- While it holds the foreground, floats are not topmost (the layer is
  dropped), so they never cover it; the next flow window to take the
  foreground raises the layer again.
- Keymap chords still reach the daemon while an elevated window is in the
  foreground only if the keymap itself can see the keys (AutoHotkey at
  medium integrity does not receive input aimed at an elevated window);
  focus commands then act from the last tracked focus.
- A daemon started elevated manages everything, including elevated
  windows, as before this change.
- **Tests.** Unit tests for the gate decision (`elevation_blocks`), a
  handle without a process, registration of an elevated window (also
  maximized or iconic) as `Ignored(Elevated)`, `set-window` and
  `minimize-window` refusing it, and its exclusion from the layer. The VM
  driver `test-stage31b.ps1` (E cases) starts a test window elevated in the
  guest and checks the log, the query and the strip.
