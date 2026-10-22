# Roadmap and Future Work

Where `flow` is headed. The core tiling engine, the scrolling canvas, the
floating space, and niri-style workspace switching are all implemented and
exercised end-to-end. What remains is polish, the long tail of IPC commands,
multi-monitor support, crash recovery, and the aspirational input-driven
features.

## Status at a Glance

| Area | Status | Note |
|---|---|---|
| Tiling engine + scrolling canvas | ✅ Done | `mutate → project → animate` pipeline |
| Window lifecycle hooks | ✅ Done | create/destroy/minimize/restore/show-hide/foreground + `STATECHANGE`/`NAMECHANGE` |
| Dispatch surface | ✅ Mostly | focus, swap, scroll, widths, center, monocle, close — see [Remaining IPC stubs](#remaining-ipc-stubs) for the gaps |
| Floating space | ✅ Done | tile↔float, centered placement, merged scroll+float batches |
| Workspaces | ✅ Done | 10 per monitor; switch + move-to animate; `SwapWorkspace` pending |
| Classification + learned rules | ✅ Done | DWM-cloak/iconic/Alt-Tab/owner filters + 4-layer rule pipeline |
| Animation | ✅ Done | 31 easing curves + `CubicBezier`, `RetargetFromCurrent`, `MockBackend` |
| Graceful-shutdown rescue | ✅ Done | `rescue_stranded_windows` restores off-screen windows on clean exit |
| Mouse-drag follow on tiled windows | ✅ Done | title-bar drag reorders live (`src/daemon/drag.rs`, [Tile Drag](./tile-drag.md)); the fork adds Mod + left-button drag anywhere in the window via keymap-driven sessions (`drag begin` / `drag end`, ADR-0011) |
| `SwapWorkspace` animation | 🜂 Near-term | only workspace command still stubbed |
| Remaining IPC stubs | 🜂 Near-term | `PlaceAbove`, `QueryState`, `CheckConfig`, `SetConfigValue`, `ForgetApp`/`ForgetAllApps` |
| Floating enhancements | 🜂 Near-term | smart placement, size memory, z-order, gap mgmt |
| Real cross-column `MoveWindow` | 🜂 Near-term | Left/Right aliased to swap-column; row-preserving move + floating nudge pending |
| Multi-monitor support | ✅ Done | one `Monitor` + scrolling space per display; windows tile on the monitor they appear on; parked columns and parked workspaces stay off every monitor (neighbours modelled on all four sides); cross-monitor focus/move commands (`focus-monitor`, `move-to-monitor`, edge hops) — see [Command Reference › Monitors](../user-guide/dispatch-reference.md#monitors) |
| Tiling on/off toggle | ✅ Done | `flow dispatch tiling on\|off\|toggle` (`Alt + Shift + t`): a pause inside the daemon — every tile back on-screen (pre-tiling rect where known, parked workspaces included), borders dropped, hooks registry-only, layout commands refused — and a resume that rebuilds from the live desktop with remembered seats; reported by `flow ping`, `flow query all` and the status file — see [IPC › Tiling on/off](./ipc-and-watchdog.md#tiling-onoff-pause-and-resume) |
| Monitor hotplug | ✅ Done | `WM_DISPLAYCHANGE` / `SPI_SETWORKAREA` debounced into one reconcile: monitors matched by handle → rect → primary, survivors re-projected in place, windows of a vanished display migrated to the monitor Windows reports for them (else the primary), owner memory remapped, everything re-seated instantly — see [IPC & fail-back](./ipc-and-watchdog.md#monitor-hotplug) |
| Cloak parked windows (`DWMWA_CLOAK`) | ◷ Mid-term | read side exists; write side missing |
| Watchdog + recovery snapshot | ◷ Mid-term | not started; gated on `flow`/`flowd` being complete |
| Input hooks (DragSession etc.) | ◷ Aspirational | `src/input/` does not exist; drag is covered from the keymap (ADR-0011), resize is not |
| Coexistence warning | ◷ Aspirational | detect komorebi / GlazeWM at startup |

## Implemented Baseline

The `mutate → project → animate` pipeline, the window registry, and the
Win32 hook thread are all in place and driving real window management:

- **Window lifecycle**: create, destroy, minimize, restore, show/hide,
  foreground, plus the `STATECHANGE` (maximize/fullscreen recovery) and
  `NAMECHANGE` (late-title recovery) hooks. See
  [Event Pipelines](./event-pipelines.md).
- **Dispatch surface**: focus, per-window swap, column swap, semantic move,
  scroll, expand/shrink/set column width, center, monocle toggle, close
  window. See [IPC & Watchdog](./ipc-and-watchdog.md).
- **Floating windows**: tile↔float transitions (`set-window float|tile|cycle`),
  centered default placement, and merged scroll+float animation batches. See
  [Floating Space](./floating-space.md).
- **Workspaces**: ten workspaces per monitor; `switch-workspace` and
  `move-to-workspace` animate a vertical-packing switch (animate / teleport /
  skip partitioning). See [Workspace Hierarchy](./workspace.md).
- **Classification**: DWM-cloak, iconic, Alt-Tab visibility, and owner
  pre-filters, plus the four-layer user/learned/default/`default_action`
  rule pipeline. See [Classification & Learned Rules](./classification.md).
- **Animation**: 31 named easing curves plus arbitrary CSS `cubic-bezier`,
  `RetargetFromCurrent` mid-flight retargeting, and a `MockBackend` for
  tests. See [Animation](./animation.md).
- **Graceful-shutdown rescue**: on clean exit the daemon runs
  `rescue_stranded_windows`, moving any off-screen window back onto the
  active monitor using its `pre_manage_rect` as the anchor. See
  [IPC & Watchdog](./ipc-and-watchdog.md).

## Near-term: Polish and Completeness

The wiring-heavy work is finished. The remaining near-term items close gaps
in the IPC surface, make tiled windows feel responsive to direct
manipulation, and round out the floating and workspace feature sets.

### React to Mouse Dragging of Tiled Windows (done)

A title-bar drag of a tile is followed live: the other windows reflow to
show the drop zone and the layout commits on release, entirely inside the
tiling model ([Tile Drag](./tile-drag.md)). The fork adds niri's Mod +
left-button drag from anywhere in the window: the keymap moves the window
and brackets the gesture with `flow dispatch drag begin` / `drag end`, which
put the daemon in the same drag state (ADR-0011).

### Workspace: SwapWorkspace

`SwapWorkspace` is the only workspace command still routed to
`unimplemented_command`. `SwitchWorkspace` and `MoveWindowToWorkspace` are
fully implemented with a vertical-packing animation model; `SwapWorkspace`
needs its own animation decision because it exchanges two workspaces'
positions in the packed stack rather than sliding between them. Its protocol
shape (`SwapWorkspace { workspace_id }`) is already locked in.

### Remaining IPC Stubs

Several `SocketMessage` variants are declared and parsed by the CLI but
return `unimplemented_command` on the daemon side. Their wire formats are
stable so external tooling and keybindings can target them now:

| Command | Purpose |
|---|---|
| `PlaceAbove` | Raise the focused floating window's z-order (`SetWindowPos` with `HWND_TOPMOST`/restore) |
| `QueryState` | Read-only introspection of daemon/registry state beyond `QueryWindowsAll` |
| `CheckConfig` / `SetConfigValue` | Runtime config validation / mutation without a daemon restart (`ReloadConfig` is already implemented) |
| `ForgetApp` / `ForgetAllApps` | Programmatic clearing of learned rules (today this requires hand-editing `history-flow-rules.toml`) |

See [Classification & Learned Rules](./classification.md) for the learned-rules
model that `ForgetApp` would expose programmatically.

### Floating Window Enhancements

The floating space is functional; the open work is quality-of-life. See the
"Future Work" section of [Floating Space](./floating-space.md) for the full
list, summarised here:

- **Smart placement** — cascade or offset new floats so they don't fully
  overlap.
- **Per-window float size memory** — remember each window's last floating
  rect and restore it on subsequent tile→float transitions.
- **Z-order raising** — `PlaceAbove` to bring the focused float to the top
  of the z-order (depends on the IPC stub above).
- **Floating gap management** — reserve padding around floats at workspace
  edges.

### MoveWindow: Real Cross-Column Move and Floating Nudge

`MoveWindow`'s up/down path is implemented (it delegates to
`dispatch_swap_window` for an in-column swap). Two paths remain deferred:

- **Tiled left/right, row-preserving** — `Left`/`Right` are currently
  aliased to `SwapColumn` (a column swap), not a true move that preserves
  row membership. The alias is intentional as a placeholder; a real
  cross-column extract-and-insert is the missing piece.
- **Floating, any direction** — a pixel nudge by a configurable shift.

The branching structure already lives behind a single delegation point in
`dispatch_move_window`, so these land without changing the IPC protocol or
keybindings.

## Mid-term

### Multi-Monitor Support

**Vertical arrangements (done).** `MonitorInfo` carries `neighbour_up/down`
next to `neighbour_left/right`, inactive workspaces on a monitor with a
display stacked above or below park beyond the virtual screen on that side
(`WorkspaceParking`), and the neighbour-teleport rule in the animation bridge
reads all four flags. Still open: a free vertical side on a *diagonal*
arrangement parks a workspace one height away, which is off every monitor but
inside the virtual-screen bounding box, so the shutdown rescue (which tests
overlap against that box) leaves such a window where it is.

**Hotplug (done).** The monitor list follows the desktop at runtime:
`WM_DISPLAYCHANGE` and `SPI_SETWORKAREA` are debounced into one reconcile that
matches old monitors to the new enumeration, re-projects survivors, migrates
the windows of a vanished display and re-seats everything instantly. See
[Monitor hotplug](./ipc-and-watchdog.md#monitor-hotplug). Still open: a
window the user had deliberately minimized on a display that is then
unplugged is restored along with the ones Windows minimized (the two are
indistinguishable), and a reconcile that lands mid-drag is simply postponed.

The workspace hierarchy models `Vec<Monitor>` with `active_monitor: usize`,
and every IPC command routes through `active_scrolling()`; the constructor
builds one `Monitor` per display from `EnumDisplayMonitors` + `GetMonitorInfoW`
([`src/daemon/new.rs`](../../src/daemon/new.rs)).

### Performance: Cloaking Off-Screen Windows

Parked (off-screen) tiled windows are kept one column-width beyond the
nearest viewport edge so they animate smoothly when scrolled into view.
They are, however, still rendered. A future optimisation can apply
`DWMWA_CLOAK` (`SetWindowCompositionAttribute`) to parked windows so the
DWM skips compositing them, reducing GPU work on large canvases. The
classification pipeline already inspects `DWMWA_CLOAKED`; the write side
is the new work.

### Watchdog and Recovery-Snapshot Persistence

**Not started.** This is deliberately deferred until `flow` and `flowd` are
feature-complete; it is tracked here as the intended design.

The planned design: `flowd` spawns a separate `flow-watchdog` helper
process with `--parent-pid` and `--recovery-path` arguments. The watchdog
polls the parent PID and, on unexpected exit, reads a
`flow-recovery.json` snapshot and calls `SetWindowPos` to restore each
window to its pre-manage geometry. The `Window` struct already carries
`pre_manage_rect` for this purpose; the atomic write-to-temp-then-rename
persistence path and the watchdog binary itself are what is missing. The
watchdog must survive even if the daemon is corrupted, hung, or crashed, so
it owns a separate process lifetime — the OS reaps the daemon while the
watchdog keeps running, with no dependency on daemon state (it reads a file
and calls Win32 APIs directly) and a minimal code path focused only on
recovery. A Windows service was considered and rejected — it requires admin
privileges for installation and adds a service-control-manager dependency;
a child process is simpler and sufficient for a single-user desktop tool.

On **graceful** shutdown, no watchdog is needed: the daemon performs the
equivalent restore inline via `rescue_stranded_windows` (already
implemented — see [Implemented Baseline](#implemented-baseline)). The
watchdog only covers the crash case.

See [IPC & Watchdog](./ipc-and-watchdog.md).

## Aspirational: Deliberately Deferred

These features are acknowledged as valuable but **not planned** for the
current development cycle.

### InputInterceptor, DragSession, ResizeSession

Full mouse-driven tiling where `Super + Left Mouse Button` initiates a drag
or resize session with layout snapping. This was described in the original
spec but has no active implementation work — the `src/input/` module does
not exist. The daemon's input surface today is the IPC pipe; mouse-driven
tiling would add an in-process global input hook alongside the existing
WinEvent hooks.

### Super+LMB Mouse Gestures

Super + left-button drag exists in the fork without an input hook: the
AutoHotkey keymap owns the gesture and the daemon exposes `drag begin` /
`drag end` sessions (ADR-0011). Resize gestures and anything that needs the
daemon to see the mouse itself still depend on the input hook above.

### Coexistence Warning

If `komorebi`, `GlazeWM`, or another tiling window manager is detected as
running, `flow` should display a warning and ask the user to close the
conflicting manager before using `flow`. Coexistence with another WM that
also moves windows will produce unpredictable results.

## Design Records

These sections record timeless constraints and deliberate design decisions,
not pending work.

### Keybindings Intentionally Removed

Keybinding handling was **intentionally removed** from both the config and
the codebase. The rationale: external tools like AutoHotkey, PowerToys, or
Komorebi's keybinding layer are better at translating physical keypresses
into IPC commands than a re-implemented keyboard hook. `flow`'s role is the
layout engine and window manager — not the input layer. See
[Design Decisions](./design-decisions.md) for more on this separation of
concerns.

Users map their preferred hotkeys to `flow dispatch` CLI calls via their
chosen keybinding tool. This keeps `flow`'s attack surface small and avoids
duplicating well-tested input infrastructure.

### Known Win32 Limitations

These are not bugs — they are inherent properties of the Windows rendering
model.

#### SetWindowPos vs DeferWindowPos

`flow` uses `SetWindowPos` (immediate positioning) rather than
`DeferWindowPos` (batch positioning). `DeferWindowPos` batches multiple
repositions into a single repaint, but it is atomic: a single elevated
admin window (protected by UIPI) fails the entire batch with
`ERROR_ACCESS_DENIED`. Individual `SetWindowPos` calls mean one failure
logs a warning but does not block the remaining windows. See
[Animation](./animation.md) for the per-backend rationale.

#### GetWindowRect Includes Invisible Borders

`GetWindowRect` returns a rect that includes a hidden ~7px border on the
left, right, and bottom edges. This is not the visual rect of the window.
`flow` works around this via the `InvisibleBounds` tracking in the registry.
See the [Window Registry](./window-registry.md) chapter for how invisible
bounds are measured and how `visible_to_window()` compensates.

#### Applications Own Their Render

`flow` can request a window position via `SetWindowPos`, but the application
controls its own rendering. Some apps (especially UWP and Electron-based)
may not immediately respect position changes or may reposition themselves
autonomously. This is a fundamental constraint of the Windows windowing
model.
