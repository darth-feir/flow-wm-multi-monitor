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
| `columns.presets`, `columns.default`, `columns.screen` | Each monitor's preset is re-resolved against its list: it stays on the preset it was on when the list still has it, else lands on that list's `default` (so a monitor that had no presets before starts on `default`). Existing per-column widths preserved for a plain fraction; a pattern re-pins them by position. A list with a term under the floor on any attached screen is refused and the old config kept. |
| `columns.visible_rows` | Every column re-shares its height over the new viewport size and the focused row is kept visible (windows animate). |
| `columns.max_rows` | Applies to the next merge/consume/insert; a column already over the new cap keeps its rows until windows leave it. |
| `animation.*` | New duration / easing / enabled flag applies to the next animation. |
| `navigation.edge_hop` | Applies to the next `focus` / `move-window` at a strip edge. |
| `adopt_maximized` | Applies to the next maximized window the daemon meets (a window already parked as ignored is adopted when it is next focused, shown or moved). |
| `floating.always_on_top` | Immediate: turned on, every floating window and every rule-ignored window is raised above the tiles; turned off, they go back to the normal z-order (a window that was "always on top" by itself keeps that). |
| `binds.*` | The daemon re-resolves the table; the hotkey script re-reads it a few seconds after the save and re-registers the chords (one-line message). A table with an error keeps the old chords. |
| `startup.*` | Applies to the next daemon start only. |
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
| `check_for_updates` | `bool` | `false` | Whether `flow start` runs the update checker (the same one behind `flow update`, configured by [`[update]`](#update--update-checker)) and prints a one-line notice when something newer exists on GitHub. **Off by default in this fork** — nothing is contacted at start unless you opt in with `true`. When on it runs after the daemon is ready, is bounded by a 5 s timeout, stays silent on any failure, never blocks startup and never downloads anything. The explicit `flow update` command is unaffected by this flag. |
| `adopt_maximized` | `bool` | `false` | Whether a window that is **maximized** when FlowWM meets it — in the startup scan, when it opens, when tiling is turned back on — is un-maximized and classified through your rules (tiled or floated like any other window) instead of being left alone as "ignored: maximized" until you restore it. Fullscreen windows (video, games) are never adopted, nor is a window your rules ignore. An adopted window is maximized again when FlowWM hands the desktop back: `flow dispatch tiling off`, `flow stop`, a fail-back. With `false` (the stock behaviour) a maximized window stays ignored until you restore it or run [`set-window`](./dispatch-reference.md#set-window-mode) on it, which adopts it either way. |

Example:

```toml
columns_per_screen = 4
# column_width = 1280          # optional fixed width; ignores columns_per_screen
min_column_width_px = 640
min_window_height_px = 100
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
| `presets` | `Vec<String>` | `["1/3", "1/2", "1/1"]` | Ordered presets. Each is a fraction `"a/b"` of the work area, a plain number in `(0, 1]` such as `0.5`, or a `+` pattern of those (`"1/3+2/3"`, see below). Must not be empty. |
| `default` | `Option<String>` | `None` (= the first preset) | The preset every monitor starts on. Must be one of `presets`. |
| `visible_rows` | `u32` | `0` | How many rows of a column are on screen at once. `0` shows every row sharing the height. With `2`, a column of four windows shows two at a time and `focus up/down` scrolls through the stack. |
| `max_rows` | `u32` | `0` | The most rows a column may hold; `0` is unlimited. Must be at least `visible_rows` when both are set. Merging into a full column moves the window past it instead. |
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

#### Width patterns: `"1/3+2/3"`

A preset with two or more terms joined by `+` is a **pattern**. The columns
of the strip take the terms **by position**, repeating: with `"1/3+2/3"` the
first, third and fifth columns are a third of the work area and the second,
fourth and sixth two thirds; `"1/4+1/2+1/4"` repeats every three columns.
The terms of a pattern may add up to at most the whole screen (`1/1`), so a
whole period plus its gaps fits the work area exactly and scrolling by one
column shows the same shares in the other order. A single fraction is the
one-term pattern.

Widths never change with focus or scrolling. They are decided by position,
so inserting a column (a new window, a merge, a swap) re-flows the columns
after it, and so a pattern **pins** the strip: `expand-column`,
`shrink-column`, `set-column-width` and `toggle-monocle` are refused on that
screen with a message naming the pattern, and a mouse resize snaps back on
release. Switch to a single-fraction preset to resize by hand. Why patterns
are positional rather than anchored on the focused column:
`docs/adr/0014-width-patterns-by-column-position.md`.

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
| `presets` | `Vec<String>` | The list for those screens; same grammar as the top-level `presets`. |
| `default` | `Option<String>` | The preset such a screen starts on; one of the rule's `presets`. |
| `min_column_width_px` | `Option<u32>` | A floor for those screens instead of the top-level one, which is screen-blind. |

Every field of `match` that is given must hold, and at least one must be
given. Orientation and width survive display renumbering, so prefer them
over `name` for everything but a specific display.

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
`row_offset` (the first visible row) and `visible_rows`, and every window's
`parked_row`.

### `[padding]` — spacing

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `window_gap` | `u32` | `16` | Uniform gap (pixels) between all elements: window-to-window, window-to-screen-edge. |
| `up` | `u32` | `0` | Reserved space (pixels) above the tiling area. |
| `down` | `u32` | `0` | Reserved space (pixels) below the tiling area — e.g. for taskbar clearance. |

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

- `"original_slot"` — Restore to the exact column/row it was minimized from.
- `"right_of_focused"` — Restore into a new slot immediately to the right of the currently focused window.
- `"append_right"` — Append to the rightmost column.

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

### `[focus]` — foreground reconciliation

`EVENT_SYSTEM_FOREGROUND` is a best-effort stream from Windows: under rapid
window churn (e.g. a browser closing multiple tabs), the OS can settle the
foreground without emitting the final event, leaving flow's internal focus
stuck on a stale window. The daemon closes that gap by periodically
reconciling its tracked focus against the authoritative `GetForegroundWindow()`
query.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `foreground_sync_interval_ms` | `u32` | `250` | Interval (milliseconds) between reconciliation passes. The common in-sync pass is a single microsecond-scale read that no-ops. |

Example:

```toml
[focus]
foreground_sync_interval_ms = 250
```

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

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `keep_days` | `u32` | `14` | How many days of daily log files to keep. At every start the daemon deletes `logs/flowd-*.log` files whose date (in the file name) is older than this many days. `0` keeps every file forever. Files with other names are never touched. |

Example:

```toml
[logging]
keep_days = 14
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
| `center-column` | `center` | |
| `toggle-window-floating`, `move-window-to-floating`, `move-window-to-tiling` | `set-window cycle` / `float` / `tile` | |
| `switch-focus-between-floating-and-tiling` | `switch-focus-between-floating-and-tiling` | focus the other side: the float used last (else the top one), or the strip's last tile; Mod+Shift+V by default |
| `focus-floating`, `focus-tiling` | the same names | one side explicitly; an empty side is a no-op |
| `fullscreen-window` | `set-window fullscreen` | |
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
in your home folder and are not tied to the daemon's lifetime.

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
(fields AND'ed together) and an `action`.

#### Match fields

All match fields are optional. Multiple fields on the same rule are combined
with AND logic — all must match.

| Field | Type | Description |
|-------|------|-------------|
| `exe` | `String` | Executable name to match (e.g. `"chrome.exe"`). Case-insensitive on the filename. |
| `title` | `String` | Window title substring to match. |
| `class` | `String` | Win32 window class name to match. Often the most reliable identifier — see note below. |

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
   you press `Alt + T` on a floating window (or float a tiled one),
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
