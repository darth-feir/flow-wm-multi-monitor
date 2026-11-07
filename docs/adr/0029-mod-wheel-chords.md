# Mod + mouse wheel chords are binds, with a cooldown in the keymap

**Status:** Accepted (stage 34, 2026-11-06). Adds sixteen wheel chords to the
keymap's built-in `[binds]` table (`keymaps/win-niri/binds.toml`), the key
names `WheelUp`, `WheelDown`, `WheelLeft`, `WheelRight` (niri's
`WheelScroll*` spelling kept as an alias), and a per-chord cooldown in
`flow.ahk`. Builds on [ADR-0015](0015-binds-are-config.md).

## Context

The user wants a hand on the mouse to move through the scrolling layout
without the keyboard: hold Mod (Win) and turn the wheel, exactly like the
Left / Right arrow chords. niri binds `Mod+WheelScrollDown` and friends in
its default config for the same reason.

The chord parser already knew the wheel (as niri's `WheelScroll*`), but
nothing was bound, a wheel chord fired once per notch (a single flick on a
free-spinning wheel is five to ten notches), and the overlay would have
listed each chord on its own line.

## Considered Options

- **Where the chords live.**
  - **(a) In the default `[binds]` table, like every other chord.** Chosen:
    the user can remap or clear each with `"none"` in `flow.toml`, `flow
    config check` validates them, `flow query binds` and the overlay show
    them, and the keymap needs no new kind of record.
  - (b) Hardwired in `flow.ahk` next to Mod + drag. Rejected: not
    configurable, invisible to `query binds`.
- **One flick, one step.**
  - **(a) A per-chord cooldown in the keymap (150 ms since the chord last
    stepped).** Chosen: the keymap is where the wheel events arrive; the
    ticks inside the window are swallowed as well, so the window under the
    cursor does not scroll either. A wheel held spinning steps about six
    times a second, which stays controllable.
  - (b) A debounce (step once the wheel has been still for N ms). Rejected:
    every step would lag by N ms, and a long spin would give one step.
  - (c) In the daemon (ignore a dispatch repeated within N ms). Rejected:
    the daemon would need to know which dispatches came from a wheel, and the
    ticks would still cost a `flow.exe` process each.
- **Passthrough.**
  - **(a) An unbound wheel chord is simply not registered; the plain wheel
    is never registered (`Chord::parse` refuses a bare key other than Print /
    Pause).** Chosen: the hook never suppresses what it has no hotkey for, so
    apps keep scrolling, and a chord cleared with `"none"` reaches the app
    with the modifiers held (Ctrl+wheel zooms again, for example).
  - (b) `~` hotkeys (fire and pass through). Rejected: Mod+wheel would also
    scroll whatever is under the cursor.
- **Remote Desktop twins.** The keymap registers a Win-less twin of every Win
  chord for an mstsc full screen ([ADR-0020](0020-app-fullscreen.md), stage
  27). A wheel chord's twin would be a bare `WheelUp` hotkey whose gate runs
  on the script's thread for every wheel notch in every app. **Rejected:**
  wheel chords get no twin; inside a Remote Desktop full screen Mod+wheel
  goes to the remote session.

## Decision

- Key names `WheelUp`, `WheelDown`, `WheelLeft`, `WheelRight` (AutoHotkey's
  own names) are canonical; `WheelScrollUp` .. `WheelScrollRight` parse to
  the same keys, so two spellings of one chord in a table are an error.
- Default table: with Mod, Mod+Shift, Mod+Ctrl and Mod+Alt, wheel up and
  tilt left run the `-left` action of the matching arrow chord
  (`focus-column-left`, `move-column-left`, `focus-monitor-left`,
  `move-column-to-monitor-left`); wheel down and tilt right the `-right`
  one. Up / down inside a column stays on the keyboard.
- The overlay groups a run of binds by key family as well as group and
  modifiers, so the wheel gets its own rows
  (`Mod+WheelUp/WheelDown/WheelLeft/WheelRight`) and the arrow rows keep
  their text.
- `flow.ahk` wraps the callback of every `...Wheel(Up|Down|Left|Right)`
  hotkey in `WheelBind`: it sends the menu-mask key (`{Blind}{vkE8}`, as
  Mod + drag does) so the Win release opens no Start menu, then steps only
  if `WheelCooldownMs` (150) have passed since that chord last stepped.
  `A_MaxHotkeysPerInterval` is raised to 400 so a fast wheel does not trip
  AutoHotkey's runaway-hotkey dialog.

## Consequences

- Mod+wheel, Mod+Shift+wheel, Mod+Ctrl+wheel and Mod+Alt+wheel no longer
  reach applications while tiling is on (Ctrl+wheel zoom with the Win key
  held, for instance). Tiling off, or `"none"` in `[binds]`, gives them back.
- The cooldown is per chord: turning the wheel the other way, or adding
  Shift, steps at once.
- Not covered: Mod+wheel inside a Remote Desktop full screen (no twin, see
  above); a wheel with high-resolution notches smaller than 120 still steps
  once per message, and the cooldown keeps that to one step per 150 ms.
