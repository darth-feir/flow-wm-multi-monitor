# Width patterns are screen slots, filled from the focused column

**Status:** Accepted — supersedes the by-position rule of
[ADR-0014](0014-width-patterns-by-column-position.md) for `+` patterns. The
pattern grammar, the per-screen rules, the validation against the real
screens and the pin (resize commands refused while a pattern is on) are
unchanged.

A `+` pattern's `n` terms are the **slots** of the screen, left to right.
The strip has an **anchor**: the column whose left edge sits at the
viewport's left edge (the first column at or past `viewport_offset`, capped
so a whole run of `n` slots fits before the strip's end). Column `i` takes
term `(i − anchor + slot_shift) mod n`, Euclidean, and the camera starts on
the anchor, so the visible run of `n` columns fills the screen exactly and
the view always begins on a column boundary. Applying a pattern (Mod+R,
`flow dispatch width`) resizes the columns on screen in place and chooses
`slot_shift` so that the **focused column takes the first term**; the slots
are then fixed on screen while the strip scrolls — a column that comes in
past an edge takes the slot it lands in, the column that leaves gives its
slot up — and `slot_shift` is kept across reload and hotplug carry-over,
reset only when the pattern is cleared or replaced by a fraction.

## Context

ADR-0014 assigned a pattern's terms by strip position (`terms[i mod n]`),
which is stable under focus and scroll. One day of use showed two things
the user did not want. First, *which window is the big one* was decided by
where it happened to sit in the strip: with `1/3+2/3` the user could not
make the window they were in the wide one without scrolling until the
positions lined up. Their request, verbatim: *"if i have the right window
selected and do the resize it makes that one the big one … then that column
stays that way so if i do mod left the window that was on the left now
resizes to the column it is taking."* Second, the camera kept its raw pixel
offset across a width change, so a scrolled view began mid-column; on a
monitor with a neighbour on that side the cut column is parked by the
overhang rule, which left a bare gap at the left edge and pushed the last
column off the right (fixed on its own in a6808bc by re-anchoring the camera
on the left-edge column; this record keeps that rule and builds on it).

## Considered Options

- **Keep widths by strip position.** Rejected by the user: the arrangement
  is a property of the screen (a wide column *here*, a narrow one *there*),
  not of the strip's numbering, and the numbering is invisible to them.
- **The leftmost window on screen takes the first term.** Rejected: it fixes
  the screen's shape but the user still cannot choose where the main slot
  lands without scrolling; "the window I am in becomes the big one" is the
  gesture they described.
- **The focused window takes the first term, the slots are then fixed
  (chosen).** The slot order is one small integer per strip (`slot_shift`),
  picked when the pattern is applied and left alone afterwards, so a focus
  change within the visible run moves nothing, and a scroll past an edge
  re-fills the same slots with the columns that are now on screen. The
  user writes the main size first (`2/3+1/3`), and the pattern continues to
  the right of the focus and wraps round to its left.
- **Anchor the run on the focused column, re-choosing the shift on every
  focus move.** Rejected: that is a resize in disguise — every focus change
  within the run would move windows — which is what the user has asked to
  get rid of twice.
- **Snap the camera to column boundaries in the mutation functions.**
  Rejected: the mutation functions are deliberately width-agnostic and
  many; the slot rule is one function applied to every layout they return
  (`pin_to_slots`, in `apply_mutation_for`), where the position pin already
  ran.

## Consequences

- **One rule, one place.** `anchor_of` finds the anchor under the widths
  the mutation left in place; `pin_run` writes the widths and the camera;
  `pin_to_slots` runs both on every new layout and in `refit_to_config`,
  and keeps the column to show (the swap target, else the strip's focus) in
  view: terms adding up to less than the screen can let more than `n` old
  columns fit before the re-flow, so when that column would end past the
  right edge the run is moved right so that it ends it. No mutation
  function and no caller changed.
- **Scrolling re-fills the slots.** Focus past the left edge makes the
  incoming column the anchor: it takes the left slot's width, the old anchor
  moves to the next slot, the last column of the run leaves on the right.
  Past the right edge the first column that is fully visible in the
  right-aligned view becomes the anchor; with terms adding up to one screen
  the right-aligned and left-aligned views coincide, with less the view is
  left-aligned and the strip's end may leave room on the right.
- **Insert, remove, swap.** A new column takes the slot it is created in
  and the run keeps the focus in view, so an appended focused window moves
  the run one column right and the leftmost column leaves; removing the
  anchor makes the next column the anchor; swapping two visible columns
  swaps the windows and leaves the slot widths on screen.
- **Changing the number of slots.** The anchor stays and the `n` slots are
  filled from it; extra columns leave on the right, except that the focus
  is kept by moving the run right by the minimum, so the focus is the last
  slot, and the focus-first shift is then applied to that run. Growing —
  more slots, or a pattern to a fraction — fills to the right from the
  anchor; at the end of the strip the end-of-canvas clamp (the `len − n`
  cap for a pattern, `reanchor_after_reflow` for a fraction) moves the view
  left so the screen is full and there is never a gap.
- **Observability.** `flow query all` reports `slot_anchor` and
  `slot_shift` per monitor; the daemon's refusal names the pattern and says
  the columns on screen take its shares left to right.
- **Accepted costs.** Every scroll past an edge resizes the whole visible
  run, which is the point. A reload or hotplug that lands a strip on a
  pattern it was not on uses slot order 0 (nothing was focused when the
  user chose it). The example config now reads `"2/3+1/3"`: a user who kept
  `"1/3+2/3"` gets the focused window *narrow*, as written.
- **Tests.** In the engine: the focused column takes the first term for
  each of three focus positions and for `2/3+1/3` with the right window
  focused; focus within the run moves nothing, past the left edge the
  incoming column takes the left slot, past the right edge the slots stay;
  a column appended next to the focus takes its slot and the leftmost
  leaves, removing the anchor re-forms the run; a swap moves windows not
  slots; four `1/4` columns to three slots with the first and with the last
  column focused, and back, with and without a fifth column; terms under one
  screen keep the focus in view; a carried-over pattern keeps its slot
  order and a fraction resets it; the pattern → fraction → pattern camera
  re-anchoring from a6808bc. In the daemon: slots from the focused column on
  apply and re-apply, append, prepend, remove, swap and back to a fraction,
  the four refusals and a slipped resize undone. The VM driver
  (`test-stage23.ps1`) focuses the right window, applies `2/3+1/3` by a real
  Mod+R and checks it became the wide one, then a real Mod+Left past the
  edge brings the incoming window into the left slot with the slots
  unchanged, no gap at the left edge and nothing off the right. See
  `docs/src/user-guide/config-reference.md` (*Width patterns*) and
  `docs/src/dev-guide/layout/mutations.md` (*Width patterns pin columns to
  screen slots*).

## Amendment (stage 28, 2026-10-27): fullscreen columns

A fullscreen column (ADR-0021) is a run of one: found at the left edge it is
the anchor, never capped. A run of `n` slots never holds a fullscreen column
other than its anchor; when one would fall inside it the run ends just before
it (anchor `fullscreen − n`), and no anchor moves back onto or across an
earlier fullscreen column. A fullscreen column takes no term; the terms are
counted over the other columns. See
[ADR-0021](0021-fullscreen-is-a-column.md), *Amendment to ADR-0017*.
