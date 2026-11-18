# Riders follow their anchor

**Status:** Accepted (stage 44, 2026-11-17). Amends
[ADR-0024](0024-owned-windows-ride-the-floating-layer.md): supersedes its
"riders are never moved by the daemon" and its whole-layer z-order for
riders. New module `daemon::rider_follow`; `daemon::top_layer` gains the
rider seat rule.

## Context

On the host (2026-11-17, read-only probe of SSMS 21's windows) the connect
dialog is a WPF window (`HwndWrapper[DefaultDomain;;<guid>]`) **owned by the
tiled SSMS main window**, `WS_CAPTION | WS_THICKFRAME | WS_EX_TOPMOST`,
520x720, in the normal desktop band. ADR-0024 made it a rider of the main
window, which fixed the lock-out of 2026-11-02, but the user saw two
defects:

- **It does not move with its window.** A strip scroll, `move-to-monitor`
  or a workspace switch moves the main window; the dialog stays where it
  was, over whatever the strip brings there, or alone on a screen its
  window left. ADR-0024 decided "riders are never moved by the daemon".
- **Its z-order follows the layer, not its window.** While the layer is
  raised (any flow window has the foreground) the dialog is held topmost,
  over everything that covers its own window. And because the app made it
  topmost itself, ADR-0024's leave step ("put its own flag back") left it
  topmost after a drop too: over foreign apps.

Windows' own rules for owned windows (documented: *Window Features*, owned
windows): an owned window is always above its owner in the z-order, is
hidden when its owner is minimized, and is destroyed with its owner.
Windows does **not** move an owned window when its owner moves.

## Considered Options

- **(a) Move riders by the anchor's delta wherever the daemon moves the
  anchor; seat the rider by its owner group's foreground.** Chosen.
- (b) Read the anchor's position back after every batch settles and move
  the riders then. Rejected: the dialog jumps after the slide, and a user's
  own move of the anchor (a float dragged natively) would be followed too.
- (c) Make riders registry members (`Ignored(ExplicitRule)` floats).
  Rejected for the reasons in ADR-0024 (classification, borders, rules for
  every dialog).
- (d) Keep riders topmost with the layer and only move them. Rejected: the
  second defect is the z-order itself.
- (e) Per-app handling for SSMS / WPF. Rejected: the owner chain is the
  signal; nothing here names an app.

## Decision

### 1. Position follows the anchor (`daemon::rider_follow`)

Every daemon path that moves a tracked window either submits an animator
batch or teleports it:

- **A batch** (`animate_layouts_on`, `submit_animation`,
  `animate_workspaces_on`, `seat_all_monitors_instantly`, the `tiling off`
  batch) goes through `FlowWM::animate_with_riders`: for each rider whose
  anchor has a target, a target for the rider is added to the same batch:
  its current position plus the anchor's delta (window-rect space), its
  own size, the anchor's `instant` flag. The rider slides with the anchor
  frame by frame. A batch that retargets one still in flight starts from
  the pair the last submit commanded while the anchor has not reached its
  target (`follow_base`), so the offset does not drift.
- **A teleport** (`teleport_workspaces`, `teleport_preview`) moves the
  riders right after the anchor (`follow_anchor_now`).
- **A drag** of the anchor (a keymap session or a native move-size of a
  tile or float) records each rider's offset at the start and keeps it on
  every `LOCATIONCHANGE` of the dragged window and at the drop; the drop's
  layout submit then carries them on (`riders_drag_begin / move / end`).
- **The shutdown rescue** moves each rider with its rescued anchor
  (`riders_snapshot` before the layer's release, `follow_rescued_anchors`
  after the passes), so no dialog stays parked off screen with the daemon
  gone.

`follow_verdict` (pure, unit-tested) decides per rider: the anchor does not
move → nothing; the rider is not shown (hidden with a minimized anchor) →
left alone; the anchor or the rider is hung (`IsHungAppWindow`, ADR-0035)
→ left where it is (a synchronous `SetWindowPos` would wait on that
thread); otherwise moved by the delta. An anchor parked as hung (ADR-0035)
leaves the layout, so nothing moves its riders until it answers again and
is re-seated, when they follow.

