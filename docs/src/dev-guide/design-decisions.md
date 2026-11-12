# Design Decisions

This chapter collects the major "why not X" trade-offs in one place. Each decision
is presented with the chosen approach, the main alternative considered, and the
rationale for the choice.

## Single Cargo Package (Not a Workspace)

`flow` is a single Cargo package with two binaries (`flowd`, `flow`)
sharing one library crate (`src/lib.rs`). Rust supports this layout natively — one
package can contain a library crate, a default binary, and additional binaries.

The alternative was a multi-crate workspace from the start. That was rejected
because the internal subsystem boundaries are still evolving. Module boundaries
within a single package are sufficient for the current scale, and keeping
everything in one package avoids Cargo configuration overhead and makes refactoring
across module boundaries trivial.

Extraction to a separate crate will happen only when a concrete reason appears:
the code is reusable outside `flow`, compile times become a real bottleneck, or
the API has stabilized and deserves a stronger boundary. Likely future
extraction candidates are `src/animation` (if the animation system becomes
reusable elsewhere) and `src/ipc` (if external tools need a standalone client
crate). Internal modules like `registry`, `layout`, and `workspace` should stay
where they are until a strong reason to move them emerges.

## Pixel Widths (Not Proportional Eighths)

Column widths in the virtual layout are stored as absolute pixel values
(`width_px: i32`), not as proportional units (e.g. eighths of a base column
width).

An earlier revision used eighths to stay resolution-independent. This caused a
gap-loss bug: `expand_column` computed the correct pixel target
(`column_width + window_gap`), then `pixels_to_eighths` re-quantized onto the
base grid, discarding the gap because it was smaller than one eighth and rounded
away. Each expand step grew by exactly `column_width` instead of
`column_width + window_gap`.

Pixel widths make the gap observable at every step and fix the bug. The cost is
dependence on the configured column width and monitor resolution — accepted because
`window_gap` is already pixel-based everywhere else and resolution independence
was never a real requirement (the target is Windows desktop, not cross-platform).

## No `Arc<Mutex>` (Single-Thread Ownership)

The daemon uses single-thread ownership: `FlowWM` lives entirely on
the IPC thread, and all subsystem methods take `&mut self`. There is no
`Arc<Mutex<T>>` anywhere in the daemon.

The previous architecture used `Arc<Mutex<WindowRegistry>>` because hook events
and IPC commands were consumed by different parts of the code with no single
coordination point. The refactor to a single orchestrator eliminated that need.
The borrow checker now enforces exclusive access at compile time, which is
strictly safer than `Mutex` (runtime-only enforcement, potential deadlocks). The
hook thread communicates exclusively through an `mpsc` channel and never touches
daemon state.

See [Threading Model](./threading-model.md) for the full model.

## TOML Config (Not YAML, JSON, or Lua)

Configuration uses TOML. The alternatives were YAML (indentation-sensitive, complex
spec), JSON (no comments, verbose), and Lua (fully programmable but requires a
runtime and steep learning curve).

TOML's clean syntax, comment support, and good fit for flat/nested config made it
the clear choice. Lua remains a future option if users need conditional logic
(e.g. per-monitor layouts), but TOML covers all current use cases.

The config system uses a two-layer merge model: compiled-in `Default` impls
provide the base, and the user's `flow.toml` overlays on top. Serde's
`#[serde(default)]` at the container level means a user's config can be partial,
empty, or nested-partial — serde fills the gaps from the `Default` impl.

## Config Defaults: Code Is the Single Source of Truth

Default values for every config field live in the `Default` impls of each config
struct in `src/config/types.rs`. Each struct carries
`#[serde(default)]` at the container level, so a user's `flow.toml` may be partial
or empty.

`default-config.toml` in the repo root is a hand-written **example** file, copied
to users by `flow config init`. It is **not** read at runtime — the compiled
defaults are authoritative. When a developer adds or changes a config field,
they must update both the `Default` impl and `default-config.toml`. A test
(`default_config_toml_matches_compiled_defaults`) enforces they stay in sync.

