# Windows that cannot be tiles: overlays, undecorated popups, refused rects

**Status:** Accepted (stage 42, 2026-11-17). Adds a style gate beside the
band gate of ADR-0033 (`Ignored::Overlay`), a shape rule inside the
classification pipeline (learned rules can no longer tile an undecorated
popup; `set-window` learns nothing from one), and a placement acceptance
check after every layout pass that releases a tile which keeps its own size.

## Context

**Measured on the host (Windows 11 build 26300, 2026-11-16)** with a
read-only probe (`probe-teams-share.ps1` in the VM harness: styles, band,
rect and `flow query all` per window of `ms-teams.exe`) while a Teams screen
share was running, once with the sharing control bar collapsed and once
expanded and moved:

| window | class | rect | band | style | ex-style | flowd (stage 41) |
|---|---|---|---|---|---|---|
| share border | `ScreenBorderWindow` | 1920x1080 at 0,0 | 1 | `WS_POPUP`, `WS_CAPTION` | `TOOLWINDOW`, `LAYERED`, `TRANSPARENT`, `NOACTIVATE`, `TOPMOST` | not registered (only because of `WS_EX_TOOLWINDOW`, the Alt+Tab gate) |
| sharing control bar, collapsed | `TeamsWebView` | 297x35 | 1 | `WS_POPUP` (no caption, no sizing border) | `TOPMOST`, `NOREDIRECTIONBITMAP` | **tiled** |
| sharing control bar, expanded | `TeamsWebView` | 790x55 (a new hwnd) | 1 | the same | the same | **tiled** |
| main window | `TeamsWebView` | 946x1051 | 1 | `WS_CAPTION`, `WS_THICKFRAME` | none of the above | tiled (wanted) |

The bar was tiled by a machine-written rule in `history-flow-rules.toml`,
`exe = "ms-teams.exe"`, `class = "TeamsWebView"`, `action = "tile"`, which
`set-window` had learned from the **main** window: the bar shares its class.
The daemon gave the bar a column and sized it to the slot; the bar kept its
297x35 and sat at the column's top, the rest of the column blank: a phantom
column. Re-created on expand, the new hwnd got a column again.

flowd read neither `WS_EX_TRANSPARENT` nor `WS_EX_NOACTIVATE` anywhere; the
border was left alone by accident of a third style.

**Reproduced in the VM** with stand-ins (`guest\testwin42.ps1`: one plain
Win32 window per process, exe `tw42.exe`) on the stage 41 build
(`out\s42-old.txt`): the border without `WS_EX_TOOLWINDOW` became a float
and was moved (0,0 1920x1080 to the work area, 1924x1034 at -2,0); the bar
with a learned tile rule took a column and refused the slot (27 size
requests over the run, the column blank); the re-created bar took the same
column; a caption + sizing-border window that reverts every resize stayed a
tile at 500x300 in a 946-wide slot.

## Considered Options

- **The border.**
  - **(a) A style gate: `WS_EX_TRANSPARENT` or `WS_EX_NOACTIVATE` makes a
    top-level window `Ignored::Overlay`, after the elevation and band gates
    and before every rule.** Chosen. Both are documented: a transparent
    window does not receive mouse input (it passes to the windows under
    it), a no-activate window never becomes the foreground window. Either
    one means the user can neither focus nor arrange the window, so a tile
    or a float of it is meaningless, whatever its other styles.
  - (b) Keep relying on `WS_EX_TOOLWINDOW`. Rejected: an accident; an
    overlay without it (the stand-in `TW-OV2`) was managed and moved.
  - (c) A class rule for `ScreenBorderWindow`. Rejected: one app's class;
    every share tool has its own.
- **The bar.**
  - **(a) "Untileable by shape": `WS_POPUP`, no `WS_THICKFRAME`, and not a
    full `WS_CAPTION`, floats unless the user's `flow-rules.toml` tiles
    it.** Chosen; the predicate is below.
  - (b) A rule for `ms-teams.exe` / `TeamsWebView`. Rejected: per app, and
    the main window shares the class, so a class rule cannot tell them
    apart; the styles can.
  - (c) Delete or rewrite the learned rule. Rejected: the rule is right for
    the main window; what is wrong is applying it to a window of another
    shape.
