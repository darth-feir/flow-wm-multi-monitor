# win-niri keymap

FlowWM keybindings in niri's flavour: the Windows key is Mod, arrows instead
of hjkl, and every chord maps onto a `flow dispatch` command. The bindings
themselves are listed in the header of `flow.ahk`; this file holds the
design notes, the Windows lessons behind the scripts and the setup.

## Files

| File | Role |
|------|------|
| `flow.ahk` | The keymap. `#Include`s the three files below. Started by the daemon (`flow start --ahk`) inside its kill-on-close job object. |
| `flow-exe.ahk` | `ResolveFlowExe()` — where `flow.exe` is. Shared by `flow.ahk` and `flow-resident.ahk`. |
| `launcher.ahk` | Application launcher on Mod+Space / Mod+D: Start-menu shortcuts, open windows, a few built-ins, fuzzy-matched. |
| `help.ahk` | Key-binding overlay on Mod+Shift+/. Its `Bindings` array is the source of truth for the overlay; keep it in step with the hotkeys. |
| `flow-resident.ahk` | Optional resident starter: runs all the time (shortcut in `shell:startup`) and turns Mod+Ctrl+T into an on/off switch for the whole of FlowWM. |
| `flow.toml.example` | The daemon side of the map: `[columns]` width presets and `edge_hop = false`. |

Install: copy `flow.ahk`, `flow-exe.ahk`, `launcher.ahk` and `help.ahk` to
`%USERPROFILE%\.config\flow\` (the files must sit next to each other — the
includes are `%A_ScriptDir%`-relative), merge `flow.toml.example` into
`flow.toml`, and run `flow start --ahk`. `launcher.ahk` and `help.ahk` are
includes and do not run on their own.

## How flow.exe is found

`ResolveFlowExe()` (`flow-exe.ahk`) tries, in order:

1. `%FLOW_BIN%\flow.exe` — `FLOW_BIN` is the directory of the running
   `flowd.exe`; the daemon sets it in the keymap's environment when it
   launches `flow.ahk`, so the keymap always drives the daemon it belongs
   to. Set it yourself for `flow-resident.ahk` if flow is installed
   somewhere unusual.
2. `%LOCALAPPDATA%\Programs\flow\flow.exe` — the usual install location.
3. plain `flow` — last resort, from `PATH`.

Never the working directory, on purpose: `CreateProcess` searches the cwd
before `PATH`, and the daemon used to inherit whatever cwd `flow start` was
run from, so a keymap that ran `flow` by bare name could have picked up a
`flow.exe` planted there. The daemon now starts the script with the config
directory as its working directory (see the developer guide, *The hotkey
script is tied to the daemon*). The resolved path is the first line of the
`FLOW_AHK_LOG` trace. Every `Run` of `flow.exe` quotes the path.

Terminals and launched applications start in the home folder
(`%USERPROFILE%`), not wherever the daemon — and so this script — was
started from; `flow-resident.ahk` starts the daemon from the home folder
for the same reason.

## Daemon liveness and fail-back

Every window-management chord is registered under `#HotIf DaemonUp` (or
`DaemonUp && TilingOn`), so the moment the daemon's pipe is gone the Win
key and all chords revert to stock Windows:

- At startup the script refuses to hook anything unless the daemon answers
  within 10 s (`WaitNamedPipe` on `\\.\pipe\flow`, or `FLOW_PIPE_NAME`).
- `CheckDaemon` re-checks every 2 s with a 1 ms `WaitNamedPipe`. `A_LastError`
  2 means no such pipe (daemon gone); 121 (`ERROR_SEM_TIMEOUT`) means the
  pipe exists but every instance is busy, i.e. a daemon is there. Busy
  counts as alive only for `MaxBusyPolls` (5) consecutive polls: a wedged
  daemon holds the pipe busy forever and must count as down so the Win key
  is released. The daemon-side client applies the same reading (see the
  developer guide, *Overlapped I/O and deadlines*).
