# Owned windows of flow windows ride the floating layer

**Status:** Accepted (stage 31, 2026-11-03). Amends
[ADR-0013](0013-floating-layer-follows-adr-0003.md): an untracked window
owned by a flow window is no longer a "non-flow" foreground. Membership of
the layer, the drop / raise rule for every other window and the shell
exception are unchanged; explicitly ignored shell surfaces leave the
membership (below).

## Context

On 2026-11-02 SQL Server Management Studio 21 locked the user out: its
"Cannot connect" error box stayed behind the SSMS window and could not be
reached, and every click on SSMS's taskbar button activated the invisible
box again. The daemon log showed the SSMS main window as a layer member
(`Floating(Active)`: no learned rule matched the new per-process class,
`default_action = "float"`), the connect dialog and the error box as
untracked windows, and the layer dropping and raising on every activation:

- **Owned windows are never tracked.** `creation_gates` rejects a window
  with a `GW_OWNER` (`OwnedOrChild`), so a modal dialog or a message box
  never gets a registry record.
- **An untracked foreground was always `Foreign`.** The error box taking
  the foreground dropped the layer, and the drop seated every member under
  it with `SetWindowPos(member, hwndInsertAfter = foreground)`. For the SSMS
  main window that asks Windows to put an owner directly under its own
  popup; Windows moves the whole owner group, and where the modal box ends
  up among its siblings is up to Windows.
- **Every flow activation raised the members above it again.** A tile of
  another app (Teams, Copilot) taking the foreground is `Flow`, so the layer
  raised: `HWND_TOPMOST` for the SSMS main window and every other float. The
  box itself was never put in the topmost band, so any float — or the owner
  group's own order — covered it.

niri treats dialogs as floating windows: they sit above the strip with the
rest of the floating layer. The fork's layer had no place for a window it
does not track.

## Considered Options

- **(a) Riders: an owned foreground is classed by the flow window that owns
  it and held in the topmost band with the layer.** Chosen.
- **(b) An owned foreground counts as `Flow`, nothing more.** Rejected: the
  members already topmost cover the non-topmost modal box, and the next
  raise covers it again.
- **(c) Demote only the owning member while its popup has the foreground.**
  Rejected: it keeps the self-referential seat, other floats still cover the
  popup, and a tile activation covers it again.
- **Track owned windows as `Ignored(ExplicitRule)`.** Rejected: it would
  route every dialog, menu-like popup and tool palette through
  classification, seating, borders and rule learning, and change behaviour
  for users without the layer.
- **A tier-2 anchor (an unowned foreground, or one owned by a hidden
  window, anchored at a disabled tracked window of the same process).** Not
  built: in the VM (stage 31, P0 and R1–R5) the error box's owner chain
  reaches the tracked main window, directly or through the connect dialog,
  in WinForms and in WPF. The same probe shows why a *float* owner did not
  lock the stand-in out even before this change: `HWND_TOPMOST` on the owner
  lifts its whole owner group (the box stays above it), and
  `SetWindowPos(owner, hwndInsertAfter = its own popup)` changes nothing.
  The stand-in lock-out is the *tile* owner (R3/R4: the box is never
  topmost, any raised float covers it) plus the flags the old drop left
  behind (the no-op seat kept the owner group topmost while the daemon
  believed it released, so `tiling off`, `flow stop` and a later foreign
  window left it on top). Whether SSMS's box is owned by its main window is
  still open; a read-only enumeration of SSMS's windows the next time the box
  shows decides whether the tier-2 anchor is needed.

## Decision

- **A rider** is an untracked, visible top-level window that holds the
  foreground (it took it, or it is the live foreground at the periodic pass)
  and whose `GW_OWNER` chain — at most eight hops, a cycle ends the walk —
  reaches a tracked window, its *anchor* (`top_layer::anchor_of`).
- **Classification.** Shell classes first, as before. Then an untracked
  foreground with an anchor is classed by the anchor: the live fullscreen
  check on the rider and on the anchor (either one fullscreen → `Foreign`),
  then `owns_foreground(anchor.state)`. A dialog of a tile, a float or an
  explicit-rule ignore is `Flow`; a dialog of a maximized, minimized or
  live-fullscreen window is `Foreign`, as is a window whose chain never
  reaches a tracked window (an owner that is a hidden helper of an untracked
  process).
