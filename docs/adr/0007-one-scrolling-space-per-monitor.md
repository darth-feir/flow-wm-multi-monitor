# One scrolling space per monitor, parking beyond the virtual screen

**Status:** Accepted — closes the roadmap's "Multi-monitor support" item. The
data model already allowed several monitors; this records how the fork made
the daemon use them and the rules that keep a window from ever being drawn on
the wrong display.

Every display gets its own `Monitor` — a work area, ten workspaces, its own
`ScrollingSpace` and `FloatingSpace` — and a window belongs to the monitor it
appears on. Off-viewport columns and inactive workspaces are parked **beyond
the Win32 virtual screen** on a side that has no neighbour, never over an
adjacent display, and any move whose start or end is a parking slot on a
blocked side is applied **instantly** instead of tweened. A display change
(attach, detach, resolution, taskbar move) is reconciled in one debounced pass
that matches old monitors to new ones, migrates the windows of a vanished
display and restores the windows Windows minimized on the way.

## Context

Upstream enumerated one monitor; `MonitorInfo` and the projection already
spoke in terms of a work area, so the model was not the obstacle. The
obstacles were all about *where a window may be put* when there is a second
display:

- The one-monitor parking rule ("one monitor height away") lands parked
  windows inside a neighbouring display on a dual setup, and Windows cannot
  clip a window at a monitor edge — a column half off the right edge of
  DISPLAY1 is simply visible on DISPLAY2.
- A tween from a parked slot to a visible one sweeps the window across the
  neighbour; sampling at 30 ms in the VM showed DISPLAY1 windows crossing
  DISPLAY2 on every jump and workspace switch.
- The hooks had no notion of an owning monitor, so a window minimized on one
  display reappeared on the other, and focus changes did not move the active
  monitor.
- Windows minimizes every window whose nearest display vanishes — including
  a *surviving* monitor's columns that were parked beyond the vanished
  neighbour — so a hotplug without a reconcile left half the strip iconic.

## Considered Options

- **One global scrolling strip spanning all monitors.** Rejected: a strip
  has one viewport; two displays would show two slices of the same canvas and
  every scroll would move both. niri, the model for this fork, keeps one strip
  per output.
- **Virtual-desktop style "park by hiding" (`ShowWindow(SW_HIDE)` or DWM
  cloak) instead of off-screen rects.** Rejected for now: hiding changes what
  the taskbar, Alt-Tab and the apps themselves see, and the cloak write side
  is a separate roadmap item. Parking stays geometric; the rule is only *which
  side* is safe.
- **Park off-screen, pick the free side, teleport across blocked sides
  (chosen).** `MonitorInfo` carries `virtual_screen` and four neighbour
  flags; the projection parks beyond the virtual screen on a free side, parks
  (rather than partially shows) a column that would overhang toward a
  neighbour, and the single `plan_move` helper every submit path uses marks a
  target `instant` when either endpoint is a parking slot on a blocked side.
  Instant targets are applied in one backend call before any tween starts and
  evict the window from an in-flight batch, so a mid-tween retarget cannot
  carry it across the neighbour either.
- **Hotplug by restart.** Rejected: the user's windows are mid-layout; a
  restart loses workspace memory and re-tiles from scratch. The reconcile
  keeps every survivor's workspaces, re-projects them in place and migrates
  only what lost its display.

## Consequences

- **Every submit path goes through one helper.** Layout, workspace switch,
  loadout seating, float/tile toggles, hotplug seating and the tiling resume
  all build targets with `plan_move` / `push_targets`; the teleport rule
  lives in one place and a new path that bypasses it reintroduces the sweep.
- **Windows remember their owning monitor.** Restore, show and state-change
  re-adds use the memory, then `MonitorFromWindow`, then the rect, then the
  active monitor. Loadout snapshots carry the monitor index (older files
  default to 0).
- **Cross-monitor commands are geometric.** `focus-monitor` /
  `move-to-monitor <dir>` pick the monitor beyond the edge sharing the most
  perpendicular extent; `focus` / `move-window` at the end of a strip hop to
  the neighbour unless `[navigation] edge_hop = false`. Several monitors'
  layouts are submitted as one animator batch, because a second submission
  used to supersede the first.
- **The reconcile is debounced and ordered.** Windows sends several
  `WM_DISPLAYCHANGE` / `SPI_SETWORKAREA` per change; the daemon re-arms a
  300 ms settle timer and runs once, retrying up to five times on a transient
  empty enumeration. The animator is cancelled and joined first. Casualties
  (tracked windows minimized at or after the first report, one-second grace)
  are restored with `SW_SHOWNOACTIVATE` — never `SW_RESTORE`, which bounces
  the foreground through each of them — and re-seated in their old
  `(monitor, column, row)` order. A window the user minimized before the
  change stays minimized. Everything is seated as instant targets: the old
  positions belong to a geometry that no longer exists.
- **Accepted costs.** Parking beyond the virtual screen relies on Windows
  tolerating far-off-screen rects (it does; `flow rescue` and the shutdown
  rescue bring them back if the daemon dies). The reconcile re-reads only the
  monitor list; per-monitor DPI differences are not modelled (`flowd` runs
  with the system DPI awareness upstream chose). A reconcile that lands inside
  a tile drag is postponed by one settle period rather than cancelling the
  drag.
- **Tests.** The parking and teleport rules are pure projection/mutation
  tests; the hotplug pass runs against a `TopologyProbe` mock (removal with
  migration, addition, work-area-only change, restore order, a survivor's
  parked column returning to its slot). The VM harness (`flowwm-vm`,
  two-display Hyper-V guest) samples window rects during scrolls and switches
  to assert that no DISPLAY1 window is ever observed over DISPLAY2. See
  `docs/src/dev-guide/ipc-and-watchdog.md` (*Monitor hotplug*) and
  `docs/src/dev-guide/layout/projection.md`.