- After 5 missed polls (~10 s) the script shows a message and exits.
- A dispatch is run without blocking the hotkey thread (`Run` returns at
  once); its exit code is collected asynchronously by `DispatchDone`, which
  polls the process every 50 ms for up to ~5 s. A non-zero exit means the
  daemon did not answer, so the liveness check runs right away and the
  very next chord already falls through to Windows.

The daemon itself kills the script when it stops (job object + pidfile), so
`flow stop` or a daemon crash releases the Win key even if the script's own
polling has not noticed yet.

## Tiling toggle (Mod+Ctrl+Shift+T)

`flow dispatch tiling toggle` pauses tiling inside the daemon without
stopping it. While tiling is off every window-management chord (focus,
move, monitors, workspaces, close, center, float, width, consume/expel,
terminals) is released to Windows, so Win+Arrows snap, Win+1..9 launch from
the taskbar, and so on. Only the toggle itself, the launcher, the help
overlay and the stop chords stay bound. The state is read from `flow ping`
at startup (`"tiling off"` in its output; `true` is assumed if ping cannot
be run or does not answer in 3 s) and flipped locally whenever the toggle
dispatch exits 0; an on-screen message reports each flip.

## Stop and start (Mod+Ctrl+T, Mod+Shift+E)

`flow.ahk` binds Mod+Ctrl+T and Mod+Shift+E (niri's quit chord) to
`flow stop` followed by its own exit. `flow-resident.ahk` binds the same
Mod+Ctrl+T to `flow start --ahk` under `#HotIf DaemonDown()`, so with the
resident script running the chord is a plain on/off switch. Both scripts
hook the key; the one whose `#HotIf` condition is false lets it pass
through to the other, and the conditions are exclusive (pipe absent /
present). The resident script reports a start that has not produced a
daemon after 8 s.

## What stays Windows, what is shadowed

Win+L (lock) is handled by Windows below the hook layer and is untouched;
the only ways around that are a keyboard-firmware or registry remap of the
left Win key (the FlowWM README's "sacrificial key" setup), neither of which
is used here.

Not bound here, so still Windows: Win+Tab, Win+E, Win+Shift+S,
Win+A/I/X/S/N/W/P, Win+Comma (peek), Win+Period (emoji), Win+Ctrl+Space.

Shadowed while the daemon runs: Win+1..9/0 (taskbar launch), Win+C, Win+Q,
Win+V (clipboard history), Win+R (Run: use the right Win key or the
launcher), Win+F (Feedback Hub), Win+D (show desktop), Win+T (taskbar
cycle), Win+Minus/Equal (Magnifier), Win+Arrows (Snap), Win+Ctrl+Left/Right
(virtual desktops), Win+Shift+Left/Right (Windows' own move-to-monitor),
Win+Space (Windows' input-language switcher; Win+Ctrl+Space, which cycles
the other way, still works). With tiling off only Win+Space, Win+D,
Win+Shift+/, Win+Ctrl+T, Win+Ctrl+Shift+T and Win+Shift+E remain shadowed.

## AutoHotkey and Windows lessons

- **No bare-Win-tap swallow.** AutoHotkey already masks the Start menu
  after every Win chord, and the `~LWin::Send "{Blind}{vkE8}"` trick dropped
  roughly one chord in eight in testing (its own key send lands between the
  Win press and the arrow). A bare Win tap therefore still opens Start.
- **`#+/` never fires in AutoHotkey v2** (Shift changes how `/` resolves),
  so the help chord binds the physical key by virtual-key code: `vkBF` =
  `VK_OEM_2` = the `/ ?` key on US layouts.
- **Never `TrayTip` for status messages.** Windows 11 turns a tray tip into
  a toast that stays until dismissed and holds the foreground lock, so
  neither this script's windows nor the daemon's focus pushes can take the
  foreground meanwhile. `Osd()` shows a click-through (`E0x20`), no-activate
  tool window in the bottom-right corner of the primary screen instead.
- **A GUI shown from a chord may not get the foreground** while the Win key
  is still held. `ShowCentered()` centres the window on the work area of
  the monitor that holds the active window, then retries `WinActivate` for
  ~0.5 s. It measures the window with a hidden `Show` first so auto-sized
  controls are already laid out.
- **Admin launches** use the shell's `RunAs` verb, so UAC prompts as usual;
  a declined prompt is not reported. Alacritty is run by fixed absolute
  paths first (the machine-wide `%ProgramFiles%` install before the
  per-user one) and a `where.exe` lookup only as the last resort; Windows
  Terminal's Store execution alias is a fixed per-user path
  (`%LOCALAPPDATA%\Microsoft\WindowsApps\wt.exe`), with `wt.exe` left to the
  shell's lookup only if the alias is absent.
- **Exit codes without blocking a hotkey:** `DispatchDone` opens the
  process with `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE` (enough to
  read the exit code) and polls `WaitForSingleObject` with a zero timeout.

## Launcher

Needs nothing from the daemon — it only uses `ShowCentered()` and
`AhkLog()` from `flow.ahk` and still works when `flow` itself is
unavailable. Showing it again while it is open hides it. Keys while it is
active: Up/Down/PgUp/PgDn move the selection (Tab / Shift+Tab too), Enter
runs, Esc hides, double-click runs. Enter runs or activates the selection;
with nothing matched, the typed text is passed to `Run` as a command or
path.

**Index.** Every `.lnk` under the user and common Start menus
(`A_StartMenu`, `A_StartMenuCommon`, `A_Programs`, `A_ProgramsCommon`,
nested roots skipped), deduplicated by display name case-insensitively,
`Uninstall*` skipped, rebuilt lazily every 60 s; plus Settings, Lock and
Task Manager as built-ins; plus the open top-level windows (empty titles,
tool windows, cloaked windows — other virtual desktops, suspended UWP —
the shell's own frames and the script's own windows are skipped),
rebuilt on every show. With an empty query windows are listed first.

**Fuzzy matching** (`FuzzyScore`) is a case-insensitive subsequence match.
0 means no match (the query is not a subsequence of the text); otherwise:

| Points | For |
|--------|-----|
| +10 | each matched character |
| +25 | a match on the first character of the text |
| +15 | a match that starts a word (after space, `-`, `_`, `.`, `/`, `\`, `(`, `)`, `[`, `]`, `:`, `,` or a camelCase hump) |
| +12 | a match directly following the previous match |
| −1 | per character skipped since the previous match (at most −8) |
| +30 | the whole query is a prefix of the text |
| +0..10 | short texts (`10 - len/8`) |

When a character would match mid-word without continuing the previous
match, a later word-start occurrence is preferred — but only if the rest
of the query still matches after it, so a valid match is never lost. For
the query `edge`: *Microsoft Edge* scores 92 (e@11 word start 17, then d,
g, e consecutive at 22 each, +9), *Registry Editor* 0 (the only g comes
before the only d: not a subsequence), *Event Viewer* and *File Explorer* 0
(no d, no g). `FuzzySelfCheck()` pins these at load time and reports to
`AhkLog`, never a dialog. A window is scored on its title and its process
name, whichever is higher.

**Refilter.** Sorting uses `Sort()` on `paddedScore<TAB>name<TAB>index`
lines: a fixed-width zero-padded `999999 - score`, so plain lexicographic
order gives score descending, then name (case-insensitive); the entry index
is read back from the end of each line, entries scoring 0 are dropped, at
most 60 rows are shown. A 100 ms watchdog (`LauncherWatch`) refilters if
the Edit text no longer matches what the list was built from (a missed
`Change` event) and keeps the keyboard focus on the Edit so typed letters
never go to the ListBox, whose own type-ahead would eat them; `Enter`
refilters first if the list is stale, so it never acts on a stale row.

## Tracing

Set `FLOW_AHK_LOG=<path>` in the environment to trace every dispatch and
its exit code, the liveness gate transitions, the launcher's fuzzy
self-check and every refilter, and the OSD messages.
