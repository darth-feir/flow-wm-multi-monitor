# New windows open on the focused screen

**Status:** Accepted (stage 30, 2026-11-02). Changes where the creation
pipeline seats a window; the strip, its widths and every other seating path
are unchanged. Stage 30d (2026-11-03) adds that a window flow has never
placed is placed instantly, on any path (*Appearing in place*).

## Context

Windows decides where a new top-level window appears: on the monitor of the
launching process's last window, on the primary monitor, or wherever the app
remembers. Until stage 30 the daemon seated a freshly classified tile on that
monitor (`seat_created_tile` used `home_monitor_for_hwnd`, which for a window
without owning-monitor memory is `MonitorFromWindow`, then the rect). Working
on the left screen, Mod+T (Alacritty) opened on the right one every time,
because Windows put it on the primary. niri opens a new window on the
focused output, next to the focused column; the fork follows niri where the
platform allows.

## Decision

- **A new window is seated on the active (focused) monitor**: a tile in that
  strip after its focused column (`ScrollingSpace::insert_window`), a float
  centred on that monitor's work area (`centered_float_rect_on`). The window
  is moved there by the normal animation of the seat, the same layout path a
  `move-to-monitor` takes, so the target strip's width (its `[columns]`
  preset or pattern) applies and the move animates across the screens.
- **What counts as new.** The creation pipeline now knows how a window
  arrived (`hooks::Arrival`). `New`: `EVENT_OBJECT_CREATE`, the pending
  creation retry, `SHOW`, `UNCLOAKED` and `NAMECHANGE` registrations, and a
  maximized window adopted at creation. `Returning`, which keeps the screen
  the window is on: an untracked window first met by the restore hook (it was
  on the desktop, minimized), one first met when it takes the foreground (the
  user clicked it where it is) and one met by `set-window` (it is the
  foreground). Windows found by the startup scan never go through this path,
  and tracked windows coming back from minimize, the tray or a maximized
  state are re-seated on their owning monitor as before
  (`readd_tile_to_home_monitor`).
- **Config switch.** `[focus] new_window_monitor = "focused" | "placed"`,
  default `"focused"`. `"placed"` restores the old behaviour exactly.

## Pre-placement before the first paint (stage 30b)

**Problem.** The seat above runs when the window is classified, and a
window is classified only once it is visible with a title. By then it has
been shown and painted where Windows put it, so a new window on the other
screen flashes there before it jumps to the focused one (Alacritty from
Mod+T on the host). The cross-screen move itself is instant; the flash is
the first paint.

**Decision.** `EVENT_OBJECT_CREATE` arrives before the app shows its
window. When a `Created` event fails classification, `daemon::pre_place`
moves the window onto the focused screen right away, before its first show,
if all of these hold:

- `[focus] new_window_monitor = "focused"`, tiling is on, there is more than
  one monitor, and the window was not pre-placed before;
- it is not visible yet, is a root window (`GetAncestor(GA_ROOT)`), has no
  owner, is not `WS_CHILD`, not `WS_EX_TOOLWINDOW` (even with
  `WS_EX_APPWINDOW`), has `WS_CAPTION` or `WS_THICKFRAME`, and is neither
  minimized nor maximized;
- the monitor it is on (as `home_monitor_for_hwnd` sees it) is not the
  active one, and its rect is not empty;
- no rule layer (user, learned, bundled, fallback) would `ignore` it as
  matched without a title (`WindowRegistry::would_ignore_untitled`; rules
  on the title cannot match yet, so they do not count);
- its thread answers a `WM_NULL` within 50 ms (`SendMessageTimeoutW`,
  `SMTO_ABORTIFHUNG`).