A workspace switch parks windows by moving them (`WorkspaceParking`), not by
hiding them, so the riders of a parked anchor are parked with it by the
same mechanism. Riders are noted with the layer off too (an untracked
foreground whose owner chain reaches a tracked window), so the following
does not depend on `[floating] always_on_top`.

### 2. Visibility follows Windows

A minimized anchor: Windows hides its owned windows and shows them again on
restore (measured below). flowd does not force anything; a rider hidden
while its anchor is minimized — by `IsIconic`, or still by the registry,
because on a restore the owner's foreground can be handled before Windows
shows the owned windows — or parked as not responding (Windows' ghost hides
the hung windows) is kept in the table (`forget_rider` and the purge skip
it), so it keeps following once its anchor is back.

### 3. Z-order follows the owner group (`top_layer::rider_seat`, `rider_step`)

| Layer on, anchor tracked, rider visible? | Raised, and the owner group holds the layer's last non-shell foreground? | Seat |
|---|---|---|
| no | — | `Released`: its own flag back, if the layer held or seated it |
| yes | yes (the anchor, the rider, or a window whose owner chain reaches the anchor) | `Topmost`: `HWND_TOPMOST`, entering after the members on a raise, the foreground last; re-asserted when demoted |
| yes | no (another tile or float, a foreign window, a dropped layer) | `AboveOwner`: seated directly above its owner |

