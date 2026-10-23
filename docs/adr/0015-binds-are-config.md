# Key bindings are configuration the daemon owns, and the keymap interprets

**Status:** Accepted — closes the roadmap's `CheckConfig` stub on the way.
`SetConfigValue` stays open.

The chords live in `flow.toml`, in a `[binds]` table written the way niri
writes its binds (`"Mod+Shift+Left" = "move-column-left"`, `"Mod+T" =
{ spawn = ["alacritty"] }`), laid over a built-in default table that is the
keymap's `binds.toml` compiled into the daemon. The **daemon** parses and
validates the table like any other part of the config and answers
`flow query binds` with the resolved list: each chord's AutoHotkey string,
what it runs, when it is live, and the help rows. The **hotkey script**
registers that list at runtime with `Hotkey()`, draws its overlay from it,
and re-reads it when `flow.toml` is saved. A saved `flow.toml` is reloaded
by the daemon itself (a once-a-second look at the file's modification time),
and `flow config check` asks the daemon to list every problem without
applying anything. Three chords stay in the script and are refused by the
table: Mod + left-button drag, Mod+Ctrl+T (FlowWM off/on) and
Mod+Ctrl+Shift+T (pause).

## Context

The fork's keymap was a 430-line AutoHotkey script with every chord
hardcoded and a second hardcoded copy of the list in the help overlay.
Changing a chord meant editing AutoHotkey; the overlay and the script
drifted; the launcher and the overlay bound their own chords in their own
files. The user, comparing with niri ("can we do something with the way the
keybinds work? make it more unified?"), wanted one readable table in the
config, in a spelling a niri user already knows, that applies on save. The
daemon already had a validate-before-apply reload, a `CheckConfig` stub on
the roadmap, and `FLOW_BIN`, which ties the script to the daemon's own CLI.

## Considered Options

- **The script parses `[binds]` itself.** Rejected: AutoHotkey would need a
  TOML parser, every bad entry would surface as a script error on the
  desktop, and the daemon could not refuse a broken table or list it for
  `flow config check`. The daemon owns the config; the script owns the hook.
- **The daemon generates an `.ahk` file from the table.** Rejected: a
  generated script is a second source the user finds and edits, it needs a
  restart of the script to apply, and a generation bug is a desktop error
  dialog. Runtime `Hotkey()` registration from a listing applies in place
  and turns every problem into a log line.
- **Switch the config to KDL, as niri.** Rejected (and the user agreed to
  keep TOML): a second config language is the kind of change that gets a
  fork rejected, and the TOML parser, the schema and the reload path exist.
  niri's *spelling* is kept where it costs nothing: chord names and action
  names.
- **The table replaces the defaults (niri's model).** Rejected for the
  common case: changing one chord would mean pasting sixty lines. Entries
  are laid over the built-in table, `"none"` unbinds a default, and
  `defaults = false` gives the niri model to anyone who wants it. The
  defaults are a TOML file in the keymap's folder (`binds.toml`) compiled
  into the daemon, so the readable source and the built-in table are the
  same bytes.
- **Document order for the listing.** Not available: the TOML parser sorts
  table keys. Rather than depend on it, the resolved list is in a canonical
  order (group, modifiers, key), which also puts a user's new chord next to
  its siblings in the overlay. A consequence: two spellings of one chord in
  one table are an error, not last-wins.
- **A file watcher thread (`ReadDirectoryChangesW`, or the `notify` crate).**
  Rejected for now: the event loop already wakes at least once a second for
  its liveness check, two `GetFileAttributesEx` calls per second are free,
  there is no new dependency and no thread to fail back, and a change seen
  unchanged on two looks rides out editors that write in several steps.
  Latency is one to two seconds; a watcher would make it instant and can
  replace the poll behind the same `observe` seam if that ever matters.
- **A `binds_rev` counter in `Pong` for the keymap to notice a reload.**
  Rejected: the keymap already polls every two seconds for liveness without
  spawning a process (`WaitNamedPipe`); polling the file's modification time
  on the same timer costs as little, and `flow ping` every two seconds would
  spawn a process each time. The keymap re-queries ~2.5 s after a change,
  after the daemon's own debounce, and re-registers only when the listing
  differs.
- **Everything configurable, including the on/off switch.** Rejected: the
  Mod+Ctrl+T pair is shared with `flow-resident.ahk`, which runs with no
  daemon to ask, and the pause flips the script's own state; the drag is a
  mouse hotkey with its own move loop. The three are reserved so a broken
  table can still be turned off from the keyboard.

## Consequences

- **One vocabulary.** The bind table, `flow query binds` and the CLI speak
  niri's names (`flow dispatch focus-column-left` is accepted as an alias
  of `focus left`); the daemon's own command names stay primary. Two niri
  names have only a near equivalent (`consume-window-into-column`,
  `expel-window-from-column`) and their help text says so.
- **The overlay cannot disagree with the chords.** Both come from the same
  listing; the hardcoded `Bindings` array is gone. Consecutive binds of one
  group and modifier set collapse into one row.
- **Programs are resolved the same way on both sides.** A bare `spawn` name
  (and a `[startup] spawn` entry) is looked up in Program Files,
  `%LOCALAPPDATA%\Programs`, the Store's execution aliases and `PATH`, never
  the working directory (ADR-0012's rule for `flow` itself); a path must be
  absolute. The keymap's Alacritty / Windows Terminal special cases became
  that rule.
- **Validation is everywhere the config is loaded.** Start fails at stage
  `config` with the first problem; `flow config reload` and the watcher keep
  the running config; `flow config check` lists every problem (each
  `[binds]` entry separately, plus the preset lists against the attached
  screens) and falls back to a local, screen-blind check when no daemon
  runs. `CheckConfig` is no longer a stub.
- **Live reload applies to the whole config**, not only binds: a saved
  `flow.toml` or `flow-rules.toml` is reloaded one to two seconds later,
  logged, and held while tiling is off (reload is refused then) until
  tiling resumes.
- **`[startup] spawn`** runs the user's programs once after `ready`, from
  the home folder, logged; a failure never stops the daemon. Same trust
  boundary as the keymap script in the same directory.
- **Accepted costs.** The listing is read through a temp file in `%TEMP%`
  (UTF-8, no console flash). A chord the hook cannot register (a key
  AutoHotkey rejects) is a log line, not an error at config load, because
  only the script knows. Help text and chord names are ASCII so the
  overlay renders the same everywhere. The baked fallback listing in
  `flow.ahk` must be regenerated when the defaults change; a test says so
  and prints the block. Reload latency is one to two seconds for the
  daemon and up to five for the keymap. `flow query binds --plain` is part
  of the keymap's contract and is versioned with the daemon through
  `FLOW_BIN`.
- **Tests.** The chord and action grammars, the built-in table chord by
  chord, layering, duplicate spellings, every-problem reporting, the TOML
  round trip, the overlay rows, the report and plain listing, the CLI
  aliases and the baked fallback (`src/config/binds.rs`); the program
  lookup (`src/config/startup.rs`); the debounce
  (`src/daemon/config_watch.rs`); `CheckConfig` over a real directory and
  `query binds` (`src/daemon/width_preset.rs`). The VM driver
  (`test-stage21.ps1`) saves an override, an unbind and a broken entry into
  the guest's `flow.toml` and checks the daemon's log, `flow config check`,
  `flow query binds`, the overlay's text and real key presses, then a
  `[startup]` entry across a restart. See `docs/src/dev-guide/keybinds.md`.
