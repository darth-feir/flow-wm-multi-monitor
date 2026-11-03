# The floating layer follows ADR-0003's invariant, with a knob and a shell exemption

**Status:** Accepted — implements the invariant of
[ADR-0003](0003-float-tile-stacking-invariant.md) (floats TOPMOST while flow
owns the foreground, dropped otherwise) in the fork's `[floating]
always_on_top` layer, and borrows the "drop = seat below the foreground"
mechanism from upstream's unmerged ADR-0007 (`feat/float-drop-lower`). Two
deliberate deviations are recorded below: the layer stays behind a config
option, and shell surfaces do not flip it.

The layer's members — tracked `Floating(Active)` windows and tracked
`Ignored(ExplicitRule)` windows — are `WS_EX_TOPMOST` **only while the
foreground is a flow window**: a registry window in `Tiling(Active)`,
`Floating(Active)` or `Ignored(ExplicitRule)` that is not fullscreen by a
live geometry-and-style check. Any other foreground — a window fullscreen on
its monitor, an untracked window, a registry window in any other state —
**drops** every member the daemon raised: `SetWindowPos(member,
hwndInsertAfter = foreground, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)`,
which clears TOPMOST and seats it directly under that foreground in one
call. A shell surface taking the foreground (taskbar, desktop, Start/search,
Alt-Tab host, the daemon's own windows) and a null foreground leave the last
decision in place. The rule is one pure function, `classify_foreground`, run
on exactly one window, the foreground.

## Context

Stage 15 of the fork built the layer the way niri does it — floats above
the strip at all times — and gated it behind `always_on_top` because upstream
users expect stock stacking. ADR-0003 (already on master at the fork point)
rejects exactly that option: an always-topmost float covers fullscreen video,
borderless games and other applications' windows, which PowerToys
Always-On-Top users hit as well (its issue #27088). The fork's layer let go
only of a window the daemon itself made fullscreen with Mod+F; a browser in
F11 or a game had floats sitting on top of it. The maintainer's own
unmerged branch goes further and lowers floats *below* the foreign
foreground, because `HWND_NOTOPMOST` alone leaves them at the top of the
ordinary band — still above the fullscreen window.

## Considered Options

- **Keep "always topmost" and document the limitation.** Rejected: it is
  the one design conflict with an accepted upstream ADR, and the user's own
  reason for the layer ("I should not have to hunt for a float") is served
  just as well when the float only yields to a fullscreen or foreign window.
- **Drop with `HWND_NOTOPMOST`.** Rejected after the VM showed the float
  still above a borderless full-screen window: clearing the flag puts the
  member at the top of the non-topmost band, which is where the foreground
  also lives. Seating it below the foreground (`insert_below`) is one call
  and gives the z-order fullscreen > float > tiles.
- **Decide on every window's state** (members in, members out). Rejected:
  the invariant is about one window, the foreground. One lookup plus the
  live fullscreen check per foreground change is the whole cost.
- **Treat every non-flow foreground alike** (ADR-0003's wording). Deviated:
  the taskbar, the desktop, Start/search, the Alt-Tab host and the daemon's
  own session sink and border overlays take the foreground for a moment on
  ordinary clicks; flipping the layer on each would flash every float on
  every taskbar click. These classes **keep the last decision**. The keymap's
  OSD is not on the list: it is shown without activation and never takes the
  foreground, and any other AutoHotkey window is an ordinary app.
- **Remove the config knob** (ADR-0003 ships always-on). Deviated: upstream's
  default behaviour is unchanged when `always_on_top = false`, and the fork's
  users opted in. The knob stays, default `false`.
- **Live fullscreen by the existing classifier.** Rejected: `is_fullscreen`
  is primary-monitor only. The live check (`is_fullscreen_on_its_monitor`)
  compares `GetWindowRect` with the `rcMonitor` of `MonitorFromWindow` and
  requires neither `WS_CAPTION` nor `WS_THICKFRAME`, so a maximized window,
  which keeps its chrome, never reads as fullscreen. The classifier is left
  as it was.

## Consequences

- **Where the rule runs.** In `on_focus_changed`, the single foreground
  sink (after register / reclassify, before the border refresh); on the
  foreground window's own `LOCATIONCHANGE` and `STATECHANGE` (F11 resizes,
  it does not change state — the hook forwards `LOCATIONCHANGE` for the
  current foreground and the loop re-decides only when the moved window is
  the foreground); in the periodic `reassert_top_layer` pass (`[focus]
  foreground_sync_interval_ms`), which reads the foreground itself and then
  re-raises demoted members while raised; and in `sync_all_top_layer`
  (tiling on, config reload, loadout load, display change, startup, which
  seeds the foreground from `GetForegroundWindow`).
- **Was-topmost memory survives.** A drop touches only members the daemon
  raised; a window that was topmost on its own is left alone, and a raise is
  the ordinary `Enter`, so stop / tiling off / option off still restore the
  flag the window had before the daemon met it (unchanged from stage 15).
- **Mod+F is unaffected.** The daemon's fullscreen window keeps its chrome,
  so it reads as a flow window; the fullscreen code keeps owning its own
  topmost flag.
- **Visible side effects, accepted.** A dialog owned by a tile (Save As) was
  non-flow, so the layer dropped under it and raised again when the owner
  was activated. *Superseded by
  [ADR-0024](0024-owned-windows-ride-the-floating-layer.md):* a window owned
  by a flow window is classed by its owner and rides in the topmost band
  with the layer, because the drop seated an owner under its own popup and
  the next raise covered the popup (the SSMS error box, 2026-11-02). A window
  that takes the foreground before it is registered (created
  invisible and titled late, like Windows Terminal) drops the layer and
  re-raises when it is registered a moment later: one flash per such launch,
  inherent to "untracked = drop". A daemon started while an untracked window
  is foreground begins dropped. The boundary flicker ADR-0003 accepts applies
  here too.
- **Observability.** `flow ping` prints `floating layer raised|dropped`
  (`Pong.floating_layer`, optional, older daemons parse); `flow query all`
  keeps `on_top`, false for members while dropped; the drop logs the
  foreground's class and why it is foreign.
- **Tests.** The rule table, the raise/drop/raise round trip with the
  `insert_below` calls, an F11-style live-fullscreen drop and chrome-back
  raise, shell and null foregrounds keeping state, an OS-ignored registry
  foreground dropping, a self-topmost member across a drop, the periodic
  pass both ways, and tiling off/on, option off and release while dropped,
  all against the `TopmostProbe` mock (which gained `insert_below`,
  `is_fullscreen_live` and `class_name`). The VM driver (`test-stage19.ps1`)
  checks the z-order under a borderless full-screen window, a stripped-chrome
  tile, an untracked tool window, and real taskbar and Win-key input; stages
  15 and 18 pass unchanged. See `docs/src/dev-guide/floating-space.md`
  (*The floating layer*).
