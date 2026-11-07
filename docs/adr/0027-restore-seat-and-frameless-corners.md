# A restored tile returns to its seat, and a frameless window has square corners

**Status:** Accepted (stage 33, 2026-11-06). Two host reports in one stage.
Part 1 makes `[minimize_restore] strategy` (documented since upstream, never
read by the daemon) take effect, with its default `"original_slot"`. Part 2
amends the corner rule of stage 31b
(`docs/src/dev-guide/borders.md`, *Corner preference*; noted in
[ADR-0025](0025-elevated-windows-are-unmanaged.md)).

## Part 1: the restore seat

### Context

On the host, restoring a minimized tile (taskbar click, Alt+Tab) made it the
active window but put it at the far right end of the strip, and the view did
not scroll to it: in a strip wider than the screen nothing visible changed.
Same in book layout. The restore hook appended the window
(`add_window`) and animated; the only thing remembered across a minimize was
the owning monitor. The registry did save a `last_virtual_slot`, but nothing
read it for the layout.

niri has no minimize. The closest reference is how it treats a focus change:
the focused column is always brought into view. A restore is a focus change
the user asked for, so that is what the user expects to see.

### Considered Options

- **(a) Remember the seat at minimize and re-seat there.** Chosen.
- **(b) Insert next to the focused column (`right_of_focused`).** Kept as a
  strategy, not the default: after a restore the user looks for the window
  where it was.
- **(c) Keep appending, only scroll the view.** Kept as `append_right`;
  fixes "nothing changes" but loses the arrangement.
- **(d) Switch to the remembered workspace when it is parked.** Rejected: a
  restore would then move every other window of that screen. Windows already
  shows the window; it lands in the visible workspace instead.

### Decision

- **What is remembered.** The minimize hook, before the tile leaves its
  strip, records a `RestoreSeat` on the window's registry record next to
  the owning-monitor memory: monitor index, workspace id, column, row,
  whether it had the column to itself, and the column width
  (`FlowWM::capture_restore_seat`). Process-local (not serialised), dropped
  with the record when the window is destroyed or purged, and used up by the
  next re-add of any kind.
- **Where it goes** (`plan_restore_seat`, pure). The home monitor is
  resolved as before (owning monitor, then handle, then rect). A seat on
  another monitor says nothing about this strip: appended. Otherwise the
  column index is clamped to the current column count (columns closed while
  minimized move it towards the end, never past it). It rejoins its old
  column at its row when it shared one and that column still has room
  (`ScrollingSpace::restore_window_at`, the fullscreen restore primitive),
  else it becomes a column of its own at its old width. A seat on another
  workspace of the same monitor (switched away, or the workspace is gone)
  lands in the active workspace at the clamped index as a column of its own.
- **The view follows.** In every restore case (seat, clamp, append) the
  strip's focus cursor moves to the window and the view scrolls so it is on
  screen (`ensure_focused_visible`, the same call a focus move uses: strip
  and book layout alike; in book layout it is in the open run, not a pile).
- **Focus.** When Windows made the restored window the foreground, it
  becomes the registry focus, its monitor the active one, and the previous
  focus's border is refreshed. The foreground event of a taskbar restore
  can arrive while the window is still minimized (not seated, so the focus
  handler finds no column to scroll to), so the restore hook does this itself. A pending focus hand-off (the 500 ms guard armed
  when the tile was minimized) is dropped: it was protecting a focus the
  user has now moved on purpose.
- **Strategy.** `[minimize_restore] strategy`: `original_slot` (default)
  as above, `right_of_focused` inserts after the strip's focused column,
  `append_right` appends; all three follow the view. A window recovered
  from maximized is still appended without a scroll. (Stage 33 also
  appended a window re-shown after a hide; stage 33b changed that, see
  *Hidden windows* below.)
- **Floats** keep their stage 30 behaviour (rect restored).

### Consequences

- Minimize and restore round-trips keep the arrangement, also several at a
  time in any order (each seat is per window; indices shift with the strip
  in between, which matches what the user sees).
- `flow query all` reports the restored window at its column (`state` slot).
- The monitor index in a seat is compared with the owning monitor; after a
  display change re-numbers the monitors the seat may not match and the
  window is appended (the hotplug reconcile seats its own casualties).

## Part 2: corners of a frameless window

### Context

Stage 31b made the border ring square when the window has neither
`WS_CAPTION` nor `WS_THICKFRAME`. On the host, Alacritty with
`window.decorations = "none"` still got a rounded ring around a square
window (`flow query all`: `border_corner: default`).

### Measurement (2026-11-06)

Read on the host (styles and rects only, no test windows there) and in the
VM (pixels; daemon stopped, magenta backdrop, `guest/probe33-corners.ps1`):