The move keeps the size and the window's offset from its work area's
origin, shifted inside the focused screen's work area (at its left / top
edge when the window is larger), with `SWP_NOSIZE | SWP_NOZORDER |
SWP_NOACTIVATE` and no show flag. When the window is classified later, it
already lies on the focused monitor and the seat has no cross-screen move to
make. The set `FlowWM::pre_placed` makes it once per window; an entry leaves
when the window is classified, destroyed or the creation retries give up.
Only the `Created` event pre-places: the startup scan, `tiling on` and the
Returning arrivals never do.

**Why synchronous, why the probe.** A `SetWindowPos` on another thread's
window waits until that thread handles the messages it sends. An
asynchronous move (`SWP_ASYNCWINDOWPOS`) would not wait, but it is carried
out whenever the app next pumps messages, which can be after the seat's own
move, and would then put the tile back at the pre-placed spot. So the move is
synchronous, like every other move flow makes, and the `WM_NULL` probe keeps
it to threads that are pumping right now. A window still being set up by a
busy thread is left alone; its seat moves it as before.

**Limits.**

- An app that is not pumping messages between creating and showing its
  window (it fails the probe), creates its window already visible
  (`WS_VISIBLE` in `CreateWindowEx`: WinForms' `Application.Run(form)` does
  this), or places itself after our move (apps that restore their own
  window placement) still flashes; the seat move then works as in stage 30.