This avoids the fragility of "file A is the source of truth except when it's
missing" and the complexity of a two-layer TOML merge with a shipped file at
runtime.

## Separate `WindowRegistry` and `ScrollingSpace`

The window registry (`src/registry`) and the tiling engine
(`ScrollingSpace` in `src/workspace`) are separate components with a clear
purity boundary.

The registry owns all window metadata: HWND-to-`WindowId` mapping, titles,
classes, classification state (tile/float/ignore), and invisible bounds. It
bridges the layout engine to Win32 — it is the only place that holds HWND
references.

`ScrollingSpace` owns the layout math: virtual layout, focus state, column
widths, viewport offset. It operates exclusively on `WindowId` values and never
sees HWNDs. The pure layout computation in `src/layout/` is even more isolated —
it has zero Win32 dependencies.

This separation means the layout math is fully unit-testable on any platform and
can be reasoned about without considering Win32 quirks. The daemon is the only
code that shuttles data between the two subsystems.

## `SetWindowPos` Over `DeferWindowPos`

Window positioning uses `SetWindowPos` rather than the batched `DeferWindowPos`
API.

`DeferWindowPos` batches multiple position changes and triggers a single repaint
at the end — useful for UI frameworks that own all the windows they move. In
`flow`'s case, not all windows are deferrable (some apps reject deferred
positioning), and each application owns its own rendering pipeline. `SetWindowPos`
applies the position change immediately, which is what the animation system
expects when tweening individual window rects frame by frame.

The deciding reason, recorded in `src/animation/backend/win32.rs`, is UIPI:
the deferred batch is atomic, so a single window of a higher-integrity
process (an elevated app, a UAC prompt) makes `EndDeferWindowPos` fail with
`ERROR_ACCESS_DENIED` and nothing moves. Per-window calls fail one window at
a time. Since ADR-0019 every call on a managed window goes through
`place_window` in that file, which picks the `SetWindowPos` flags (pure move,
silent parked resize, plain resize); since ADR-0025 elevated windows are not
managed at all, so the refused calls are not sent in the first place.

## `WindowId` as the Platform-Independent Bridge Type

`WindowId` (currently `pub struct WindowId(pub isize)`, wrapping the raw HWND
value) is the bridge type between the registry and the layout engine.

The layout engine only ever sees `WindowId` — it never knows about HWNDs. This
keeps the layout math platform-independent and unit-testable. The registry is the
only component that holds the HWND-to-`WindowId` mapping and performs Win32 calls.

The `isize` wrapping exists because `HWND` is `!Send` in windows-rs (it wraps a
raw pointer), but `WindowId` must be `Send` to cross thread boundaries (e.g. the
hook callback sends `HookEvent` variants such as `HookEvent::Created { hwnd: isize }`
through the `mpsc` channel). The
raw integer value is just a kernel handle — it is safe to share across threads.

## Keybindings Removed, Delegated to External Tools

Keybindings were removed from `flow`. The daemon accepts IPC commands over a named
pipe, and users are expected to configure external tools (AutoHotkey, PowerToys,
etc.) to send those commands.

The original design had a built-in `InputInterceptor` with Super-key capture and
configurable hotkey bindings. This was removed because:

- It required intercepting global input, which conflicts with other tools the
  user may already have.
- A dedicated keybinding tool gives users more flexibility (per-application
  rules, macros, chords) than any tiling manager's built-in binding system.
- It keeps `flow` focused on what it does well: window management and layout.

The IPC protocol surface (focus, swap, scroll, resize, etc.) is fully defined and
stable — any external tool that can write JSON to a named pipe can drive `flow`.

The fork keeps the daemon out of global input but moves the binding *table*
back into configuration: ADR-0015 adds a `[binds]` table to `flow.toml` that
the daemon parses, validates and lists (`flow query binds`), and the
AutoHotkey keymap (`keymaps/win-niri`) registers the hotkeys from that list.
The daemon still intercepts no input; see [Key Bindings](./keybinds.md).