- **A tile that does not take its slot.**
  - **(a) Read the rect back once after the batch settles; a window that
    still has exactly the size flow met it at, and is smaller than its
    slot, is released to the floating layer once.** Chosen.
  - (b) Retry the resize. Rejected: the window answers every resize with
    its own size (measured: the stage 41 build sent the bar 27 resizes);
    retrying is a fight with no end.
  - (c) Leave it a tile, centred in its slot. Rejected: the column stays
    blank around it, the phantom the user reported.
  - (d) Make the border a tool window or otherwise change other apps'
    windows. Rejected: flowd does not restyle foreign windows.

## Decision

### 1. Overlays are never managed

`registry::classification::is_overlay(ex_style)`:
`ex_style & (WS_EX_TRANSPARENT | WS_EX_NOACTIVATE) != 0`. `get_window_info`
reads `GWL_STYLE` and `GWL_EXSTYLE` once at arrival
(`registry::win32::window_styles`); `register_window_from_info` tests it
right after the band gate (elevated, shell band, **overlay**, session lock,
then the rules). An unreadable style reads as 0, so a read failure leaves
the window to the rules (fail-open). Like the band it is decided once at
arrival.

- State `Ignored(Overlay)`, `flow query all` state `Ignored::Overlay`.
- Log, info, once per window: `window 0x.. (exe "class") is a
  click-through or no-activate overlay (ex-style 0x..); left unmanaged`.
- Not a floating-layer member (only `Ignored(ExplicitRule)` is), never
  moved, raised, bordered or learned; `set-window` refuses it: `window 0x..
  (exe) is a click-through or no-activate overlay; flow leaves it alone`.
- A tool-window overlay (the real Teams border) still never reaches this
  gate: the Alt+Tab gate (`creation_gates`) skips it first, so it is not in
  `flow query all` at all. The new gate covers the overlays that gate lets
  through.

Documented: the meaning of both styles (`CreateWindowEx` extended styles).
Inferred: that no window carrying either one is one a user wants arranged.
Known consequence: the On-Screen Keyboard and similar never-activating
tools are no longer managed (they could not be focused as tiles anyway).

### 2. Undecorated popups without a sizing border float