- Hidden captioned windows that are never shown (an AutoHotkey script's main
  window, a hidden console's `conhost` window) are moved too. They are
  invisible, so nothing changes on screen; if one is shown later it opens on
  the screen that had the focus when it was created.
- `pre_manage_rect`, the rect a window goes home to on `flow stop` and
  `tiling off`, is read when the window is classified, so for a pre-placed
  window it is the rect on the focused screen, not where Windows first put
  it. That is accepted: it is where the user saw the window open.
- A pre-placed window classified as a float is centred on the focused screen
  as before (`centered_float_rect_on`); nothing extra happens to it.

## Spawning on the focused screen (stage 30c)

**Problem.** Pre-placement did not help the case that started it. On the
host, Mod+T starts Alacritty (winit), which creates its window hidden, then
sets up its renderer without pumping messages, then shows the window. Its
thread fails the `WM_NULL` probe for the whole gap, so the window is shown
and painted on the primary screen and only then seated on the focused one.
Two ways to hide that first frame from outside the app were probed in the VM
and fail:

- `DwmSetWindowAttribute(DWMWA_CLOAK)` on another process' window returns
  `E_ACCESSDENIED`: only the owning process may cloak its windows.
- Making the window layered and fully transparent (`WS_EX_LAYERED` with
  alpha 0) needs `SetLayeredWindowAttributes`, which, like a synchronous
  move, waits for the app's thread.

**Decision.** For programs flow itself starts, the parent chooses where the
first window goes. `CreateProcessW` takes `STARTUPINFOW.dwFlags |=
STARTF_USEPOSITION` with `dwX` / `dwY`; Windows uses that point, in place of
its own cascade, for the first overlapped window the new process creates
with `CW_USEDEFAULT` as its position. winit passes `CW_USEDEFAULT` when no
position is configured, so does WinForms' `WindowsDefaultLocation`, and so
do most apps that do not remember a placement. The window is then created
on the focused screen; there is nothing to move and no flash.

- The daemon gets a `spawn` IPC command (`flow spawn [--] <program>
  [args...]`, `daemon::spawn`). It resolves the program like a `[startup]
  spawn` entry (`config::startup::resolve_program`), starts an `.exe` with
  `CreateProcessW` from `%USERPROFILE%` with `CREATE_NEW_CONSOLE` (the
  daemon's own console is hidden; a console program needs a visible one, as
  under the keymap's `Run`), closes both handles and logs `spawn: started
  <path> (pid N) at (x,y) on monitor M`. Anything that is not an `.exe` (a
  `.cmd`, an extensionless file on `PATH`) is started as before through
  `std::process::Command`, without a position.
- The point is on the active monitor: right of the focused tile's rect plus
  the gap (the slot a new column gets, after the focused column) when a
  column of the strip's base width fits there, else the work area's top-left
  corner plus the gap (`spawn_origin`; since stage 30d the exact slot rect,
  `spawn_rect`, see *Appearing in place*). The daemon tiles the window
  afterwards anyway; the point only has to be on the right screen and near
  the final slot so the settle animation is short.
- No position is passed while tiling is off or with `new_window_monitor =
  "placed"`: Windows decides then, as the setting says.
- `[startup] spawn` entries use the same path, so they open on the active
  monitor too.
- The keymap's `Launch` sends every non-admin `.exe` spawn bind through
  `flow spawn`, each argument quoted for the CRT parser so the daemon gets
  them one by one. Before that it calls `AllowSetForegroundWindow` for the
  daemon (the script had the last input), and the daemon does the same for
  the child, so the new window may take the foreground as it did under
  `Run`. When `flow spawn` cannot be run or exits non-zero (no daemon, start
  failed), the script runs the program itself as before.

**The monitor hint.** `ShellExecuteEx` with `SEE_MASK_HMONITOR` passes the
monitor in `STARTUPINFO.hStdOutput` (`STARTF_USESTDHANDLES` not set) with
the undocumented flag `0x400`; Windows then cascades the first
`CW_USEDEFAULT` window on that monitor. The documented part of the
convention (the `hStdOutput` remark in the `STARTUPINFO` reference) only
says an app *may* read it. In the VM probe both Alacritty and a WinForms
window honoured `STARTF_USEPOSITION`, `hStdOutput` + `0x400` and
`ShellExecuteEx`; `hStdOutput` without `0x400` changed nothing. The daemon
uses only `STARTF_USEPOSITION`: documented, and it gives the exact point.

**Limits.**

- Admin spawns (`admin = true`, `*RunAs`) still go through the keymap's
  `Run`: an elevated start needs the UAC path, which takes no startup
  position from us. They can still paint on the other screen first.
- Apps that position their first window themselves (a remembered placement,
  an explicit position, a single-instance app that hands the request to its
  running process, a launcher stub that starts the real exe, such as the
  Store's `wt.exe` alias) ignore the hint. Pre-placement and the seat still
  apply to them.
- Programs started outside the keymap and the daemon (Start menu, taskbar,
  Explorer, another app) get Windows' placement; pre-placement and the seat
  are all flow can do for them.
- The VM cannot show Alacritty's window (no OpenGL 3.3 in Hyper-V: it
  exits with `OpenGL error requesting shader version` about 0.15 s after
  creating it hidden), so the VM proves the first window's rect and the
  daemon log; the visible result is checked on the host.

## Appearing in place (stage 30d)

**Problem.** After stage 30c Alacritty was created on the focused screen,
but at a point near its slot with its own default size, painted there
(white, before its renderer drew), and then glided into its column: the seat
tweened it from where it was created to its slot.

**Decision.** Two changes, one for the rect a spawned window is created at,
one for how every new window reaches its slot.

- **The exact rect.** `spawn_position` asks the active strip where a window
  inserted now would go: `ScrollingSpace::insert_preview_rect` runs
  `insert_window` with a placeholder on a clone of the strip (same focus,
  widths, width pattern, layout mode and camera; the strip is not touched)
  and reads the placeholder's projected rect. That is the seat's own
  computation, so it is right for book mode (the new column is focused and in
  view, never in a pile), for a strip that has to scroll to show the new
  column (a 1/2 column after the right of two visible ones) and for an
  empty strip (the first column at the left edge). `spawn_rect` insets it by
  the border ring (`thickness - overlap`), as the seat does for a tile, and
  falls back to the work area's corner at the base width when the slot is
  not on the work area. `CreateProcessW` gets `STARTF_USEPOSITION |
  STARTF_USESIZE` with that rect. The log line and the IPC reply carry the
  size: `spawn: started <path> (pid N) at (x,y) on monitor M, size WxH`.
- **No frame translation.** The seat moves a tile to its visible rect
  translated by the window's invisible borders (`InvisibleBounds`, read
  from DWM for a window that exists). A window that does not exist yet has
  none to read, and the size of those borders depends on the app's frame
  (a standard resize frame, a custom frame, none), so the hint is the
  visible rect. A window with a standard frame is created with its frame
  inside its slot by the invisible border (7 px on the left, right and
  bottom at 100 %) and the seat puts it exactly in place; its visible frame
  already matches the slot.
- **The size applies only when the app lets Windows choose it**
  (`CW_USEDEFAULT` as the width). winit passes its own size when the app
  requests one; Alacritty computes its window size from its font and
  terminal dimensions and so keeps it. For such an app the position helps
  and the size jumps to the column's at the seat.
- **Instant first placement.** A window flow has never placed moves to its
  target instantly; the other columns of the batch tween as before
  (`animation::first_placement`, applied in `push_targets_as`). "Never
  placed" is "no previous rect at all": no rect in `submitted_rects` and no
  registry rect (a tile's `tiled_rect`, which only the layout sets and
  nothing clears, or a float's rect). This covers every new window, not
  only spawned ones: a window from the Start menu or the taskbar appears in
  its slot instead of gliding from where Windows put it, and so do the
  `Returning` arrivals met for the first time (a window first seen at its
  restore or when it takes the foreground, a window registered while
  tiling was off and seated on resume). A window that was seated before
  keeps the tween when it comes back: minimize, hide and destroy drop its
  submitted rect, but its registry tiled rect stays, so a restore still
  glides into place. The fresh-float branch passes a snapshot of the
  registry rects without the new float, because `register_float` writes
  its target into the registry before the batch is built. The decision is
  logged at debug: `animate: new window 0x.. placed instantly at its slot
  (x,y,w,h)`.

**What still shows.** The app's own first frame: Windows shows the window
where it was created (for a spawned program with a sizable default, its
slot), and what it paints before its first real frame (a white or black
background while a GPU renderer starts) is the app's. For an app that picks
its own size (Alacritty) that first frame has the app's size at the slot's
top-left until the seat resizes it, without a glide. A window that is not
spawned by flow still has its first frame where Windows put it (on the
focused screen when pre-placement moved it); the seat then jumps it into the
slot.

## Consequences

- A window an app opens on its own while the user works on the other screen
  (a second window of a background app, a mail compose window opened from a
  toast) also opens on the focused screen. That is niri's behaviour and the
  point of the change; users who want Windows' placement set `"placed"`.
- The window appears where Windows put it for one frame and is then placed
  in its seat instantly (stage 30d; before, it tweened there). Since stage
  30b most windows are moved before their first show (see *Pre-placement*
  above), so there is no frame on the other screen; a program flow spawns
  is created at its slot (stage 30d).
- Dialogs and other windows the rules ignore are not seated at all and stay
  where their owner put them.
- Tests: `daemon::hooks` (`monitor_for_arrival` for both values and both
  arrivals), `config::types` (default, parsing, refusal of other values);
  VM driver `flowwm-vm/test-stage30.ps1` cases N1-N4. Pre-placement:
  `daemon::pre_place` (candidate filter, rect translation and clamp),
  `registry::core` (`would_ignore_untitled`); VM cases P1-P5. Spawning:
  `daemon::spawn` (the `.exe` split, an unknown program),
  `ipc::message` (argv round trip), the `flow spawn` parser; VM cases S0-S4.
  Appearing in place: `workspace::scrolling_space` (`insert_preview_rect`
  for an empty strip, a strip, a 1/2 strip that scrolls, book mode),
  `daemon::spawn` (`spawn_rect`), `daemon::animation` (the first-placement
  rule, the seat of a new tile against a seated one and a restore); VM cases
  R0-R4.