## Loadout Window Identity: HWND-Exact (Not Fuzzy Matching)

The loadout feature (saving and restoring workspace arrangements across
daemon restarts) identifies windows by their **Win32 `HWND`**, stored directly
in the loadout file. On load, each saved slot is matched to the live window with
the identical HWND. If any saved HWND is not currently live, the **entire** load
aborts and the daemon falls back to its fresh init layout — a strict no-partial
guarantee.

The alternative considered was **fuzzy similarity matching**: score each candidate
live window on a combination of exe, class, title, and HWND, then assign the
best-scoring window to each slot. This was rejected for three reasons.

First, the loadout's job is **resilience, not desired-state declaration**. Its
purpose is to recover the exact arrangement that existed when the daemon was
stopped, restarted, or crashed — a window of seconds during which the target
applications keep running and their HWNDs are stable. HWND is therefore a
unique, exact, unambiguous key for the only case that actually matters. The
"save a canonical layout and restore it after a reboot" use case is better
served by classification config rules (declarative and durable) than by a
snapshot tied to specific window instances.

Second, fuzzy matching is **information-theoretically unsolvable for the very
windows that motivated the feature**. Windows Terminal instances share one
executable and one window class, and their titles (which encode the active
tab or working directory) are volatile. After a reboot, several identical
Terminal windows are genuinely indistinguishable — their original slot
assignments are lost forever. HWND is the only signal that could disambiguate
them, and across a reboot HWND is meaningless. Fuzzy matching would therefore
offer to "restore" a layout it cannot restore correctly, failing silently.

Third, fuzzy matching **destroys the no-partial guarantee and drags in a
layout-simplification algorithm**. "Best match" always returns a candidate, so
a slot whose window is genuinely absent would be silently force-paired with the
least-bad survivor, misplacing windows. Applying a partial layout well (skip the
missing slot, collapse the gap) requires recomputing column and row geometry —
real layout work for a case that, in the resilience window, essentially never
occurs, because target applications are independent processes that do not close
during a daemon restart. Aborting the whole load on any missing HWND avoids both
the silent-misplacement danger and the simplification algorithm entirely.

Stored alongside the HWND is `exe`, retained as a **diagnostic-only** field:
it makes the loadout file self-describing at the moment a restore fails (a
missing window's identity is only known at save time, so it must be persisted
to be useful at load time). It is never consulted by the matcher. The window
`class` is dropped as low-value (opaque Win32 identifiers of no use to a human
reader), and since stage 16 the `title` is no longer written either: it was
only ever informational, and it is personal data in a file that outlives the
session (still read from older files, shown in the abort message if present).

This extends the `WindowId`-as-bridge decision above: `WindowId` bridges the
registry and layout engine *at runtime*; the stored HWND bridges the same
identity *across daemon restarts*.

## Drop the loadout staleness guard (No `saved_at` / `max_age_secs`)

Restore runs **no staleness check**: a loadout is applied whenever every saved
HWND is live, and the no-partial abort handles everything else. Earlier the
file carried a `saved_at` timestamp and auto-restore skipped snapshots older
than `max_age_secs` (default 60s). That guard was removed as **redundant with
no-partial** — the two are anti-correlated across the save→restore gap:

- **Short gap (< `max_age_secs`):** the staleness guard does not trip, so it
  contributes nothing, and HWND recycling within seconds is vanishingly
  unlikely.
- **Long gap (> `max_age_secs`):** an HWND could be recycled to a different
  window — but in that same gap other saved windows have closed too, so their
  HWNDs are simply absent and `resolve_hwnd` aborts the whole load on the
  first missing one, before any recycled handle can match. The collision is
  shadowed by an ordinary no-partial abort.

