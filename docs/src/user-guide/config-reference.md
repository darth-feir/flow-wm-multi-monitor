# Config Reference

FlowWM splits configuration across two TOML files in your config directory
(`%USERPROFILE%\.config\flow\` by default):

| File | Controls |
|------|----------|
| `flow.toml` | Geometry, padding, animation, borders, floating defaults, focus reconciliation, [key bindings](#binds--key-bindings), [startup programs](#startup--programs-to-run). |
| `flow-rules.toml` | Window classification — which apps tile, which float, which are ignored entirely. |

Both files are optional — `flow config init` writes commented defaults, but you
can delete either file (or run with an empty one) and the daemon falls back to
compiled-in defaults. Your `flow.toml` may be partial: missing fields are
filled in from `FlowConfig::default()`.

> This page is the **user-facing** reference: every field, its default, what it
> does. For the internals — config directory resolution, the 4-phase load
> lifecycle, hot-reload semantics, validate-before-apply — see
> [Config & Persistence](../dev-guide/config-and-persistence.md) in the
> Developer Guide.

## Editing and reloading

Edit either file in any text editor and save it: the daemon notices the
change within a second or two and reloads it (the log says `config:
reloaded`, or `config: reload refused: ...` with the reason). To reload at
once, or from a script:

```powershell
flow config reload
```

Either way the daemon reloads `flow.toml` and `flow-rules.toml` from disk,
validates them **before** touching any state, and applies the live-reloadable
fields. A broken file leaves the running daemon untouched — there is nothing
to roll back — and a save made while tiling is off is applied when tiling
resumes.

To list every problem without applying anything:

```powershell
flow config check
```

With a daemon running the check is done by the daemon, so it also knows the
attached screens (preset lists are checked per screen) and reports each bad
`[binds]` entry on its own line; without one, the files are checked locally.

### Live-reloadable vs. structural fields

Not every field can change at runtime. Reload applies **live** fields in place;
**structural** fields require a daemon restart.

| Field | Reload effect |
|-------|---------------|
| `borders.*` (color / thickness / overlap) | Immediate recolor/resize of all border overlays. |
| `padding.window_gap` | Re-projects every workspace's pixel layout — windows animate to the new gaps. |
| `columns_per_screen` | Updates the cached threshold (no window movement; affects future auto-center). |
| `column_width`, `min_column_width_px`, `min_window_height_px` | Update cached bounds; existing per-column widths preserved. |
| `columns.presets`, `columns.default`, `columns.screen` | Each monitor's preset is re-resolved against its list: it stays on the preset it was on when the list still has it, else lands on that list's `default` (so a monitor that had no presets before starts on `default`). Existing per-column widths preserved for a plain fraction; a pattern re-pins them to its slots, keeping the slot order the screen had. A list with a term under the floor on any attached screen is refused and the old config kept. |
| `columns.visible_rows` | Every column re-shares its height over the new viewport size and the focused row is kept visible (windows animate). |
| `columns.max_rows` | Applies to the next merge/consume/insert; a column already over the new cap keeps its rows until windows leave it. |
| `columns.layout`, `columns.screen[].layout` | Every screen switches to the mode the file gives it (columns out of view slide off-screen or onto the piles); a screen switched at runtime with `set-layout` / Mod+Shift+B goes back to the file's mode. |
| `columns.wrap_around` | Applies to the next `focus left/right` at a strip end. |
| `animation.*` | New duration / easing / enabled flag applies to the next animation. |
| `navigation.edge_hop` | Applies to the next `focus` / `move-window` at a strip edge. |
| `adopt_maximized` | Applies to the next maximized window the daemon meets (a window already parked as ignored is adopted when it is next focused, shown or moved). |
| `floating.always_on_top` | Immediate: turned on, every floating window and every rule-ignored window is raised above the tiles; turned off, they go back to the normal z-order (a window that was "always on top" by itself keeps that). |
| `binds.*` | The daemon re-resolves the table; the hotkey script re-reads it a few seconds after the save and re-registers the chords (one-line message). A table with an error keeps the old chords. |
| `startup.*` | Applies to the next daemon start only. |
| `keymap.selection_buffer` | The hotkey script re-reads it with the bindings a few seconds after the save: the selection buffer starts or stops (stopping forgets the buffered text). |
| **Workspace count (10), monitor geometry** | **Structural** — not reloadable. Restart the daemon. |

What is preserved across reload: the user's arranged layout (columns, rows,
window order), focus, scroll viewport, monocle state, the daemon's window
registry, learned classification rules, and float positions.

### Rules reload is non-fatal

If `flow-rules.toml` fails to load, the daemon **keeps the current rules** and
warns on stderr; `flow.toml` (if valid) still reloads. This per-file-independent
policy is intentional. Reloaded rules affect only **newly opened** windows —
existing layout is never disrupted.

---

## `flow.toml`

The application config. Schema:
[`schemas/flow-config.schema.json`](https://github.com/CCpcalvin/flow-wm/blob/master/schemas/flow-config.schema.json).
A hand-written, fully-commented example lives at
[`default-config.toml`](https://github.com/CCpcalvin/flow-wm/blob/master/default-config.toml)
in the repo root — `flow config init` copies it verbatim.

> **Single source of truth.** Default values live in code, in the `Default`
> impls in `src/config/types.rs`. The `default-config.toml` example is enforced
> by a test (`default_config_toml_matches_compiled_defaults`) to deserialise to
> exactly `FlowConfig::default()`. If you ever wonder "what's the real
> default?", trust the table below over an outdated copy you find in the wild.

### Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `columns_per_screen` | `u32` | `4` | How many columns fit side-by-side on one monitor. The pixel width of each column is derived at runtime from this, the monitor resolution, and `padding.window_gap`. |
| `column_width` | `Option<u32>` | `None` | Optional fixed pixel width per column. When set, ignores `columns_per_screen`. Useful on ultrawide monitors where you want consistent column sizes regardless of monitor count. Both are ignored when a [`[columns]`](#columns--column-width-presets) section is present. |
| `min_column_width_px` | `u32` | `640` | Floor for column width in pixels. Columns cannot be resized below this. |
| `min_window_height_px` | `u32` | `100` | Minimum row height in pixels — the floor for any single window's allocated height inside a column. Bounds the maximum number of rows that can stack inside one column. |
| `min_row_height_px` | `u32` | `100` | Floor for one row while a row boundary is dragged: neither row shrinks below it (the edge pins and snaps back on release). Independent of `min_window_height_px`. |
| `check_for_updates` | `bool` | `false` | Whether `flow start` runs the update checker (the same one behind `flow update`, configured by [`[update]`](#update--update-checker)) and prints a one-line notice when something newer exists on GitHub. **Off by default in this fork** — nothing is contacted at start unless you opt in with `true`. When on it runs after the daemon is ready, is bounded by a 5 s timeout, stays silent on any failure, never blocks startup and never downloads anything. The explicit `flow update` command is unaffected by this flag. |
| `adopt_maximized` | `bool` | `false` | Whether a window that is **maximized** when FlowWM meets it — in the startup scan, when it opens, when tiling is turned back on — is un-maximized and classified through your rules (tiled or floated like any other window) instead of being left alone as "ignored: maximized" until you restore it. Fullscreen windows (video, games) are never adopted, nor is a window your rules ignore. An adopted window is maximized again when FlowWM hands the desktop back: `flow dispatch tiling off`, `flow stop`, a fail-back. With `false` (the stock behaviour) a maximized window stays ignored until you restore it or run [`set-window`](./dispatch-reference.md#set-window-mode) on it, which adopts it either way. |

Example:

```toml
columns_per_screen = 4
# column_width = 1280          # optional fixed width; ignores columns_per_screen
min_column_width_px = 640
min_window_height_px = 100
min_row_height_px = 100
check_for_updates = false      # the fork's default; true opts in to the start-time GitHub check (a notice only)
adopt_maximized = false        # true: un-maximize and tile/float windows found maximized
```

These are **top-level** keys: they must come before the first `[section]`
header in `flow.toml`. A key written after a header belongs to that section
and is silently ignored (`[columns]` would swallow a `min_column_width_px`
placed below it).

### `[columns]` — column width presets

Optional. When present, column widths come from an **ordered list of presets**
instead of `columns_per_screen` / `column_width`, and every monitor keeps its
own current preset. `flow dispatch width <preset|next|prev|cycle>` moves the
focused monitor through the list (see the
[Command Reference](./dispatch-reference.md#width-presetnextprevcycle)).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `presets` | `Vec<String>` | `["1/3", "1/2", "1/1"]` | Ordered presets. Each is a fraction `"a/b"` of the work area, a plain number in `(0, 1]` such as `0.5`, or a `+` pattern of those (`"2/3+1/3"`, see below). Must not be empty. |
| `default` | `Option<String>` | `None` (= the first preset) | The preset every monitor starts on. Must be one of `presets`. |
| `visible_rows` | `u32` | `0` | How many rows of a column are on screen at once. `0` shows every row sharing the height. With `2`, a column of four windows shows two at a time and `focus up/down` scrolls through the stack. |
| `max_rows` | `u32` | `0` | The most rows a column may hold; `0` is unlimited. Must be at least `visible_rows` when both are set. Merging into a full column moves the window past it instead. |
| `layout` | `"strip"` / `"book"` | `"strip"` | Where the columns out of view go: `"strip"` parks them off-screen; `"book"` piles them behind the leftmost and rightmost visible columns (see [Book layout](#book-layout-layout--book)). A `[[columns.screen]]` rule can set its own. |
| `wrap_around` | `"off"` / `"on"` / `"book"` | `"off"` | Whether `focus left/right` wraps from one end of the strip to the other: never, on every screen, or only on a screen in book layout. Moving a column never wraps. |
| `screen` | `Vec<table>` | `[]` | `[[columns.screen]]` rules giving a matched screen its own list (see [Per-screen rules](#per-screen-rules-columnsscreen)). |

A preset `1/n` is sized so that `n` columns **plus their gaps** fill the work
area exactly: `(work_area_width - window_gap * (n + 1)) / n`. So `"1/1"` is
`work_area_width - 2 * window_gap` (the whole work area), and on a 1920 px
monitor with the default 16 px gap `"1/2"` is 936 px and `"1/3"` is 618 px.
Borders are unaffected.

`min_column_width_px` is a floor that is **checked, not applied silently**:
at startup and on reload every preset of every attached screen's list is
measured on that screen, and a term that comes out narrower than the floor
is an error naming the preset, the screen and the width — `columns.presets:
"1/3" is 602 px wide on DISPLAY1 (1872x1080 work area), below
min_column_width_px 640`. Startup stops at the `monitors` stage; a reload
keeps the old config. Give that screen its own rule, or lower the floor. (A
display that appears *later* with no rule for it is only warned about; its
narrow terms are widened to the floor so the columns stay usable.)

The monitor's current preset is the base width of its strip: new columns are
created at it, and the `expand-column` / `shrink-column` ladder starts from it.
Widths follow the monitor: a column that arrives from elsewhere —
`move-to-monitor`, an edge hop, a window re-tiled from floating or fullscreen,
a column migrated by a display change, a loadout column merged from a detached
monitor — takes the receiving monitor's preset width, never the width it had
where it came from. `flow query all` shows
each monitor's `width_preset` and `device_name`; the loadout saves and
restores the preset per monitor.

Example:

```toml
[columns]
presets = ["1/3", "1/3+2/3", "1/2", "1/1"]
default = "1/2"
```

#### Width patterns: `"2/3+1/3"`

A preset with two or more terms joined by `+` is a **pattern**. Its terms
are the screen's **slots**, left to right: with `"2/3+1/3"` the screen shows
two columns, one two thirds of the work area and one a third; with
`"1/2+1/4+1/4"` it shows three. The terms may add up to at most the whole
screen (`1/1`), so the slots plus their gaps fit the work area exactly. A
single fraction is the one-term pattern.

Applying a pattern (Mod+R, `flow dispatch width`) resizes the columns on
screen in place, and the **focused column takes the first term** — so write
the main size first: with `"2/3+1/3"` the window you are in becomes the wide
one whichever side it is on, the pattern continues to its right and wraps
round to its left. From then on the slots are fixed on screen. When the
strip scrolls (focus past an edge, a window arriving from another screen),
the column that comes in takes the slot it lands in — the window entering on
the left gets the left slot's width — and the column leaving the view gives
its slot up; a new window takes the slot it is created in and pushes the
last one out of view. A pattern therefore **pins** the strip:
`expand-column`, `shrink-column`, `set-column-width` and `toggle-monocle`
are refused on that screen with a message naming the pattern, and a mouse
resize snaps back on release. Switch to a single-fraction preset to resize
by hand.

Changing the number of slots keeps the column at the left edge where it is:
from four `1/4` columns to `"1/2+1/4+1/4"` the first three stay and the
fourth leaves on the right — unless the fourth is the focused one, in which
case the view moves right just enough to keep it, as the last slot. Back to
`1/4` the run grows to the right, or, at the end of the strip, the view moves
left so there is never a gap. Why slots rather than widths by strip
position: `docs/adr/0017-width-patterns-are-screen-slots.md`.

#### Per-screen rules: `[[columns.screen]]`

Each `[[columns.screen]]` table gives the screens it matches their own
preset list. A screen takes the **first** rule that matches it, else the
top-level `presets` / `default`. Matching is re-evaluated on every display
change (a display plugged in, a resolution change, a rotation): the screen
stays on the preset it was on when its new list has it, otherwise it lands
on that list's `default`.

| Field | Type | Description |
|-------|------|-------------|
| `match.orientation` | `"portrait"` / `"landscape"` | By the work area; square counts as landscape. |
| `match.min_width_px` | `u32` | The work area must be at least this wide. |
| `match.max_width_px` | `u32` | The work area must be at most this wide. |
| `match.name` | `String` | Windows' device name, `"DISPLAY2"` (the `\\.\` prefix is optional, case does not matter). `flow query all` prints each monitor's `device_name`. |
| `presets` | `Vec<String>` | The list for those screens; same grammar as the top-level `presets`. May be left out when the rule sets `layout`: the screen then keeps the top-level list. |
| `default` | `Option<String>` | The preset such a screen starts on; one of the rule's `presets`. |
| `min_column_width_px` | `Option<u32>` | A floor for those screens instead of the top-level one, which is screen-blind. |
| `layout` | `Option<"strip" / "book">` | The layout for those screens instead of the top-level `layout`. |

Every field of `match` that is given must hold, and at least one must be
given. Orientation and width survive display renumbering, so prefer them
over `name` for everything but a specific display. A rule must set
`presets`, `layout` or both; `default` and `min_column_width_px` need the
rule's own `presets`. The first matching rule decides both the list and the
layout of a screen.

```toml
[columns]
presets = ["1/3", "1/3+2/3", "1/2", "1/1"]   # every screen unless a rule matches
default = "1/2"

[[columns.screen]]
match = { orientation = "portrait" }
presets = ["1/2", "1/1"]
default = "1/1"
min_column_width_px = 500                    # 1/2 of a 1080-wide screen is 516 px

[[columns.screen]]
match = { min_width_px = 2560 }
presets = ["1/4", "1/4+1/4+1/2", "1/3", "1/3+2/3", "1/2", "1/1"]
default = "1/3"
```

#### Stacked columns: `visible_rows` and `max_rows`

By default every window in a column shares the column's height, so stacking
four windows gives four short rows. `visible_rows = 2` keeps a column at most
two rows tall on screen: the column shows two of its windows, each taking an
equal share of the work area, and the rest are **parked** off-screen above
or below the monitor (beyond the whole virtual screen when another monitor is
stacked on that side, so a parked row never lands on it). `focus up` /
`focus down` move through the stack one row at a time and scroll the column's
viewport when the next row is parked; `move-window up/down` swaps rows and the
viewport follows the window. Any other way focus lands on a parked row (a
mouse click, `focus-monitor`, a workspace switch, tiling coming back on)
scrolls it into view too. When a column has no more rows than `visible_rows`
nothing changes from today.

`max_rows` caps the stack. `merge-column` and `consume-or-expel` into a full
column do not refuse: the window moves past that column instead (exactly as
`move-window left/right` would), so repeated presses walk it along the strip.
Every other way a window joins a column — a loadout or display-change
restore, a window coming back from floating or fullscreen — appends it as a
new column after the full one. A mouse drag dropped onto a full column is
refused.

```toml
[columns]
presets = ["1/3", "1/2", "1/1"]
default = "1/2"
visible_rows = 2   # a stack shows two windows at a time
max_rows = 4       # and holds at most four
```

Note that `[columns]` also switches column widths to presets: a table with
only `visible_rows` / `max_rows` uses the default `presets`.

`flow query all` reports, per monitor, each column's `row_count`,
`row_offset` (the first visible row), `visible_rows` and `fullscreen`, and
every window's `parked_row`.

#### Book layout: `layout = "book"`

In the default strip layout the columns that are out of view are parked
just off the screens. In **book** layout they stay on the screen: the
columns before the view are **piled** behind the leftmost visible column and
the columns after it behind the rightmost one, each pile at exactly that
column's place and size, nearest column first. The edge columns are the
piles' **top cards**. The strip itself does not change — same order, same
width presets and patterns, same camera — so every command behaves as in
strip layout; only where the columns out of view sit is different.

- **Flipping** is the ordinary one-column scroll: Mod+Right past the right
  edge brings the next card of the right pile to the top while the
  leftmost card sinks onto the left pile. Only the window that changes
  place moves; the others are already where they belong.
- With **`"1/1"`** the screen is a carousel (one card on top of both
  piles), with **`"1/2"`** there are two piles, and with three or more
  columns on screen the middle ones are plain windows.
- A **taskbar click**, **Alt+Tab** or anything else that activates a piled
  window flips the book to it in one step (the columns in between change
  piles).
- **Dragging** a window (Mod+drag or its title bar) onto a top card (all
  of it but the scroll band at the screen edge) makes it the new top card
  of that pile, the old top right behind it; on a lone card (`"1/1"`) the
  half under the cursor picks the pile. An edge column whose pile is empty
  keeps the ordinary drop zones (beside it, or into it as a row). To drop
  deeper, hold the window at the screen edge: the edge scroll flips the
  book, then drop on the card shown.
- A **fullscreen** column (Mod+F) keeps the strip behaviour: it is parked
  off-screen when out of view; while it fills the screen the other columns
  lie under it.
- `wrap_around = "book"` makes Mod+Left / Mod+Right wrap round the strip on
  book screens only — handy for a carousel. While wrapping is on for a
  screen, Mod+Left / Right at its ends wrap instead of hopping to the next
  screen (`[navigation] edge_hop`); a strip of one column has nothing to
  wrap to and still hops.
- A column less than half on screen counts as out of view and is piled
  (under `"2/3+1/3"` the rounding can leave the next column a one-pixel
  sliver at the right edge; strip layout is unchanged).
- `flow dispatch set-layout strip|book|toggle` (bound to **Mod+Shift+B** by
  the win-niri keymap) switches the screen of the focused window until the
  next reload; `flow query all` shows each monitor's `layout` and each
  column's and window's `pile` (`"left"`, `"right"` or `null`).

What to expect: piled windows are covered, not hidden, so the taskbar and
Alt+Tab list them as usual and a browser in a pile may throttle like one
that is minimized. Between the rows of a stacked top card a `window_gap`
high strip of the card underneath shows. An app that brings itself to the
front without being activated is put back under its top card within
`[focus] foreground_sync_interval_ms`. Why it works this way:
`docs/adr/0022-book-mode-piles.md`.

```toml
[columns]
presets = ["1/3", "2/3+1/3", "1/2", "1/1"]
default = "1/2"
layout = "strip"          # every screen unless a rule says otherwise
wrap_around = "book"      # Mod+Left/Right wrap round on book screens

[[columns.screen]]
match = { name = "DISPLAY2" }
layout = "book"           # this screen only, with the list above
```

### `[padding]` — spacing

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `window_gap` | `i32` | `16` | Uniform gap (pixels) between all elements: window-to-window, window-to-screen-edge. |
| `up` | `i32` | `0` | Reserved space (pixels) above the tiling area. |
| `down` | `i32` | `0` | Reserved space (pixels) below the tiling area — e.g. for taskbar clearance. |

Example:

```toml
[padding]
window_gap = 16
up = 0
down = 0
```

### `[animation]` — layout transitions

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `true` | Master switch. `false` = layout changes snap instantly. |
| `duration_ms` | `u32` | `240` | Transition duration in milliseconds. |
| `easing` | `ConfigEasing` | `"ease-out-expo"` | Easing function name. See below for accepted values. |

Accepted `easing` values (one of the `ConfigEasing` enum variants):

- `"linear"`
- `"ease-in"`, `"ease-out"`, `"ease-in-out"`
- `"ease-in-quad"`, `"ease-out-quad"`, `"ease-in-out-quad"`
- `"ease-in-cubic"`, `"ease-out-cubic"`, `"ease-in-out-cubic"`
- `"ease-in-expo"`, `"ease-out-expo"`, `"ease-in-out-expo"`
- `"constant"` (jumps to the end value immediately — equivalent to `enabled = false` for the motion itself, but still goes through the animator)

Example:

```toml
[animation]
enabled = true
duration_ms = 240
easing = "ease-out-expo"
```

### `[minimize_restore]` — bringing minimized windows back

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `strategy` | `MinimizeRestoreStrategy` | `"original_slot"` | Where a minimized tiling window goes when restored. See below. |

Accepted `strategy` values:

- `"original_slot"` — Restore to the exact column/row it was minimized from. The column index is clamped to the strip's current column count (columns closed meanwhile move it towards the end); it rejoins a shared column at its row when that column still has room, else it gets a column of its own at its old width. A window minimized on a workspace that is no longer the visible one (or is gone) comes back in the visible workspace at that index; one whose monitor is gone is appended on its home monitor.
- `"right_of_focused"` — Restore into a new slot immediately to the right of the currently focused window.
- `"append_right"` — Append to the rightmost column.

With every strategy the view scrolls to the restored window (strip and book layout), and when Windows gives it the foreground (taskbar click, Alt+Tab) it is the focused window. A window that is re-shown after hiding itself, or recovered from maximized, is appended without a scroll. Why: `docs/adr/0027-restore-seat-and-frameless-corners.md`.

Example:

```toml
[minimize_restore]
strategy = "original_slot"
```

### `[navigation]` — strip edges

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `edge_hop` | `bool` | `true` | Whether `focus left/right` and `move-window left/right` at the end of a strip hop to the monitor in that direction (and `up`/`down` to a stacked monitor). `false` makes them stop at the edge — the command reports that there is nothing further and nothing moves. `focus-monitor` and `move-to-monitor` always cross monitors regardless. |

Example:

```toml
[navigation]
edge_hop = true
```

### `[drag]` — tile drag zones

How a tile dragged by its title bar or by Mod + left-button drag picks its
drop zone and scrolls the strip (see
[`drag begin` / `drag end`](./dispatch-reference.md#drag-begin--drag-end)).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `edge_scroll_width` | `i32` | `30` | Minimum band (pixels) at a screen edge that scrolls the strip; the whole band on an edge shared with another monitor. |
| `edge_scroll_ratio` | `f32` | `0.0833` | The screen-edge band as a fraction of the work-area width; also the height of the top / bottom row-scroll bands of a stacked column with off-screen rows. `0` = pixels only. |
| `col_edge_ratio` | `f32` | `0.33` | Fraction of a column's width, on each side, that drops the window as a new column beside it. |
| `col_edge_max_px` | `i32` | `0` | Pixel cap on that band; `0` = no cap. |
| `edge_scroll_initial_delay_ms` | `u32` | `500` | Gap from the first edge scroll to the first repeat; clamped up to the repeat interval. |
| `edge_scroll_repeat_interval_ms` | `u32` | `240` | Gap between column scrolls while the cursor stays in the band; clamped up to the animation duration. |

Example:

```toml
[drag]
edge_scroll_width = 30
edge_scroll_ratio = 0.0833
col_edge_ratio = 0.33
col_edge_max_px = 0
edge_scroll_initial_delay_ms = 500
edge_scroll_repeat_interval_ms = 240
```

### `[borders]` — window border overlays

Each managed window can get a thin colored ring drawn as a separate layered
overlay window that follows the target's geometry. This produces sharp
komorebi/Hyprland-style borders that the default Windows drop-shadow cannot.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | `bool` | `true` | Master switch. `false` = no overlay windows are created. |
| `thickness` | `u32` | `3` | Border ring width in pixels, applied on all four sides. |
| `overlap` | `u32` | `1` | Pixels the ring overlaps the window content. `0` = ring sits entirely in the gap; `thickness` = ring fills the slot edge-to-edge (closes the DWM hairline). |
| `focused_color` | `String` | `"#00AAFF"` | Border color for the focused/active window. Hex `#RRGGBB`. |
| `unfocused_color` | `String` | `"#555555"` | Border color for tiled-but-not-focused windows. Hex `#RRGGBB`. |
| `floating_color` | `String` | `"#AA00FF"` | Border color for floating windows. Hex `#RRGGBB`. |

The ring's corners are not configured: they follow the window. An app's own
corner preference (`DWMWA_WINDOW_CORNER_PREFERENCE`) is used as it is; a
window without one gets a rounded ring, or a square ring when it has no
caption and no sizing frame, or when its client area covers the whole window
(it draws its own frame, as Alacritty with `window.decorations = "none"`
does). `flow query all` reports the result per window as `border_corner`
(`default` = rounded, `square`, `rounded`, `rounded-small`; `null` without a
ring). Why: `docs/adr/0027-restore-seat-and-frameless-corners.md`.

Example:

```toml
[borders]
enabled = true
thickness = 3
overlap = 1
focused_color = "#00AAFF"
unfocused_color = "#555555"
floating_color = "#AA00FF"
```

### `[floating]` — floating windows

The two size fields are optional. When unset, the daemon falls back to a
built-in policy: 60% × 80% of the monitor work area, capped at roughly
1536 × 1152 (QHD-derived) so ultrawide / 4K monitors don't produce absurdly
large popups. An explicit pixel value below is always respected as-is — the
cap applies only to the fallback path.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_width` | `Option<u32>` | `None` | Explicit default width (pixels) for newly floated windows. |
| `default_height` | `Option<u32>` | `None` | Explicit default height (pixels) for newly floated windows. |
| `always_on_top` | `bool` | `false` | The **floating layer**, as in niri: floating windows always sit above the tiled windows, even when a tiled window has the focus, so a dialog is never hidden behind the strip and never has to be hunted for. Applies to every floating window and to every window your rules ignore (`Ignored::ExplicitRule` — dialogs, popups, File Explorer through a bundled rule); not to windows ignored because they are maximized or fullscreen, nor to minimized or hidden ones (they are raised again when restored or shown). Implemented with Windows' "topmost" band (`WS_EX_TOPMOST`): a window leaves it when it is tiled, minimized, hidden or put fullscreen (`set-window fullscreen` keeps its own window topmost; a float and a fullscreen window then stack by whichever was activated last), on `flow dispatch tiling off`, on `flow stop` and in a fail-back; a window that was already "always on top" on its own keeps that flag. If an app clears the flag itself, FlowWM puts it back within a quarter second. Notification toasts are topmost anyway. `flow query all` reports `on_top` per window. `false` = a focused tile covers the floats (stock behaviour). |

Example:

```toml
[floating]
default_width = 1200
default_height = 800
always_on_top = true
```

### `[focus]` — foreground reconciliation and new windows

`EVENT_SYSTEM_FOREGROUND` is a best-effort stream from Windows: under rapid
window churn (e.g. a browser closing multiple tabs), the OS can settle the
foreground without emitting the final event, leaving flow's internal focus
stuck on a stale window. The daemon closes that gap by periodically
reconciling its tracked focus against the authoritative `GetForegroundWindow()`
query.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `foreground_sync_interval_ms` | `u32` | `250` | Interval (milliseconds) between reconciliation passes. The common in-sync pass is a single microsecond-scale read that no-ops. |
| `new_window_monitor` | `"focused"` \| `"placed"` | `"focused"` | The screen a new window opens on. `"focused"` (niri behaviour) seats it on the screen that has the focus, at that screen's width preset, whatever screen Windows put it on; `"placed"` keeps the screen Windows chose. Windows found when the daemon starts and windows coming back from minimize or the tray keep their own screen either way. A window that first shows up by taking the foreground (one flow could not classify earlier, now clicked) also stays where it is. Why: `docs/adr/0023-new-windows-open-on-the-focused-screen.md`. |

Example:

```toml
[focus]
foreground_sync_interval_ms = 250
new_window_monitor = "focused"
```

Closing or minimizing the focused tile hands the focus to its successor on
the same screen, also when Windows has already given the foreground to a
window on the other screen; when it was the last tile of that screen the
screen stays the active one and the focus goes to the desktop, so the next
new window opens there.

### `[loadout]` — save/restore file

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_path` | `String` | `"loadout.json"` | The file `flow loadout save` / `flow loadout load` use when no path is given, and the one saved on `flow stop` and restored at start. A **plain file name inside the config directory**: no directories, no `..`, no drive letter. Anything else is refused when the config is loaded (`flow start`, `flow config reload`, `flow config check` all report `[loadout] default_path must be a plain file name …`). |

An explicit `flow loadout save <path>` / `load <path>` must name a `.json`
file and may not contain a `..` component; a relative path is taken
relative to the config directory (not the directory you ran `flow` from),
an absolute path is used as given.

Example:

```toml
[loadout]
default_path = "loadout.json"
```

### `[logging]` — log retention

The daemon writes one `logs/flowd-YYYY-MM-DD.log` per day under the config
directory. At the default level the log records every window it manages by
handle, executable and window class; **window titles are only logged at
`debug` level** (`flow start --verbose`), since they name your documents and
conversations. The saved loadout file records executables and handles,
never titles.

The keymap the daemon starts (`flow start --ahk`) can write its own trace
next to it, `logs/flow-ahk-YYYY-MM-DD.log`: every dispatch and its exit code,
the Remote Desktop hook re-installs, and the launcher's queries with the name
of the top row (an app, a file or a window title). Because of those queries
the trace is **off by default**; `keymap_log = true` turns it on from the next
daemon start. Setting `FLOW_AHK_LOG` to a path in the environment also turns
it on, writes there instead, and wins over `keymap_log` either way.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `keep_days` | `u32` | `14` | How many days of daily log files to keep. At every start the daemon deletes `logs/flowd-*.log` and `logs/flow-ahk-*.log` files whose date (in the file name) is older than this many days. `0` keeps every file forever. Files with other names are never touched. |
| `keymap_log` | `bool` | `false` | Write the keymap's trace to `logs/flow-ahk-YYYY-MM-DD.log`: the daemon sets `FLOW_AHK_LOG` for the script it launches, unless `FLOW_AHK_LOG` is already set in its environment. Read when the daemon launches the keymap, so a change takes effect at the next `flow start --ahk`. |

Example:

```toml
[logging]
keep_days = 14
```

### `[keymap]` — keymap options

Options of the hotkey script (`keymaps/win-niri`) that are not key
bindings. The daemon only carries them to the keymap, in the same
`flow query binds` listing the chords come from (an `option` record in
`--plain`), so a saved change applies within a few seconds.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `selection_buffer` | `bool` | `false` | The X11-style selection buffer (`selection.ahk`). Text highlighted with the mouse (a drag, a double or triple click) is read through UI Automation and kept apart from the clipboard; a middle click on a text field or a terminal pastes it, and the clipboard is put back afterwards. Elsewhere (a web page, a link, a button, a password field, Remote Desktop) the middle click is passed on unchanged. Apps that draw their own text give nothing to capture; Alacritty is captured through its copy-on-select and needs `[selection] save_to_clipboard = true` in its own `alacritty.toml` (the clipboard is put back after the read). The overlay (Mod+Shift+/) shows a *Middle click* row while it is on. Details and limits: the keymap README, *Selection buffer*. |

Example:

```toml
[keymap]
selection_buffer = true
```

### `[update]` — update checker

`flow update` is a **checker, never an installer**: it asks GitHub how this
build relates to a branch (by the commit the binary was built from — see
`flow --version`) or to the latest release, prints the answer and the recipe
for updating, and exits `0` when nothing is newer, `10` when something is.
It never downloads or replaces `flow.exe` / `flowd.exe`. The same checker
runs at `flow start` when the top-level `check_for_updates` is `true`. See
[Update Checker](../dev-guide/updater.md) for the mechanics and the full
exit-code table.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `repo` | `String` | *(absent)* | GitHub repository as `"owner/name"`. When absent it is derived from the `origin` remote of the checkout the binary was built from, if that is a github.com URL (https or ssh form); otherwise the upstream `CCpcalvin/flow-wm`. Anything that is not `owner/name` is refused with exit code 3. |
| `branch` | `String` | *(absent)* | Branch to compare the build's commit against. When absent, the branch that was checked out at build time (shown by `flow --version`). Set it to the **empty string** `""` to compare the crate version against the repository's latest **release** instead; a build with no branch at all (detached `HEAD`, not a git checkout) also uses release mode. |

Both keys are optional; so is the whole table. A build that is not from a git
checkout has no commit to compare and must use release mode.

Example:

```toml
[update]
repo = "yourname/flow-wm"    # your own fork on GitHub
branch = "main"              # or "" to compare against releases
```

---

### `[binds]` — key bindings

The chords, in niri's spelling, laid over the keymap's built-in table
(`keymaps/win-niri/binds.toml` in the repository, compiled into the daemon).
Without a `[binds]` section you get that table; with one, each entry
replaces the default on the same chord or adds a new one, and `"none"`
unbinds a default. The daemon validates the table like the rest of the
file and hands it to the hotkey script, which registers the chords at
runtime and builds the Mod+Shift+/ overlay from it — so the overlay always
shows what is bound. See `flow query binds` for the resolved list.

```toml
[binds]
# defaults = false                         # start from an empty table instead of the built-in one
"Mod+Shift+R" = "switch-preset-column-width-back"   # add a chord
"Mod+Q" = "none"                           # unbind a default
"Mod+Shift+Q" = "close-window"             # ... and put it elsewhere
"Mod+Return" = { spawn = ["alacritty"] }   # run a program
"Mod+Shift+Return" = { spawn = ["wt", "-p", "PowerShell"], admin = true, help = "admin shell" }
"Mod+Shift+WheelScrollDown" = "focus-column-right"
"Mod+X" = "dispatch width 1/2"             # anything `flow dispatch` accepts
```

**Chords.** `Modifier+...+Key`, any order, any case; the last word is the
key. Modifiers: `Mod` (also `Super`, `Win` — the Windows key), `Ctrl`
(`Control`), `Alt`, `Shift`. Keys: a letter, a digit, `F1`..`F24`, one of
`Left Right Up Down Home End Page_Up Page_Down Insert Delete BackSpace
Return Tab Escape Space Print Pause Minus Equal BracketLeft BracketRight
Semicolon Apostrophe Grave Backslash Comma Period Slash WheelScrollUp
WheelScrollDown WheelScrollLeft WheelScrollRight`, or a raw AutoHotkey
`vkXX` / `scXXX` code. A bare key needs a modifier (`Print` and `Pause`
excepted); mouse buttons are not configurable. `Mod+Ctrl+T` and
`Mod+Ctrl+Shift+T` are reserved by the keymap (FlowWM off/on, tiling
pause), as is Mod + left-button drag. Two spellings of one chord in the
same table are an error.

**Actions.** A string is an action line; a table is a program to run.

| Action | What it runs | |
|---|---|---|
| `focus-column-left` / `-right`, `focus-window-up` / `-down` | `flow dispatch focus <dir>` | |
| `move-column-left` / `-right`, `move-window-up` / `-down` | `move-window <dir>` | |
| `focus-monitor-left` / `-right` / `-up` / `-down` | `focus-monitor <dir>` | |
| `move-column-to-monitor-<dir>` (also `move-window-to-monitor-<dir>`) | `move-to-monitor <dir>` | |
| `focus-workspace N` | `switch-workspace N` | N from 1 |
| `move-column-to-workspace N` (also `move-window-to-workspace N`) | `move-to-workspace N` | |
| `close-window` | `close-window` | |
| `minimize-window` | `minimize-window` | the focused tile or float leaves the layout, the focus moves to its successor on the same screen; a restore (taskbar, Alt+Tab) puts it back in its slot and scrolls it into view (`[minimize_restore]`); Mod+M by default |
| `center-column` | `center` | |
| `toggle-window-floating`, `move-window-to-floating`, `move-window-to-tiling` | `set-window cycle` / `float` / `tile` | |
| `switch-focus-between-floating-and-tiling` | `switch-focus-between-floating-and-tiling` | focus the other side: the float used last (else the top one), or the strip's last tile; Mod+Shift+V by default |
| `focus-floating`, `focus-tiling` | the same names | one side explicitly; an empty side is a no-op |
| `fullscreen-window` | `set-window fullscreen` | |
| `toggle-layout` | `set-layout toggle` | book / strip layout on the focused window's screen until the next reload; Mod+Shift+B by default |
| `switch-preset-column-width`, `switch-preset-column-width-back` | `width next` / `width prev` | |
| `set-column-width "+10%"` / `"-10%"` | `expand-column` / `shrink-column` | one step either way; refused on a width pattern |
| `consume-or-expel-window-left` / `-right` | `consume-or-expel <dir>` | |
| `consume-window-into-column` | `merge-column right` | nearest equivalent: the focused window joins the column to its right |
| `expel-window-from-column` | `promote right` | |
| `dispatch <args>` | `flow dispatch <args>` | anything the CLI accepts |
| `launcher` | the keymap's application launcher | |
| `show-hotkey-overlay` | the key-binding overlay | |
| `toggle-tiling` | `tiling toggle` with the keymap's message | Mod+Ctrl+Shift+T does this and is reserved |
| `quit` | `flow stop` and the keymap exits | |
| `none` | unbinds the chord | |
| `{ spawn = ["prog", "arg", ...], admin = false, help = "..." }` | runs the program | see below |

Window-management actions are live while the daemon runs *and tiling is
on*; `launcher`, `show-hotkey-overlay`, `toggle-tiling`, `quit` and `spawn`
whenever the daemon runs. The same names work on the command line:
`flow dispatch focus-column-left` is accepted as an alias of `flow dispatch
focus left`.

**Programs.** `spawn` takes the program and its arguments. A bare name
(`"alacritty"`, `"wt"`, `"notepad"`) is looked up in `%ProgramFiles%\<name>\<name>.exe`
(and the x86 folder), `%LOCALAPPDATA%\Programs\<name>\<name>.exe`, the
Store's execution aliases (`%LOCALAPPDATA%\Microsoft\WindowsApps\<name>.exe`)
and then each `PATH` entry — never the folder FlowWM was started from. A
path must be absolute. `admin = true` runs it elevated (UAC prompts as
usual); `help` replaces the overlay's `run <program>` text. Programs start
in your home folder.

A spawn bind without `admin` is started by the daemon (`flow spawn`), which
asks the program's first window to open on the focused screen, at the place
and size its new column will get (`STARTF_USEPOSITION | STARTF_USESIZE`), so
it does not appear on the other screen first. This works for programs that
let Windows choose their first window's position (Alacritty, Windows Forms
apps and most others); the size only for programs that also let Windows
choose the size (Alacritty picks its own). A program that places itself
(a remembered position, a single-instance app, a launcher stub such as the
Store's `wt` alias) opens where it wants and is then moved to the focused
screen. `admin = true` spawns go through the UAC path, which takes no position,
so they can still show on the other screen for a moment. Only `.exe` programs
get a position. When the daemon does not answer, the keymap starts the program
itself. `flow spawn <program> [args...]` does the same from a shell. With
`[focus] new_window_monitor = "placed"` or tiling off no position is passed.

**Where to look.** `flow query binds` prints the resolved table as JSON
(`flow query binds --plain` is the listing the keymap reads); the overlay
(`show-hotkey-overlay`, Mod+Shift+/ by default) shows it on screen;
`flow config check` names every bad entry (`binds."Mod+Bogus": unknown key
"Bogus" (...)`). Help texts and chord names are ASCII so the overlay renders
the same everywhere.

### `[startup]` — programs to run

Programs the daemon starts once it is ready, in order, each as the program
and its arguments. A bare name is looked up like a `spawn` bind (above); a
program that cannot be started is logged and the rest still run. They start
in your home folder, open on the focused screen like a spawn bind, and are not
tied to the daemon's lifetime.

```toml
[startup]
spawn = [["alacritty"], ["wt", "-p", "PowerShell"]]
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `spawn` | `Vec<Vec<String>>` | `[]` | One entry per program: `["program", "arg", ...]`. An entry without a program name is a config error. |

Applies to the next daemon start; it is not re-run on reload.

## `flow-rules.toml`

Window classification rules — which windows FlowWM should tile, float, or
ignore entirely. Rules are evaluated **top-to-bottom, first match wins**. If no
rule matches, `default_action` is used.

Schema:
[`schemas/flow-rules.schema.json`](https://github.com/CCpcalvin/flow-wm/blob/master/schemas/flow-rules.schema.json).

> FlowWM ships a set of built-in defaults for common Windows system windows
> (taskbar, Search UI, system dialogs, Task Manager, on-screen keyboard,
> Chromium legacy windows, etc.). These are embedded into the binary at
> compile time — they cannot be deleted or corrupted at runtime — and your
> `flow-rules.toml` is checked **first**, so your rules always outrank the
> built-in ones. See
> [`default-flow-rules.toml`](https://github.com/CCpcalvin/flow-wm/blob/master/default-flow-rules.toml)
> in the repo root for the full list of what's ignored by default.

### Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `default_action` | `WindowAction` | `"float"` | Action applied when no rule matches a new window. |

Accepted `default_action` values:

- `"tile"` — New windows enter the scrolling column layout.
- `"float"` — New windows float freely (FlowWM's default — most apps are written to float).
- `"ignore"` — FlowWM pretends the window doesn't exist. No border, no layout slot, no focus tracking.

### `[[rules]]` — individual rules

Each rule is a table in the `rules` array. Every rule has a `match` clause
(fields AND'ed together) and an `action`, an option such as `fullscreen`, or
both. A rule without an `action` only carries its options: classification
skips it and goes on to the next rule.

#### Match fields

All match fields are optional. Multiple fields on the same rule are combined
with AND logic — all must match.

| Field | Type | Description |
|-------|------|-------------|
| `exe` | `String` | Executable name to match (e.g. `"chrome.exe"`). Case-insensitive on the filename. |
| `exe_regex` | `String` | Regex on the executable name. Case-insensitive unless it starts with `(?-i)`. |
| `process_path` | `String` | Exact full executable path. Case-insensitive. |
| `process_path_regex` | `String` | Regex on the full executable path. Case-insensitive unless it starts with `(?-i)`. |
| `title` | `String` | Exact window title. Case-sensitive. |
| `title_contains` | `String` | Substring of the window title. Case-sensitive. |
| `title_regex` | `String` | Regex on the window title. Case-sensitive unless it uses `(?i)`. |
| `class` | `String` | Exact Win32 window class name. Case-sensitive. Often the most reliable identifier — see note below. |
| `class_regex` | `String` | Regex on the window class name. Case-sensitive unless it uses `(?i)`. |

> **Prefer `class` over `exe` + `title` for system helpers.** The class is
> assigned at window creation, while the title may be set a few milliseconds
> later. Class matching is therefore race-free; title matching can miss a
> window classified at creation time before its title arrives. This is why the
> bundled defaults match `Chrome_RenderWidgetHostHWND` by class — a single
> rule covers every Chromium host (Chrome, Edge, VS Code, Electron apps…)
> regardless of exe name.

#### Action field

Each rule's `action` is one of the `WindowAction` values (`"tile"` / `"float"`
/ `"ignore"`), same as `default_action`.

#### `fullscreen` field

How `set-window fullscreen` (Mod+F) is done for a matching window:

- `"frame"` (default) — FlowWM sizes the window's frame over the monitor's
  whole screen; the app is not told and keeps its caption.
- `"app"` — FlowWM asks the app for its own full screen, as its title-bar
  maximize button would, and leaves the rect to the app. The app's own
  maximize and restore are followed. See
  [`set-window fullscreen`](dispatch-reference.md#set-window-fullscreen).

The option is looked up on its own: the first rule, over your rules, then the
learned rules, then the bundled ones, that matches **and sets** `fullscreen`
decides it. A learned rule never sets it, so learning that an app tiles does
not undo it. The bundled rules set `"app"` for Remote Desktop:

```toml
[[rules]]
match = { exe = "mstsc.exe", class = "TscShellContainerClass" }
fullscreen = "app"
```

Put `fullscreen = "frame"` on a rule for the same window in your
`flow-rules.toml` to go back to the frame. In its own full screen Remote
Desktop sends the Windows key combinations to the remote session (the
`.rdp` setting `keyboardhook:i:2`, its default); the bundled keymap
(`flow.ahk`) puts its keyboard hook back in front whenever a Remote Desktop
full screen becomes active, so the FlowWM chords keep working there and
every other Windows key combination still reaches the session.

### Example

```toml
#:schema ./schemas/flow-rules.schema.json

default_action = "float"

# Tile my terminal and editor unconditionally
[[rules]]
match = { exe = "WindowsTerminal.exe" }
action = "tile"

[[rules]]
match = { exe = "Code.exe" }
action = "tile"

# Float the Steam friends list
[[rules]]
match = { exe = "steamwebhelper.exe", title = "Friends List" }
action = "float"

# Ignore a misbehaving helper window from some app
[[rules]]
match = { class = "SomeHelperClass" }
action = "ignore"
```

### How classification actually works (short version)

When a new window appears, FlowWM checks, in order:

1. **User rules** — your `flow-rules.toml`, top-to-bottom, first match wins.
2. **Learned rules** — machine-written to `history-flow-rules.toml`. Whenever
   you tile a floating window (or float a tiled one) with `set-window` —
   `Alt + T` in upstream's `flow.ahk`, Mod+V in the win-niri keymap —
   FlowWM records your choice and applies it automatically next time you open
   that app.
3. **Bundled defaults** — `default-flow-rules.toml`, embedded in the binary.
4. **`default_action`** — if nothing above matched.

You never edit `history-flow-rules.toml` directly; it's managed by the daemon.
To "forget" a learned choice, delete the file (the daemon recreates it empty on
the next launch) or add an explicit rule for that app in `flow-rules.toml` —
user rules always outrank learned rules. For the full algorithm, see
[Classification & Learned Rules](../dev-guide/classification.md) in the
Developer Guide.

---

## Config directory resolution

The directory FlowWM reads for `flow.toml` and `flow-rules.toml` resolves in
this priority order:

1. `--config <dir>` flag on the CLI (highest priority)
2. `FLOW_CONFIG_DIR` environment variable
3. Default: `%USERPROFILE%\.config\flow\`

The `logs/` folder (where the daemon writes its date-stamped log) lives inside
the config directory, so overriding the config dir moves the logs too.

For the full lifecycle — init, load, validate, use, hot-reload — see
[Config & Persistence](../dev-guide/config-and-persistence.md) in the
Developer Guide.