`registry::classification::is_untileable_shape(style)`:
`style & WS_POPUP != 0 && style & WS_THICKFRAME == 0 && style & WS_CAPTION
!= WS_CAPTION`. A popup with neither a title bar (`WS_CAPTION` is
`WS_BORDER | WS_DLGFRAME`; a thin border or a dialog frame alone is no title
bar) nor a sizing border: the app chose its size and gives the user no way
to change it. `WS_POPUP` is required so a borderless main window built as
an overlapped window (winit's undecorated windows keep `WS_CAPTION |
WS_THICKFRAME`, ADR-0027; WinForms `FormBorderStyle = None`) is not caught.
Unreadable style: 0, not untileable.

In the pipeline (`classification::shaped_verdict`): when such a window's
first matching layer says `Tile` and that layer is **not** the user's
`flow-rules.toml` (a learned rule, a bundled rule, or `default_action`), the
action becomes `Float`. `Ignore` and `Float` verdicts are untouched. The
shape is stored on the window (`Window::untileable_shape`), so the
reclassification after a maximize uses it too.

- Log, info, once per window, when the shape overruled a tile:
  `window 0x.. (exe "class") is an undecorated popup without a sizing
  border (style 0x..); it floats (a learned rule that tiles it does not
  apply)` (or `a bundled rule`, `default_action`).
- A user rule that tiles it wins; flowd logs a warning **once per rule**
  per load of `flow-rules.toml`: `flow-rules.toml rule N (exe = "...",
  class = "...") tiles window 0x.. (exe "class"), an undecorated popup
  without a sizing border; it is tiled because the rule says so`. The
  placement check (3) still applies to it.
- A new float of this shape stays where its app put it (its own rect, when
  that lies on a screen) instead of being centred on the focused screen
  (ADR-0023): the app positions such a popup on purpose (the bar sits at
  the top centre of the shared screen).
- **Learned rules are no longer written for it.** `set-window` on such a
  window still works, but records nothing (`set-window: exe (class) is an
  undecorated popup; the decision is not learned`, info). A learned rule is
  keyed on exe + class, and the window's main-window sibling may share the
  class: a `float` learned from the bar would float the main window. The
  history file is not rewritten; an existing rule keeps working for the
  windows of the class that have another shape.
- `flow query all`: `untileable: "undecorated popup"`.

Documented: the style bits. Inferred: that such a popup is not meant to be
resized by anyone but its app.

### 3. The placement acceptance check

`daemon::placement_check`. Every submit path records the windows it
commanded (`record_submitted`); `SETTLE_MS` = 500 ms after the last submit,
with the animator idle, each of them that is still an active tile (not a
fullscreen column, not in its app's own full screen, not being dragged,
visible, not minimized, tiling on, session unlocked) has its window rect
read back once. Its slot's window rect is the one the animator was sent
(`tile_geometry` of the last submitted rect).

`refused_its_rect(own, actual, expected)`: the window **kept its own
size** (`actual` within `KEEP_TOLERANCE_PX` = 2 px of the size it had when
flow met it, `Window::last_natural_size`) **and is smaller than its slot
by more than `REFUSAL_MIN_PX` = 32 px in width or height**.

- The minimum-size distinction: a window clamped by its
  `WM_GETMINMAXINFO` minimum is never smaller than asked in either
  dimension, so the second condition is false; and its size changed
  (the other dimension followed the slot), so the first is usually false
  too. It stays a tile, wider than its column (stand-in `TW-MIN`: 1200 px
  minimum in a 936 px slot, not released). A window met at its minimum size
  that is larger than its slot both ways is not released either.
- The 32 px margin keeps a window that nearly fits (a terminal snapping to
  character cells, an app rounding its size) a tile; 2 px absorbs
  rounding in the size read.
- Slow is not refusing: the check runs only once the animator is idle (its
  `SetWindowPos` calls are synchronous, so each frame has been applied
  unless the window changed it), and a candidate is first asked
  `IsHungAppWindow` and a 50 ms `WM_NULL` (ADR-0035): one that does not
  answer is not judged (`placement check: window 0x.. (exe) does not
  answer; not judged`, debug; the stage 41 gate parks it). After the answer
  the rect is read once more.
- Release: the window leaves its strip (the neighbours close the gap, in
  one batch with the float), becomes a float at the size it insists on, at
  its current rect when that lies inside its screen's work area (else
  centred there), enters the floating layer and gets a float border.
  `Window::refused_rect` is set; `flow query all`: `untileable: "refused its
  rect"`. Log, info: `window 0x.. (exe "class") kept its own size WxH for
  a WxH slot; released to the floating layer (refused its rect)`.
- Exactly once: a float is not checked, so nothing is retried. A
  `set-window tile` on it clears the flag and the next placement judges it
  afresh (and releases it again if it still refuses). No rule is written.

Documented: `SetWindowPos` sends `WM_WINDOWPOSCHANGING`, where the window
may change the size; `WM_GETMINMAXINFO` bounds the tracking size.
Measured: the Teams bar keeps its size; the stand-ins `TW-BARU` and
`TW-REV` keep theirs through `WM_WINDOWPOSCHANGING`. Inferred: that a
window still at exactly its own size after a synchronous resize to a
larger slot refused it.

## Consequences

- During a Teams share the border is never touched, the bar floats where
  Teams puts it (also after each re-creation) and the main window keeps
  tiling through its learned rule.
- A window that refuses its slot is a tile for one settle period (500 ms
  plus the animation) before it floats; the strip reflows once.
- The `untileable` query field and four log lines say why a window is not
  in the strip.
- Residual: a window that refuses its slot but resizes to some third size
  (neither its own nor the slot's) stays a tile; one whose style changes
  after arrival keeps the shape it arrived with.

## Tests

Unit tests: `registry/classification.rs`
(`overlay_and_untileable_shape_predicates`,
`shaped_verdict_floats_untileable_windows_unless_a_user_rule_tiles_them`),
`registry/core.rs` (`overlays_are_ignored_and_untileable_popups_float`),
`daemon/placement_check.rs` (`a_window_that_kept_its_own_size_in_a_larger_slot_refused_it`,
`a_window_that_took_its_slot_or_nearly_did_not_refuse_it`,
`a_window_clamped_to_its_minimum_size_did_not_refuse_its_slot`),
`daemon/dispatch.rs` (`set_window_on_an_overlay_is_refused`).
VM: `test-stage42.ps1` with `guest\stage42-lib.ps1` and
`guest\testwin42.ps1` in the VM harness (A / A2 overlays, B the bar, C a
learned rule, E the resizable popup of the same class, H the re-created
bar, D user rules, F the reverting window, G the minimum size, O the
overlays at the end).
`test-stage36.ps1` cases L2, L4a, L4b and L5b changed expectation: their
band-1 lock stand-ins are `WS_POPUP` windows without caption or sizing
border, so under `default_action = "tile"` they now float and `set-window`
learns nothing from them (the real lock surfaces are band 17, ADR-0034).