For a collision to slip through, every saved HWND would have to be live *and*
one recycled to a different window — a gap so long a 60s threshold is the wrong
proxy anyway. Worse, the threshold paid a **false-negative cost**: a valid
restore after a >60s gap (every window still open, every HWND stable) was
silently skipped. Removing the guard fixes that. The file format bumped
`version` 2 → 3 (old files are rejected by the existing version guard, logged
and harmless); `--no-restore` (the "don't even try" escape hatch) is unchanged.
Full rationale in `docs/adr/0006-drop-loadout-staleness-guard.md`.

## Restore Animates All, Switch Teleports Bystanders (Intentionally Divergent)

Workspace switching and loadout restore both end with the same picture: one
workspace visible at vertical offset `0`, every other non-empty workspace
parked one monitor-height (plus a `window_gap`) above or below it. They reach
that picture through **different animation policies**, and the two paths
deliberately do not share a code path beyond the pure merge+offset math.
(On a monitor with another display directly above or below, the fork parks
that side's workspaces beyond the Win32 virtual screen instead and moves them
there instantly, ADR-0007; see
[Neighbours on four sides](./layout/projection.md#neighbours-on-four-sides).)

- **Switch** (`switch_workspace_layout`) animates only the two *participants*
  (the source and destination workspaces, whose offset genuinely changes) and
  **teleports** side-changed bystanders into place via `teleport_workspaces`
  (an instant `SetWindowPos`, no tween).
- **Restore** ([`apply_loadout`](./loadout.md) → `build_seating_batches` →
  `animate_workspaces`) animates **every** non-empty workspace to its parking
  offset and performs **no** teleport step.

### The precondition that drives the divergence: are the bystanders pre-parked?

The teleport is only safe when the bystander workspaces are **already parked
off-screen**. That is the invariant a running daemon maintains between switches:
every non-active workspace sits one monitor-height above or below the viewport,
off the visible area. A switch flips which workspace is at offset `0`, and a
bystander whose parking *side* changed (e.g. workspaces 3–7 when switching
2 → 8: below active 2 but above active 8) is already off-screen on both ends.
Snapping it instantly is **invisible** — it moves from one off-screen slot to
another, so the user never sees the jump. Teleporting it avoids a long,
distracting slide across the full visible area.

At restore time that precondition does **not** hold. Restore runs after the
layout has just been applied but before any seating animation, so **nothing is
pre-parked** — every window is still at its on-screen init position (the
freshly-tiled layout the apply step just computed). A bystander workspace is
therefore sitting *on-screen*, not one monitor-height away. Teleporting it to
its parking offset would `SetWindowPos` an **on-screen** window straight
off-screen — a visible snap/jump the user sees. Animating it instead slides it
out smoothly.

So the same teleport that is correct for a switch is **wrong** for a restore,
and vice versa: a switch cannot animate every bystander (visual noise, eight
workspaces sliding at once on a 10-workspace switch), and a restore cannot
teleport (on-screen windows would visibly jump off-screen).

### The accepted cost: a full-span slide on an explicit load

Because restore animates rather than teleports, a workspace whose parking side
**flips** relative to the previously-visible workspace on an explicit
`flow loadout load` will animate across the full vertical span — it starts
on-screen and slides all the way to its off-screen parking offset. This is the
visible "streak" the switch path's teleport exists to suppress. It is accepted
because restore is a rare, explicit, whole-layout operation where a short
slide is expected and acceptable, and the alternative (teleport) is strictly
worse here (the on-screen snap).

Startup auto-restore (`try_restore_loadout_default`) has **no such streaks**,
because at startup no window begins parked either — every window starts at its
fresh-init position and animates once to its restored target. There is no
prior visible workspace to flip *from*, so no bystander makes a full-span
crossing; every workspace simply settles into place.

### Do not unify these paths

A future maintainer may be tempted to collapse `switch_workspace_layout` and
`build_seating_batches`/`animate_workspaces` into one shared
"seat the stack" routine. **Do not.** The two paths share the pure
merge-scrolling-and-floating + `workspace_y_offset` math (that sharing is
correct and already factored into `build_seating_batches`), but they must
remain split at the animation step:

- Making **restore teleport** reintroduces the on-screen snap described above.
- Making **switch animate every bystander** reintroduces the multi-workspace
  visual noise the teleport was added to kill.

The split is load-bearing and the divergence is the point, not an accident to
clean up. The pre-parked-vs-not precondition is the single fact that decides
which policy is correct, and it differs between the two call sites.

## Decisions Made in the Multi-Monitor Fork

The fork (`feat/multi-monitor`) records its own decisions as ADRs in
`docs/adr/`, in the same shape as ADR-0006: a summary, the context, the
options considered and the consequences. Numbers were assigned on the fork;
if upstream's unmerged branches land first with the same numbers, these are
renumbered at merge time, the way ADR-0007 (cursor) already was once.

- **ADR-0007 — One scrolling space per monitor, parking beyond the virtual
  screen.** Each display owns a strip and ten workspaces; parked windows go
  beyond the Win32 virtual screen on a side with no neighbour and any move
  into or out of such a slot is instant, so a window is never drawn over the
  adjacent display. Display changes are reconciled in one debounced pass.
  `docs/adr/0007-one-scrolling-space-per-monitor.md`
- **ADR-0008 — Tiling on/off is a pause inside the daemon, not a stop.**
  Pause returns every tile to its pre-tiling rect and refuses layout
  commands; resume rebuilds from the live desktop with remembered seats.
  `docs/adr/0008-tiling-pause-is-not-a-stop.md`
- **ADR-0009 — Every exit fails back to the plain Windows desktop.** One
  ordered teardown for every exit path, the hotkey script in a kill-on-close
  job object, staged startup, and a daemon-less `flow rescue`.
  `docs/adr/0009-every-exit-fails-back-to-windows.md`
- **ADR-0010 — Windows that exist before the daemon are adopted, not
  skipped.** Minimized windows are classified from their placement,
  maximized ones un-maximized when `adopt_maximized` is on, cloaked UWP
  frames registered on uncloak; every recovery hook registers an untracked
  window first. `docs/adr/0010-pre-existing-windows-are-adopted.md`
- **ADR-0011 — Mod + left-button drag: the keymap moves, the daemon
  brackets.** No native move loop survives a hook hotkey, so the keymap
  moves the window and `drag begin` / `drag end` put the daemon in the
  native drag's state. `docs/adr/0011-keymap-driven-drag-sessions.md`
- **ADR-0012 — The daemon trusts its own user only, and nothing it did not
  write.** Pipe DACL and client QoS, bounded lines, validated loadout paths,
  `FLOW_BIN`, image-checked pidfile kills, an overlay registry instead of
  `GWLP_USERDATA`, no titles on disk, and an update *checker* in place of the
  self-updater. `docs/adr/0012-local-user-trust-boundary.md`
- **ADR-0013 — The floating layer follows ADR-0003's invariant.** Floats
  are topmost only while flow owns the foreground; see the file for the two
  deviations (a config knob, and shell surfaces that leave the state alone).
  Owned windows of flow windows are classed by their owner (ADR-0024).
  `docs/adr/0013-floating-layer-follows-adr-0003.md`
- **ADR-0014 — Width patterns are assigned by column position and pin the
  strip.** A `[columns]` preset may be a `+` pattern (`1/3+2/3`); column `i`
  took term `i mod n`. The by-position rule is superseded by ADR-0017; the
  grammar, the pin (resize commands refused while a pattern is on), the
  per-screen `[[columns.screen]]` rules that pick the list by orientation,
  width or device name, and the check of presets against the real screens
  instead of silent flooring all stand.
  `docs/adr/0014-width-patterns-by-column-position.md`
- **ADR-0015 — Key bindings are configuration the daemon owns, and the
  keymap interprets.** `[binds]` in `flow.toml`, niri's chord and action
  spelling, laid over the keymap's built-in table; the daemon validates and
  lists it (`flow query binds`), the hotkey script registers the list at
  runtime and draws its overlay from it; a saved `flow.toml` is reloaded by
  a once-a-second file watch; `flow config check` lists every problem
  through the daemon. Mod+LMB drag, Mod+Ctrl+T and Mod+Ctrl+Shift+T stay
  hardwired. `docs/adr/0015-binds-are-config.md`
- **ADR-0016 — The layers are reached by a focus switch, and every drag is
  a `drag_state`.** `focus-floating`, `focus-tiling` and niri's
  `switch-focus-between-floating-and-tiling` (Mod+Shift+V) push focus to the
  other side of the active workspace, which is how a float under a
  fullscreen window is reached; a Mod+click becomes a drag only after 8 px;
  a float being dragged — natively or by a keymap session — holds the single
  `drag_state` as `DragMode::Float`, the shape upstream's unmerged ADR-0008
  chose, so the animator, the busy gate and the routing need no second flag.
  `docs/adr/0016-layer-focus-switch-and-one-drag-state.md`
- **ADR-0017 — Width patterns are screen slots, filled from the focused
  column.** A pattern's terms are the slots of the screen, not widths by
  strip position: applying one resizes the columns on screen in place with
  the focused column taking the first term, and the slots then stay put —
  a column scrolled in takes the slot it lands in. The run starts at the
  column on the viewport's left edge, so the view always begins on a column
  boundary. Supersedes the position rule of ADR-0014.
  `docs/adr/0017-width-patterns-are-screen-slots.md`
- **ADR-0018 — Tile drops land by window halves, scroll bands are ratios,
  and a tile can be dropped on another screen.** The outer third of a
  column drops beside it, the middle third above or below the visible row
  under the cursor by which half of it the cursor is in; a stack with
  parked rows has top and bottom row-scroll bands; the screen-edge bands
  are a twelfth of the work area (pixels on an edge shared with another
  monitor); the zone is resolved on the monitor under the cursor, and a
  drop there moves the tile through `move-to-monitor`'s tail.
  `docs/adr/0018-drop-zones-by-window-halves.md`
- **ADR-0019 — A move carries no size, and a resize into a parking slot is
  not offered to the app.** Every `SetWindowPos` on a managed window goes
  through `place_window`: `SWP_NOSIZE` for a pure move, `SWP_NOSENDCHANGING`
  for a resize whose target is off the virtual screen, the plain flags for
  an on-screen resize and for the first placement back on screen. Fixes
  Visual Studio-shell apps (SSMS 21) that jumped onto the left monitor
  instead of parking right.
  `docs/adr/0019-pure-moves-and-silent-parked-resizes.md`
- **ADR-0020 — An app that has a full screen of its own is asked for it,
  and its own switches are followed.** A `fullscreen = "app"` rule option
  (bundled for Remote Desktop) makes `set-window fullscreen` post
  `SC_MAXIMIZE` / `SC_RESTORE` instead of sizing the frame, and never place
  the frame while the app is in its full screen; the app's own maximize and
  restore are adopted through a location-change watch on such windows. A
  rule's `action` became optional (options-only rules), and the keymap
  re-installs its keyboard hook in front of Remote Desktop's. Amended by
  stage 27: Mod+F into the full screen waits for the Win release, the hook
  re-install is periodic but never while a modifier is down, and bound Win
  chords get Win-less twins that fire on the keymap's own Win tracking
  (Remote Desktop keeps the Win key from Windows); the daemon gives the
  keymap a dated log next to its own.
  `docs/adr/0020-app-fullscreen.md`
- **ADR-0021 — Fullscreen is a column of the strip.** Mod+F flags the
  window's column: it spans one screen on the canvas, covers the monitor's
  screen rect while it starts the view and is parked at that size anywhere
  else (pure moves), so focus moves through it like niri and Mod+F out
  restores its width, slot, stack row and the view. A stacked window is
  expelled into its own column and merged back; a float joins the strip
  and floats back. Width and stack commands are refused on it; a pattern's
  run never holds it unless it is the anchor (a run of one), so scrolling
  off it shows the set before or after it (amends ADR-0017). Retires the
  float-space fullscreen model; no partial visibility, because Windows does
  not clip a window to its monitor.
  `docs/adr/0021-fullscreen-is-a-column.md`
- **ADR-0022 — Book mode: the columns out of view pile up behind the edge
  columns.** `[columns] layout = "book"` (per screen too, `set-layout` /
  Mod+Shift+B at runtime until the next reload) keeps the strip, its
  patterns and its camera, and gives a column strip mode would park the
  slot rect of the leftmost or rightmost visible column instead, so every
  window stays on screen and a one-column scroll flips the next card to the
  top. The daemon keeps each pile below its top, nearest first, before
  every batch and on the foreground-sync interval (one `EnumWindows`, a
  `SetWindowPos` only for a pile out of order); a window going into a pile
  is placed instantly. Activation (taskbar, Alt+Tab) flips the book in one
  jump; a drop on a top card makes the window the new top; fullscreen
  columns keep off-screen parking. Adds `[columns] wrap_around =
  "off" | "on" | "book"` for focus left/right at the strip ends.
  `docs/adr/0022-book-mode-piles.md`
- **ADR-0023 — New windows open on the focused screen.** A window the user
  just opened is seated on the active monitor (tile after its focused
  column at that strip's width, float centred there), wherever Windows
  placed it; windows found at startup, restored from minimize or first met
  when they take the foreground keep their own screen. `[focus]
  new_window_monitor = "placed"` keeps Windows' placement. Stage 30b moves
  a window that is not shown yet onto the focused screen at
  `EVENT_OBJECT_CREATE` (pre-placement), so it does not paint on the other
  screen first. Stage 30c: programs the keymap (non-admin spawn binds),
  `flow spawn` and `[startup] spawn` start are created by the daemon with
  `STARTF_USEPOSITION` at a point on the focused screen, so their first
  window is created there (for apps that do not pump messages before they
  show, such as Alacritty). Stage 30d: the hint is the rect the new column
  will get (a dry run of the seat, `STARTF_USEPOSITION | STARTF_USESIZE`),
  and a window flow has never placed (no previous rect) is moved to its
  slot instantly while the other columns tween, so new windows appear in
  place instead of gliding there.
  `docs/adr/0023-new-windows-open-on-the-focused-screen.md`
- **ADR-0024 — Owned windows of flow windows ride the floating layer.** An
  untracked window that takes the foreground and whose `GW_OWNER` chain
  reaches a tracked window (a modal dialog, an error box) is a *rider*:
  classed by that anchor instead of as a foreign app, held in the topmost
  band with the layer after the members, demoted and released with it, and
  never seated, bordered or learned. No window is seated under its own
  popup. Explicitly ignored shell surfaces (Start, search) are no longer
  layer members. Amends ADR-0013 (fixes the SSMS error box hidden behind its
  own window). `docs/adr/0024-owned-windows-ride-the-floating-layer.md`
- **ADR-0025 — Elevated windows are unmanaged.** A window whose process runs
  elevated (or whose token cannot be read) while flowd does not is out of
  reach: UIPI refuses every move, z-order change and activation. It is
  recorded `Ignored(Elevated)` at arrival, before any rule, logged once,
  shown by `flow query all`, never seated, bordered, pre-placed or learned,
  and `set-window` refuses it. It is foreign to the floating layer (the
  layer drops under it), so no refused z-order call is ever sent. Skipped
  when flowd itself is elevated.
  `docs/adr/0025-elevated-windows-are-unmanaged.md`
- **ADR-0026 — The selection buffer is a keymap option read through UI
  Automation.** `[keymap] selection_buffer` (off by default) reaches the keymap
  as an `option` record of the binds listing (live on save). `selection.ahk`,
  an include of `flow.ahk`, reads the selected text of the focused element,
  the element under the pointer or an ancestor of either (a web page keeps it
  on its Document) through UIA after a drag or a double / triple click (never the clipboard,
  never a password field; Alacritty, which has no text pattern, through its
  copy-on-select write, read and then undone) and pastes it on a middle click over a writable
  text field or a terminal, putting the clipboard back; every other middle
  click is passed on unchanged. `docs/adr/0026-selection-buffer.md`
- **ADR-0027 — A restored tile returns to its seat, and a frameless window has
  square corners.** The minimize hook records the tile's seat (monitor,
  workspace, column, row, alone, width) next to the owning-monitor memory; the
  restore puts it back there (index clamped to the column count; another
  workspace lands in the active one; another monitor appends), the view
  scrolls to it like a focus move (strip and book), and a foreground restore
  becomes the focus. `[minimize_restore] strategy` now takes effect. A tile
  hidden to the tray and shown again takes the same path (addendum, stage
  33b). The
  border's corner rule also reads whether the client area covers the window
  (Alacritty `decorations = "none"` keeps `WS_CAPTION | WS_THICKFRAME`):
  square then. `docs/adr/0027-restore-seat-and-frameless-corners.md`
- **ADR-0028 — Invisible frame borders are re-measured when a window is
  placed.** WPF / VS-shell windows (SSMS, Visual Studio) apply their chrome
  after registration, so the borders measured then were 0 and the content sat
  5-6 px inside the ring. Every layout batch, a drag start and a float's rect
  read-back re-measure the windows they touch (visible, not iconic, maximized,
  cloaked or in the app's own full screen; animator idle; a failed reading
  keeps the stored value); the 250 ms foreground sync re-measures the focused
  window and moves it into its slot on a change. No per-app rule.
  `docs/adr/0028-late-chrome-invisible-bounds.md`
- **ADR-0029 — Mod + mouse wheel chords are binds, with a cooldown in the
  keymap.** The default `[binds]` table maps Mod / Mod+Shift / Mod+Ctrl /
  Mod+Alt + wheel up or tilt left to the matching Left arrow action and wheel
  down or tilt right to the Right one (`WheelUp` .. `WheelRight`, niri's
  `WheelScroll*` as an alias). `flow.ahk` masks the Win release and lets each
  wheel chord step at most once per 150 ms, swallowing the ticks between; the
  plain wheel and unbound chords are never registered, and wheel chords get
  no Remote Desktop twin. `docs/adr/0029-mod-wheel-chords.md`
- **ADR-0030 — Lock-screen surfaces are ignored.** When the session locks,
  explorer.exe shows two `LockScreen*` frames and LockApp.exe its
  `Windows.UI.Core.CoreWindow`; with no rule they became floats or tiles and
  were centred, foregrounded and made topmost, which shifted the lock picture.
  Built-in ignore rules cover `LockApp.exe` and explorer's `^LockScreen`
  classes, those classes are shell classes (never learned, never topmost, no
  `set-window` target), and a hidden lock surface leaves the registry instead
  of lingering as a stale entry. A user `exe = "explorer.exe"` rule without a
  class still wins. `docs/adr/0030-lock-screen-surfaces-are-ignored.md`
- **ADR-0031 — The overlay names the Windows key `Win`; `Mod`, `Win` and
  `Super` are one modifier.** The keymap's modifier is always the Windows key,
  so the canonical chord text says `Win`: the overlay and hardwired rows, the
  `chord` field of `flow query binds` (JSON and `--plain`) and the canonical
  chord in config messages. The AutoHotkey hotkey strings (`#Left`) are
  unchanged. `[binds]` keeps niri's `Mod` and also accepts `Win` / `Super`;
  two spellings of one chord in a table are a duplicate whose message quotes
  both keys as written. `docs/adr/0031-the-overlay-names-the-windows-key.md`
