# Tiling on/off is a pause inside the daemon, not a stop

**Status:** Accepted.

`flow dispatch tiling on|off|toggle` hands the desktop back to Windows
without ending the daemon. **Pause** moves every tile of every workspace of
every monitor on-screen in one instant batch (to the rect it had before it
was first tiled this run, else its layout rect clamped into its monitor),
clamps floats that sit off the virtual screen, fixes the restore placement
of minimized windows, drops every border overlay and refuses every
layout-mutating command. The monitors, workspaces, strips and floating
spaces are kept untouched as the memory. **Resume** rebuilds from the live
desktop: re-scan, drop vanished windows, keep live tiles in their remembered
`(monitor, workspace, column, row)` seats, append windows that appeared
during the pause on the monitor under them, re-seat everything instantly.
The state is never persisted; a restart always starts tiling.

## Context

The user's requirement: *a toggle to turn tiling on and off; when it is off
the desktop goes back to the normal Windows window manager and the apps are
moved back on screen.* The bundled keymap owns the toggle key, and the keymap
is tied to the daemon's lifetime (job object, ADR-0009), so "off" cannot mean
"stop the daemon" — there would be nothing left to turn it back on. The
windows also had to come back where the user put them, not to their tiled
rects: a parked workspace's windows live beyond the virtual screen and would
otherwise be invisible under plain Windows.

## Considered Options

- **Stop the daemon and let the resident starter restart it.** Rejected:
  loses every workspace and column memory, and the shutdown rescue only
  brings windows on-screen, it does not know the pre-tiling rect. The fork
  later added a resident starter for the *full* on/off switch (Mod+Ctrl+T)
  on top of this pause, so both exist with different costs.
- **Keep tiling but stop issuing `SetWindowPos`** (a "frozen" daemon).
  Rejected: parked windows stay parked, borders stay on screen, and the next
  hook event re-seats whatever the user moved.
- **Pause inside the daemon with explicit pre-tiling rects (chosen).**
  `Window::pre_tiling_rect` is recorded the first time a window is seated
  (from `pre_manage_rect`, or from the float rect on `set-window tile`) and
  never refreshed by later float/tile cycles, so a window tiled an hour ago
  goes back to where it was before the daemon touched it. Windows adopted
  from a maximized state (ADR-0010) are maximized again instead of moved.
- **Resume from the remembered layout only.** Rejected: the user may have
  closed, opened, minimized or dragged windows during the pause. Resume
  therefore re-scans like startup does and reconciles the memory against the
  desktop, including a display change that arrived while paused (deferred,
  then reconciled first).

## Consequences

- **While paused the daemon is inert but awake.** Hooks keep the registry
  consistent (create/destroy/minimize/restore/show/state change are
  registry-only, foreground only updates tracked focus), drag handling never
  starts, float-sync and edge-scroll are no-ops. Every command for which
  `SocketMessage::is_rejected_while_tiling_paused` is true answers
  `tiling is off (flow dispatch tiling on)`; `ping`, the queries, `stop`,
  `loadout save` and the toggle itself keep working. The state is reported by
  `flow ping`, `flow query all` and the status file.
- **Both passes are one instant batch.** A tween would animate windows
  across the desktop the user has just been handed, and on resume the previous
  positions belong to Windows' arrangement, not the layout's.
- **The pause is where the daemon releases everything it holds on a
  window:** borders, the topmost flag of floating-layer members
  (ADR-0013), the maximized state it undid. A new per-window side effect must
  be undone in `pause_tiling` or it leaks into the plain-Windows session.
- **Accepted cost.** The keymap keeps the Win key while paused (its chords
  are gated on the daemon's pipe, not on tiling), so Win+Arrow is still the
  daemon's, not Windows Snap's; the full on/off switch exists for the user who
  wants stock Windows keys back.
- **Tests.** The Win32 questions are behind `TilingProbe`; the pause
  targets, the refusal gate, the registry-only hooks, the remembered-seat
  resume with a newly discovered window, the off/on round trip and the
  deferred display change run against a mock desktop; `tests/cli/tiling.rs`
  covers the CLI. See `docs/src/dev-guide/ipc-and-watchdog.md`
  (*Tiling on/off*).