| Window | Style bits | Client = window rect | DWM corners |
|--------|-----------|----------------------|-------------|
| Alacritty 0.17 `decorations = "None"` (host and VM) | caption + thickframe (`0x16cf0000`) | yes (908x1044 = 908x1044) | square on the host (user report) |
| Alacritty `decorations = "Full"` (VM) | caption + thickframe | no | rounded on hardware |
| Firefox, own title bar (host) | caption + thickframe | no (invisible resize borders kept) | rounded |
| WinForms `FormBorderStyle = None` (VM) | neither | yes | square |
| WinForms WS_THICKFRAME only / WS_CAPTION only / both (VM) | as named | no | rounded on hardware |
| WinForms WS_THICKFRAME (+ caption) answering `WM_NCCALCSIZE` with 0 (VM) | as named | yes | square (like Alacritty) |

The VM cannot show DWM's rounding at all: without a GPU (Hyper-V Video +
the virtual display driver) Windows 11 draws every window square, so the VM
pixels confirm the style and rect facts, not the rounding. The rounding
column is from the host and from Windows' documented behaviour (rounded
corners belong to the non-client frame). The briefed rule ("no
`WS_CAPTION` and (no `WS_THICKFRAME` or client covers)") would not have
matched: winit keeps `WS_CAPTION` for undecorated windows.

### Decision

- **The facts.** `CornerFacts { chrome, client_covers }`, read by
  `registry::win32::corner_facts` beside `chrome_bits`: the chrome bits and
  whether `GetClientRect`'s size equals `GetWindowRect`'s size within 1 px.
  The client area always lies inside the window rect, so equal sizes mean
  equal rects; sizes alone stay stable while the window is being moved.
- **The rule** (`CornerPreference::resolve`, pure): an explicit DWM
  preference wins; otherwise `Square` when the chrome bits are 0 **or** the
  client covers the window; else `Default` (rounded).
- **Cache.** The border notes the facts it resolved from; a refresh and the
  250 ms focused recheck re-resolve when they differ (as for the chrome bits
  before), so a live decorations toggle is followed.

### Consequences

- Alacritty and other windows that draw their own frame over the whole
  window get a square ring; normal windows and apps with a custom title bar
  that keep their invisible resize borders (Firefox) stay rounded.
- Limit: a window that covers its client and then extends DWM's frame back
  in (`DwmExtendFrameIntoClientArea`) may be rounded by DWM, and its ring
  would be square; there is no API to read those margins. None seen yet. An
  app that sets `DWMWA_WINDOW_CORNER_PREFERENCE` is followed.
- There is still no config key for the corner shape; it is detected.

## Addendum (stage 33b, 2026-11-06): hidden windows

### Context

An app that hides to the tray and is reopened (Discord, Teams, Steam,
Spotify, Electron apps) came back at the far right of the strip with no
scroll: the hide hook took the tile out of the strip without recording a
seat, and the show hook appended it. The registry's `last_virtual_slot`,
saved on hide as well, never placed anything.

### Decision

- The hide hook (`on_window_hidden`, also the DWM-cloak hook) records the
  restore seat of an active tile before removing it, exactly as the
  minimize hook does: same `RestoreSeat`, same lifetime (dropped with the
  record on destroy or purge, used up by the next re-add).
- The show hook (`on_window_shown`, also uncloak) re-adds the tile through
  `readd_tile(hwnd, follow = true)`, the restore path: seat applied
  (clamp, shared-column rejoin, workspace fallback), view scrolled to it,
  focus taken when Windows made it the foreground. `[minimize_restore]
  strategy` applies to both. The view follows any re-add on this path,
  with or without a seat (as for a restore).
- The minimize hook now records a seat only from an active tile, so a
  minimize that reaches a tile already hidden (or a repeated minimize)
  keeps the seat recorded first instead of clearing it.
- A maximize recovery is unchanged: appended, no scroll.

Rejected: keep re-shows as they were because "an app re-showing itself is
not the user's request". In practice the re-show is the answer to a tray
click; the user looks for the window where it was.

### Consequences

- A tile cloaked and uncloaked by DWM (a suspended UWP app, a Windows
  virtual-desktop switch) also returns to its seat; when several come back
  at once the view ends on the last one shown.
- `last_virtual_slot` stays: it is the placeholder slot of the registry's
  `Active` state on restore/show (re-synced from the layout once seated)
  and the hotplug reconcile's casualty order. It is not where a tile goes
  back.
- Unit tests: hide then show back in its column and in view, clamp after
  columns closed, destroy while hidden leaves no seat, minimize of a
  hidden tile keeps the hide's seat. VM driver `test-stage33b.ps1`, H1-H7.

## Tests

Unit tests: `plan_restore_seat` (no seat, other monitor, old column, stacked
row, clamp past the end, empty strip, other or gone workspace); the restore
hook end to end on test strips (middle tile back in its column and focused,
two restored in reverse order, clamp, stacked rejoin, gone workspace, view
follows out of view, book layout open run, strategies); `resolve` with the
new facts; `client_covers_window`. VM driver `test-stage33.ps1`
(`guest/stage33-lib.ps1`, `guest/testwin33.ps1`): R1-R7 and C1-C5.