- **Z-order.** While the layer is raised a rider is held in the topmost
  band like a member: an entering rider always gets `HWND_TOPMOST` (even
  when it inherited the flag from its owner) so it lands above the members
  raised before it. On a raise flip the members are raised first, then the
  riders, owner first, the foreground window (member or rider) last. A demoted rider is raised
  again by the periodic pass. A drop seats the riders under the dropping
  foreground first, then the members. No window is ever seated under a
  window on its own owner chain: when the dropping foreground is owned by
  the member or rider being dropped, it gets `HWND_NOTOPMOST` instead
  (`seat_below`).
- **Riders never get** a registry record, a classification, a seat, a
  border or a learned rule. They live in a side table, `FlowWM::riders`
  (`anchor`, `on_top`, `was_topmost`).
- **The flag a rider had.** `was_topmost` is "topmost when it entered and
  no window on its owner chain was held by the layer": a flag Windows
  propagated from the daemon's own raise of the owner is not the rider's,
  and is cleared when the rider leaves.
- **Leaving.** A rider leaves the band, with its flag put back, on a drop,
  `tiling off`, the option turned off (`sync_all_top_layer`), and on
  `flow stop` / fail-back / session end (`release_top_layer` releases riders
  first, then members, and clears the table). It is forgotten when it is
  hidden (`on_window_hidden`, flag put back first) or destroyed
  (`on_window_destroyed`), and purged by every pass when its class can no
  longer be read, it is no longer visible, or its anchor is no longer
  tracked (flag put back when the window is still there).
- **Explicitly ignored shell surfaces are not members.** Start menu and
  search surfaces (`StartMenuExperienceHost` / `SearchHost`
  `Windows.UI.Core.CoreWindow`) are `Ignored(ExplicitRule)` by the default
  rules, and so were layer members that got `HWND_TOPMOST` on every raise. A
  tracked window whose class is in `SHELL_CLASSES` is never wanted in the
  layer.

## Consequences

- The SSMS case: the connect dialog and the error box are `Flow` riders of
  the main window. No drop happens when they take the foreground, so the
  self-referential seat is gone by construction, and a tile activation
  leaves them above every member.
- **Modeless owned windows** (a Find dialog, a tool palette) that once took
  the foreground stay above flow windows while the layer is raised, until
  they are hidden or closed or a foreign app takes the foreground (they
  drop with the layer and come back with the next raise). Inside the band
  the normal activation order applies. Intended: they belong to a flow
  window, as in niri.
- **Owned windows that never take the foreground** (tooltips, menus,
  notification popups) are not riders; Windows' own topmost propagation from
  the owner applies to them as before.
- **A hard kill** leaves riders topmost, as it leaves members (ADR-0009);
  an inherited flag clears when the owner is set `HWND_NOTOPMOST`.
- **UIPI.** An elevated owner or rider refuses `SetWindowPos`: logged at
  debug, same as members.
- **Book mode** piles (`insert_below_quiet`) never touch riders.
- **Cost.** The owner walk (at most eight `GetWindow` calls) runs only for
  an untracked, non-shell foreground; the side table holds the few riders
  the user saw, and the purge reads a class and the visibility of each per
  pass.
- **Not changed.** With an untracked foreground the keymap's focus commands
  still work from the last tracked focus (`registry.focused()`); resolving
  them from a rider's anchor is a separate follow-up.
- **Observability.** `top layer: rider 0x.. (anchor 0x..) entered|left|
  forgotten` at info level.
- **Tests.** Unit tests against the `TopmostProbe` mock (which gained
  `owner_of` and `is_visible`): the anchor walk (hop cap, cycle), a dialog of
  a float raised after the members (owner first, foreground last), a dialog
  of a tile held topmost and re-asserted, a dialog of a live-fullscreen
  anchor and a popup of a hidden untracked owner staying `Foreign`, an owned
  shell surface still `Shell`, a drop demoting riders first and never
  seating an owner under its own popup, release and tiling off taking riders
  out with an inherited flag cleared and an own flag kept, stale purge and
  the hide / destroy paths, and an explicitly ignored shell surface never
  entering. The VM driver `test-stage31.ps1` (P0, R1–R10, Z) checks the real
  desktop; see `docs/src/dev-guide/floating-space.md` (*The floating layer*,
  *Riders*).
