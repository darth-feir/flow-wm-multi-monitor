# Windows that exist before the daemon are adopted, not skipped

**Status:** Accepted.

A window the daemon meets in a state it cannot tile — minimized, maximized,
or DWM-cloaked — is **registered anyway** and brought into the layout at the
first moment it can be: a minimized window is classified from its
`WINDOWPLACEMENT` and seated when restored; a maximized window is
un-maximized by the daemon itself when `adopt_maximized = true` (and
maximized again on pause and stop), or adopted on demand by `set-window`;
a cloaked UWP frame is registered when DWM uncloaks it. Any hook that sees a
window the registry does not track (restore, foreground, show, uncloak, and
`set-window` on the foreground) gives it a run through the creation pipeline
first.

## Context

The first real-desktop trial surfaced the same complaint three ways: *apps
that were open before the tiler launched are not managed; I can close and
reopen them but I would rather not.* Outlook had been minimized when the
daemon started and was skipped by the scan; Firefox and Alacritty had
started maximized, were parked as `Ignored(Maximized)`, and the
`EVENT_OBJECT_STATECHANGE` that upstream relied on to re-classify them on
un-maximize never arrived; the Settings app (an `ApplicationFrameWindow`)
is created cloaked and uncloaked by DWM once its content is ready, with no
`SHOW` and no second `CREATE`, so the creation gates rejected it every time.

## Considered Options

- **Keep the gates and tell the user to reopen the app.** Rejected by the
  user, and it contradicts the fork's purpose: a scroller that only manages
  windows born after it is not a daily driver.
- **Treat a minimized window as a float and leave it.** Rejected: a
  minimized window has no on-screen rect to float at, and the user's
  minimized windows were mostly tiles. Instead the window is classified from
  the rect it will restore to (`rcNormalPosition`, converted from workspace
  to screen coordinates) and recorded in the `Minimized` variant of its
  class — the same record a window minimized *after* being managed has — so
  the existing `MINIMIZEEND` path seats it with no new code.
- **Un-maximize maximized windows always.** Rejected as a default: upstream
  users expect a maximized window to be left alone. `adopt_maximized` is a
  top-level key, default `false`; the fork's example config turns it on. The
  decision is reversible per window: `tiling off`, `flow stop` and the
  fail-back put an adopted window back to maximized rather than moving it.
- **Poll for un-maximized windows.** Rejected: the missing `STATECHANGE` is
  the exception, not the rule. The re-classification runs instead on the
  events that *do* arrive when the user next touches the window —
  foreground, show, move-size end — gated by a single `HashMap` lookup, so
  the common case costs nothing.
- **A ninth hook for DWM cloaking (chosen for UWP).** `EVENT_OBJECT_CLOAKED`
  / `UNCLOAKED` are hooked as one non-mandatory range: uncloak on an
  untracked window attempts creation, on a tracked one is handled like
  `SHOW`; cloak is handled like `HIDE` (some UWP apps cloak rather than
  minimize).

## Consequences

- **Being iconic is no longer a creation gate**, and the gates are one
  function (`creation_gates`) shared by the scan, `handle_created` and
  `set-window`, so a refusal names the gate.
- **An icon's rect is never a window's geometry.** `IsIconic` flips before
  the frame has left the icon, so `GetWindowRect` can still report
  (-32000, -32000) at `MINIMIZEEND`; every reader checks `is_iconic_rect`
  and waits. A record registered from a placement carries
  `geometry_from_placement` and is re-measured (rect, natural size,
  invisible borders) on its first restore, before it is seated. Getting this
  wrong makes the icon the pre-tiling rect and `tiling off` hands the window
  back icon-sized.
- **Adoption is learned like a float/tile transition.** `set-window
  tile|cycle|float|fullscreen` on an ignored window records the decision as a
  learned rule; a rule in the user's own `flow-rules.toml` still wins over a
  learned one, a learned one outranks a bundled ignore.
- **`set-window` acts on the OS foreground**, registering an untracked one
  first; `cycle` on a window with no state means tile. Only a minimized or
  hidden window is still refused.
- **Accepted costs.** Un-maximizing a window the user wanted maximized is
  a visible change of policy, hence the default. A window that restores to
  maximized waits as `Ignored(Maximized)` until it is on-screen, because its
  OS state cannot be read while iconic. The cloak hook adds one more event
  stream to filter.
- **Tests.** Placement conversion, minimized registration and restore, the
  gates, the `set-window` decision table with the ignored column, adoption
  as tile/float/cycle (seated, learned, idempotent), pause re-maximizing and
  resume un-maximizing adopted windows, loadout leftovers. See
  `docs/src/dev-guide/window-registry.md` (*Recovery hooks*, *Windows met
  minimized*) and `docs/src/dev-guide/classification.md`.
