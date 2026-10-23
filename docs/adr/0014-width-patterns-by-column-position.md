# Width patterns are assigned by column position and pin the strip

**Status:** Accepted — extends the `[columns]` preset grammar of the fork
(`presets = ["1/3", "1/2", "1/1"]`, per monitor) with `+` patterns and
per-screen rules. The resize commands and the ladder are unchanged for a
plain fraction.

A `[columns]` preset is either one fraction of the work area or a **pattern**
of fractions joined with `+` (`"1/3+2/3"`, `"1/4+1/4+1/2"`). Column `i` of the
strip — counted from its left end, 0-based — takes term `i mod n`, so the
pattern repeats along the strip by position and a whole period plus its gaps
fills the work area exactly once (the terms may add up to at most `1/1`).
Widths are therefore a function of position alone: they never change when
focus moves or the view scrolls, inserting or removing a column re-flows the
columns after it, and while a pattern is on the strip is **pinned** —
`expand-column`, `shrink-column`, `set-column-width` and `toggle-monocle` are
refused with a message naming the pattern, and a mouse resize snaps back on
release. A single fraction is the `n = 1` case and behaves exactly as before.
`[[columns.screen]]` rules give a screen matched by orientation, work-area
width or device name its own list and floor; a screen takes the first rule
that matches it, else the top-level list, re-evaluated on every display
change and carried over by width (the same preset when the new list has it,
else that list's default). Every list is checked against the attached
screens at startup and before a reload is applied; the pixel floor is only a
fallback for a display that appears later.

## Context

The user does not resize tiles by hand and does not want a column's width to
depend on which window has the focus; what they wanted from niri's
per-column `switch-preset-column-width` was the *arrangements* it allows —
a wide editor beside a narrow terminal (`1/3+2/3`), two narrow panes and a
wide one on a larger display (`1/4+1/4+1/2`) — without the resize. The fork's
presets already applied one width to every column of a screen, and the user
also wanted a portrait display or a wider desk monitor to get a different
list than the two 1920×1080 screens (`1/4` on a 1920 screen is 460 px, under
the 600 px floor; `1/3` on a 1080-wide portrait screen is 338 px), with a
narrow preset *refused* rather than silently widened to the floor, which it
was until now (`preset_width_px` floored every width).

## Considered Options

- **Per-column preset cycling (niri's model).** Rejected by the user: the
  focused column changes width, every other column keeps its own, and the
  strip ends up a mix the user has to tidy. The point of presets here is
  that a screen looks the same wherever the focus is.
- **A pattern anchored on the focused column** (the focused column takes
  the largest share, its neighbours the rest). Rejected: it is a resize in
  disguise — every focus change moves windows — which is the behaviour the
  user asked to get rid of. It also needs a rule for what "the rest" means
  with more columns than terms.
- **A pattern anchored on the viewport** (the leftmost *visible* column
  takes term 0). Rejected: a scroll by one column would re-assign every
  width, moving every window on screen; by position, a scroll just shows the
  next period, and the period being one screen wide means an aligned view
  always shows one full period.
- **Assigned by column position, repeating (chosen).** `terms[i % n]` is one
  line, is stable under focus and scroll, and makes every structural
  mutation re-flow correctly without a case for each. The cost — the
  columns after an insert or remove change width — is inherent to any
  position-based rule and is the behaviour the user described.
- **Let resize commands override the pattern.** Rejected: the override
  would be undone by the next insert or remove (the re-flow is what makes
  patterns work), so the honest answer is to refuse up front and say why.
  A plain fraction keeps the ladder and free resize unchanged, so a user
  who wants to resize picks one.
- **Match screens by index.** Rejected: monitor indices change with the
  primary display and with hotplug order. Orientation and work-area width
  survive renumbering, and the device name (`\\.\DISPLAYn`, from
  `MONITORINFOEXW`) is there for a specific display.
- **Keep flooring presets silently.** Rejected by the user: "refused rather
  than silently clamped". The check needs the real screens, so it runs
  where they are first known — the `monitors` startup stage and the reload
  handler, before anything is applied. A display that appears later cannot
  fail a reconcile, so hotplug warns and floors.

## Consequences

- **Where the rule runs.** `ScrollingSpace` keeps the period as pixel
  widths (`width_pattern`); `apply_mutation_for` pins every new layout
  before normalization and projection, and `refit_to_config` pins after a
  geometry change, so no mutation function and no caller needs to know
  about patterns. The daemon refuses the four resize commands up front
  (`FlowWM::pinned_widths_refusal`) rather than letting them be undone.
- **One resolution function.** `resolve_for_screen` (startup, hotplug,
  reload) picks the list for a screen's facts, carries the previous preset
  over by width and computes the floored pixel widths, so the three paths
  cannot disagree. Reload captures every monitor's preset *before* the new
  config is published (`current_presets`), which is what makes carry-over
  by width rather than by index possible.
- **`Monitor` knows its device name**, `MonitorGeometry` carries
  `szDevice` as a fixed `[u16; 32]` so it stays `Copy`, and `flow query all`
  prints `device_name` so the user can write a `name` rule.
- **Accepted costs.** A new window pushed in at the front re-flows every
  column after it (visible movement on a pattern screen; none on a plain
  fraction). The keymap no longer binds Mod+Minus / Mod+Equal — the daemon
  commands stay, and a user who resizes can bind them back. A rule list is
  validated per screen, so adding a display with no rule for it may put a
  narrow preset at the floor with a warning instead of an error. The
  pattern's first term is the base width new columns are created at before
  the pin re-flows them, so the ladder bookkeeping (`max_n`) is computed
  from a width the pin may immediately replace; it is unused while pinned.
- **Tests.** The parser (patterns, bad terms, a period wider than the
  screen), the per-term pixel widths and their sum against the work area,
  rule matching (orientation, width bounds, name with and without the
  prefix, first match wins, fall-through) and validation messages, the
  per-screen check naming the term and the screen, carry-over by width;
  in the daemon, a pattern pinning and re-flowing on insert, prepend,
  remove and swap, the four refusals and a slipped resize undone, reload
  onto and off a pattern, a rule giving a monitor its own list and floor, a
  reload refused with the old config kept, and a hotplug pass that keeps a
  survivor's pattern, starts a new named display on its rule's default and
  lands a rotated display on its portrait list. The VM driver
  (`test-stage20.ps1`) cycles Mod+R by real key input through
  `["1/3", "1/3+2/3", "1/2", "1/1"]` and checks the column rects by
  position, a `name` rule for the second display, a display re-plug, and
  reports whether the guest could rotate a display. See
  `docs/src/user-guide/config-reference.md` (*Width patterns*, *Per-screen
  rules*) and `docs/src/dev-guide/layout/mutations.md` (*Width patterns pin
  columns by position*).
