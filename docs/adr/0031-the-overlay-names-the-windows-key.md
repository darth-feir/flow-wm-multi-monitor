# The overlay names the Windows key `Win`; `Mod`, `Win` and `Super` are one modifier

**Status:** Accepted (stage 37, 2026-11-10). Changes the display name of the
Windows-key modifier in everything the daemon prints about chords; the
`[binds]` grammar is unchanged in what it accepts. Builds on
[ADR-0015](0015-binds-are-config.md) and
[ADR-0029](0029-mod-wheel-chords.md).

## Context

niri writes its modifier as `Mod` because it is not fixed: Super on a
desktop session, Alt when niri runs nested in another compositor. The
win-niri keymap has no such fallback; its modifier is always the Windows key
(`#` in AutoHotkey), in a VM too. The key-binding overlay
(`show-hotkey-overlay`, Win+Shift+/) nevertheless printed niri's `Mod+Left`,
so a user reading it had to know that `Mod` means the Windows key; the
overlay's title carried a `(Mod = Win)` note to say so.

The chord parser already accepted `Super` and `Win` as spellings of `Mod`,
and two spellings of one chord in a table were already an error. Only the
output was niri-shaped.

## Considered Options

- **What the output calls the modifier.**
  - **(a) `Win` everywhere the daemon prints a chord**: the overlay rows, the
    hardwired rows, the `chord` field of every `flow query binds` record
    (JSON and `--plain`), and the canonical chord in a config message.
    Chosen: one canonical text, the name of the key on the keyboard; there
    is no second display form to keep in step.
  - (b) `Win` only in the overlay rows, `Mod` in the JSON and the `bind`
    records. Rejected: the same chord would print two ways depending on
    where one looks, and `flow query binds` is also read by people.
  - (c) Keep `Mod` and the title note. Rejected: it is the thing the user
    asked to change.
- **What the tables accept.**
  - **(a) `Mod`, `Win` and `Super` (any case), as before; the docs and the
    built-in `binds.toml` keep niri's `Mod`.** Chosen: a line copied from
    niri's config or from the overlay works, and niri users keep their
    spelling.
  - (b) Rewrite the docs and `binds.toml` to `Win`. Rejected: the grammar is
    niri's and the docs say so; the alias is enough.

## Decision

- `Modifier::name` returns `Win` for the Windows key, so `Chord::canonical`
  (and its `Display`) is `Win+Shift+Left`. The AutoHotkey hotkey string
  (`#+Left`), which the keymap passes to `Hotkey()`, is unchanged.
- The `--plain` listing changes in the `bind` records' chord field (used by
  the keymap only in its log lines) and in the `row` / `hardwired` text the
  overlay prints. The baked fallback block in `flow.ahk` is regenerated to
  match.
- The hardwired rows read `Win + left-button drag`, `Win+Ctrl+T`,
  `Win+Ctrl+Shift+T`; the overlay title drops the `(Mod = Win)` note and its
  footer names `Win+Ctrl+T` / `Win+Ctrl+Shift+T`.
- Config messages quote the table key as the user wrote it
  (`binds."Mod+Left": the same chord as "Win+Left" (Win+Left); keep one`);
  a repeated modifier names the word given twice (`modifier "Win" given
  twice (Mod, Win and Super are one key)`).

## Consequences

- A script that matched `flow query binds` output on `Mod+` must match
  `Win+` instead. Nothing in this repository other than the tests did.
- A `[binds]` table written with `Mod+`, `Win+` or `Super+` registers the
  same hotkeys; a table that uses two spellings of one chord is refused by
  `flow config check` and on reload, as any duplicate was.
- The docs still show `Mod` in `[binds]` examples while the overlay shows
  `Win`; the config reference says the two are the same key.