The step (`rider_step`, pure): `Topmost` → enter or re-assert; `AboveOwner`
→ seat when the layer held it, or once when it sits in the topmost band
while its owner does not (an app's own flag, the SSMS case), never again
(an app that sets the flag again is not fought); `Released` → the own flag
back. The seat (`seat_plan`, pure) reads the window directly above the
owner (`GW_HWNDPREV`): when that window is in the owner's band the rider is
inserted under it (one `SetWindowPos`, which also clears the rider's
topmost flag when the band is the normal one); when the owner tops its band
(or is at the top of the z-order) `HWND_NOTOPMOST` / `HWND_TOPMOST` puts the
rider at the top of that band, i.e. directly above the owner; when the
rider is already there nothing is sent; a window is never seated under its
own popup. Riders are processed owner first, so a message box owned by a
rider ends above it, and siblings (two dialogs of one owner) keep their
order: the ones to seat top-down (each lands directly above the owner,
under the one before), the ones to raise bottom-up (each raise lands on
top), the foreground last (`rider_sync_order`, which reads the z-order only
when two or more riders exist). Without it, the stage 31 case "connect
dialog and error box both owned by the main window" put the error box under
the connect dialog on the first seat (measured; stage 31 R2).

The foreground the group test uses is the layer's own: a shell surface
(Start, the taskbar) keeps the last one, as it keeps the raise / drop
decision. Every focus change whose decision stands syncs the riders
(`evaluate_top_layer`), a flip applies to every window (members first, then
riders, owner first, the foreground last), and the periodic pass syncs them
after the members.

ADR-0024's protection is unchanged: while the group holds the foreground,
no float raised by the layer covers the rider.

A rider seated over its own topmost flag keeps `was_topmost` across the
next entry, and `flow stop` / fail-back put that flag back.

### 4. Log lines

Info (a change the user sees): `top layer: rider 0x.. (anchor 0x..) entered
(was_topmost=..)` (the first entry), `... left (restored to ..)`, `...
forgotten (hidden|destroyed)`, `shutdown rescue: rider 0x.. followed its
anchor 0x.. to (x,y)`.

Debug: `top layer: rider 0x.. noted (anchor 0x..)`, `... raised again (its
owner group holds the foreground)`, `... seated above its owner 0x.. (under
0x..|top of the normal band|top of the topmost band|already there)`,
`could not seat rider ...`, `rider 0x.. hidden with its minimized or hung
anchor 0x..; kept`; `riders: 0x.. follows anchor 0x.. by (dx,dy) to (x,y)[
instantly]`, `... (teleport)`, `riders: 0x.. (anchor 0x..) not moved: the
anchor|the rider is not responding`, `... is not shown; not moved`,
`riders: 0x.. rides the drag of 0x.. (offset ..)`, `riders: the drag of
0x.. ended`, `riders: 0x.. stays where it is: its anchor 0x.. is not
responding`.

## Measured (VM)

Suite `test-stage44.ps1` (stand-in `guest	estwin44.ps1`: a WinForms
window tiled on DISPLAY1 with an owned modeless window, `Show(owner)`, and
an owned modal dialog, `ShowDialog(owner)`, `WS_EX_TOPMOST` like the SSMS
dialog, both `WS_CAPTION | WS_THICKFRAME`; layer on, strip of 1/2 columns, a
float over the dialog, a tool window flowd does not track as the foreign
app). Rects and z-order read by a guest probe (`EnumWindows` order of the
visible windows, `WS_EX_TOPMOST`, `GetWindowRect`, `IsIconic`,
`IsHungAppWindow`).

| case | stage 43 build | this build |
|---|---|---|
| drag session on the anchor (keymap `drag begin`, moves, `drag end`) | the modeless window stays put mid-drag (offset -160,360 instead of 80,420) | offset kept mid-drag and after the drop |
| `move-to-monitor left` / `right` | stays on DISPLAY1 while the anchor is on DISPLAY2 | lands on DISPLAY2 at the same offset, comes back |
| `switch-workspace 2` / `1` | stays on screen while the anchor is parked 1096 px above | parked with it (off every screen), back with it |
| `minimize-window` (Mod+M) on the anchor | Windows hides it and shows it on restore; the rider is forgotten (the anchor's foreground on restore was handled before Windows showed it) | the same from Windows; the rider is kept |
| strip scroll (`focus right` twice: the anchor is parked 2888 px left) | the modal dialog stays at its place on screen | moves by -2888,0 in the same batch (`instantly`, the anchor was teleported past the neighbour), back by +2888,0 |
| another tile takes the foreground | the dialog stays topmost, above every tile and float | not topmost, directly above its anchor; the float over it covers it |
| a foreign window (tool window) takes the foreground | the dialog stays topmost over it (its own flag was "put back") | not topmost, under the foreign window, directly above its anchor |
| the dialog takes the foreground back (layer raised) | topmost above the raised float | the same (ADR-0024's protection kept) |
| the anchor's UI thread frozen (`IsHungAppWindow` after 5.0-5.3 s), then `focus right` | the command returns in 37 ms, nothing moved; the rider is forgotten when Windows' ghost hides it | the command returns in 42 ms, nothing moved, `riders: ... stays where it is: its anchor ... is not responding`; the rider is kept and keeps its offset after the resume |
| ERROR / WARN in the log | none | none |

Freezing the stand-in with `NtSuspendProcess` never made `IsHungAppWindow`
report it within 20 s in the guest; the UI-thread sleep of stage 41 does.

## Consequences

- The SSMS connect dialog moves with its window on every daemon move and
  sits above its window, under whatever covers it, while the user is in
  another app or another window; it is above every float while SSMS has the
  foreground.
- A modeless owned window (a Find dialog, a tool palette) no longer stays
  above every flow window until a foreign app takes the foreground (ADR-0024
  consequence superseded): it sits above its own window.
- A user's own move of an anchor outside a drag session (a float moved by
  an app) is not followed; neither is a move of the rider itself undone: the
  next daemon move carries the rider from wherever it is.
- Residual: owned windows that never took the foreground are not riders and
  do not follow (Windows' topmost propagation applies to them as before). A
  hard kill of flowd leaves a rider parked off screen with its anchor (the
  next start's rescue brings the anchor back, not the rider).
- Cost: a batch with riders reads two rects per rider; nothing for a batch
  without riders. A focus change syncs the riders (a few windows): one
  ex-style read each, plus the owner walk (at most eight `GetWindow`).

## Tests

Unit tests: `top_layer` (`the_seat_rule_holds_a_rider_topmost_only_for_its_owner_group`,
`the_step_rule_raises_seats_once_and_releases_with_the_own_flag`,
`the_seat_plan_goes_under_the_window_above_the_owner_in_the_owners_band`,
a dialog of a tile seated above it when another window takes the
foreground, a dialog topmost on its own seated without a fight and its flag
back on release, a float never covering the foreground anchor's dialog,
riders of a minimized anchor kept, riders noted with the layer off) and
`rider_follow` (the verdict, the base pair, batch targets with size and
`instant`, hung and hidden riders, teleport, drag and rescue). VM:
`test-stage44.ps1` with `guest\stage44-lib.ps1` and `guest\testwin44.ps1`.
