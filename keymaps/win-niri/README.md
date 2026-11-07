# win-niri keymap

FlowWM keybindings in niri's flavour: the Windows key is Mod, arrows instead
of hjkl, and every chord maps onto a `flow dispatch` command. The bindings
are configuration: the `[binds]` table of `flow.toml`, in niri's spelling,
laid over the default table in `binds.toml` here (see *The keymap is an
interpreter* below and the user guide's Config Reference, `[binds]`). This
file holds the design notes, the Windows lessons behind the scripts and the
setup.

## Files

| File | Role |
|------|------|
| `flow.ahk` | The keymap: an interpreter that registers the chords `flow query binds --plain` lists (the daemon's view of `[binds]`), re-reads them when `flow.toml` is saved, and hardwires the Mod + left-button drag, Mod+Ctrl+T and Mod+Ctrl+Shift+T. `#Include`s the four files below. Started by the daemon (`flow start --ahk`) inside its kill-on-close job object. |
| `binds.toml` | The default `[binds]` table, compiled into the daemon (`include_str!`). Paste a line into `flow.toml` to change it. Not read at runtime. |
| `flow-exe.ahk` | `ResolveFlowExe()` — where `flow.exe` is. Shared by `flow.ahk` and `flow-resident.ahk`. |
| `launcher.ahk` | Application launcher (the `launcher` action, Mod+Space / Mod+D by default): fuzzy-matches Start-menu shortcuts and a few built-ins; `window - x` and `file - x` search open windows or files instead. |
| `selection.ahk` | The selection buffer (`[keymap] selection_buffer`, off by default): text highlighted with the mouse is kept apart from the clipboard and a middle click pastes it. See *Selection buffer* below. |
| `help.ahk` | Key-binding overlay (the `show-hotkey-overlay` action, Mod+Shift+/ by default). Its rows come from the same `flow query binds` listing the hotkeys are registered from, so it cannot disagree with them. |
| `flow-resident.ahk` | Optional resident starter: runs all the time (shortcut in `shell:startup`) and turns Mod+Ctrl+T into an on/off switch for the whole of FlowWM. |
| `flow.toml.example` | The daemon side of the map: `[columns]` width presets (fractions and `2/3+1/3`-style slot patterns, per-screen `[[columns.screen]]` rules), `edge_hop = false`, `[focus] new_window_monitor`, and commented `[binds]` / `[keymap]` / `[startup]` examples. |

Install: copy `flow.ahk`, `flow-exe.ahk`, `selection.ahk`, `launcher.ahk` and `help.ahk` to
`%USERPROFILE%\.config\flow\` (the files must sit next to each other — the
includes are `%A_ScriptDir%`-relative), merge `flow.toml.example` into
`flow.toml`, and run `flow start --ahk`. `selection.ahk`, `launcher.ahk` and
`help.ahk` are includes and do not run on their own.

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

## The keymap is an interpreter

`flow.ahk` has no chord table of its own. After its startup gate it runs
`flow query binds --plain` (the output goes through a temp file, read as
UTF-8, so nothing is decoded with the console code page and no console
window flashes) and registers every `bind` record with `Hotkey()`:

```text
bind  #Left  tiling  dispatch  0  Mod+Left  focus the next window on this screen  focus<US>left
row   Mod+Left/Down/Up/Right  focus the next window on this screen
hardwired  Mod+Ctrl+T  FlowWM off (and on again, via flow-resident.ahk)
source  defaults
```

- The **hotkey** is the daemon's translation of the chord (`Mod+Shift+Slash`
  → `#+vkBF`, see the lesson below); the **scope** picks the `HotIf`
  criterion: `tiling` = `TilingGate` (daemon up and tiling on), `always` =
  `AlwaysGate` (daemon up). The gates are named functions so a reload can
  turn a chord off under the criterion it was registered with.
- The **kind** picks the callback: `dispatch` runs `flow dispatch <argv>`
  through `D()`, `spawn` resolves and starts a program (`Launch()`),
  `launcher` / `help` / `quit` / `toggle-tiling` call the script's own
  functions. A kind this script does not know (a newer daemon) is logged
  and skipped.
- `row` and `hardwired` records are the overlay; `help.ahk` rebuilds its
  panel from them after every (re)load.

The listing is re-read when `flow.toml` changes: `CheckDaemon`'s 2 s timer
also compares the file's modification time, and ~2.5 s after a change
(after the daemon's own one-second debounce has applied or refused it) the
script queries again and re-registers only if the listing differs, with a
one-line OSD. A refused reload leaves the daemon's listing, and so the
chords, unchanged.

Hardwired here and refused by the table: the Mod + left-button drag, and
Mod+Ctrl+T / Mod+Ctrl+Shift+T, which `flow-resident.ahk` shares and which
must keep working with a broken table. `DefaultBindsText` at the end of the
script is the built-in listing for a daemon too old to answer `query
binds`; a test (`the_keymaps_baked_fallback_matches_the_defaults`) keeps it
in step and prints the block to paste when `binds.toml` changes.

To change a chord: add it to `[binds]` in `flow.toml` and save. `flow query
binds` shows the result; `flow config check` names mistakes
(`binds."Mod+Bogus": unknown key ...`); turn on the keymap log (see
*Tracing*) to see each registration (`binds: 59 registered`).

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
move, monitors, workspaces, close, minimize, center, float, width, consume/expel —
the `tiling`-scoped binds) is released to Windows, so Win+Arrows snap,
Win+1..9 launch from the taskbar, and so on. The toggle itself, the
launcher, the help overlay, the program (`spawn`) chords and the stop
chords stay bound (`always` scope). The state is read from `flow ping`
at startup (`"tiling off"` in its output; `true` is assumed if ping cannot
be run or does not answer in 3 s) and flipped locally whenever the toggle
dispatch exits 0; an on-screen message reports each flip.

## Stop and start (Mod+Ctrl+T, Mod+Shift+E)

`flow.ahk` hardwires Mod+Ctrl+T, and the default table binds Mod+Shift+E
(niri's quit chord, the `quit` action), to `flow stop` followed by the
script's own exit. `flow-resident.ahk` binds the same
Mod+Ctrl+T to `flow start --ahk` under `#HotIf DaemonDown()`, so with the
resident script running the chord is a plain on/off switch. Both scripts
hook the key; the one whose `#HotIf` condition is false lets it pass
through to the other, and the conditions are exclusive (pipe absent /
present). The resident script reports a start that has not produced a
daemon after 8 s.

## What stays Windows, what is shadowed

Remote Desktop in its own full screen (Mod+F, `fullscreen = "app"`)
installs a keyboard hook of its own and sends the Windows key to the remote
session, so Windows (and with it AutoHotkey's modifier state) never sees the
Win key go down. `flow.ahk` handles that in three parts (ADR-0020, stage 27
amendment):

- **Entering.** Mod+F on a windowed Remote Desktop is sent once the Win key
  is released (at most 2 s): with the key still down when mstsc switches, its
  release was either swallowed (a stuck Win key) or let through unmasked
  (the local Start menu opened).
- **Hook in front.** While such a full screen is the active window the
  script re-installs its keyboard hook when it becomes active and then every
  second in a typing pause (250 ms poll), never while a modifier is down or
  within 400 ms of a hotkey: a re-install forgets that a Win chord fired, and
  the Win release then opens the Start menu.
- **Win chords.** The script follows the Win key itself and registers, for
  every bound Win chord, a twin without the Win modifier that fires only
  while that key is held in a Remote Desktop full screen. So Mod+F leaves the
  full screen and Mod+arrows act locally there too; unbound chords (Win+I,
  Win+E, ...) still reach the session. A twin first releases the Win key
  in the session (masked), which got it from mstsc before the chord was
  known. A Win key seen down for more than 5 s
  counts as released (a release mstsc swallows cannot leave the twins armed).
  The hardwired chords (Mod+Ctrl+T, Mod+drag) and the wheel chords have no
  twins (a wheel twin would be a bare wheel hotkey), so Mod+wheel there goes
  to the session.

Win+L (lock) is handled by Windows below the hook layer and is untouched;
the only ways around that are a keyboard-firmware or registry remap of the
left Win key (the FlowWM README's "sacrificial key" setup), neither of which
is used here.

Not bound here, so still Windows: Win+Tab, Win+E, Win+Shift+S,
Win+A/I/X/S/N/W/P, Win+Comma (peek), Win+Period (emoji), Win+Ctrl+Space.

Alt+H/J/K/L and Alt+Shift+H/J/K/L duplicate Mod+Arrows and Mod+Shift+Arrows
(upstream's vim-style defaults). While tiling is on they shadow any app menu
accelerator on those letters (Alt+H opens Help in some apps); tiling off
releases them like every other window-management chord.

Mod + mouse wheel mirrors the Left / Right arrow chords (wheel up or tilt
left = Left, wheel down or tilt right = Right) with no modifier, Shift
(move), Ctrl (screen) and Alt (move to screen); ADR-0029. Each wheel chord
steps at most once every 150 ms (`WheelCooldownMs` in `flow.ahk`), so one
flick is one step, and the ticks in between are swallowed. The plain wheel
is never bound; a wheel chord set to `"none"` in `[binds]` is not registered
and reaches the app again (Win+Ctrl+wheel zoom, for instance).

Shadowed while the daemon runs: Win+1..9/0 (taskbar launch), Win+C, Win+Q,
Win+M (minimize all),
Win+V (clipboard history), Win+R (Run: use the right Win key or the
launcher), Win+F (Feedback Hub), Win+D (show desktop), Win+T (taskbar
cycle), Win+Arrows (Snap), Win+Ctrl+Left/Right
(virtual desktops), Win+Shift+Left/Right (Windows' own move-to-monitor),
Win+Space (Windows' input-language switcher; Win+Ctrl+Space, which cycles
the other way, still works). With tiling off only the `always`-scoped
chords remain shadowed: Win+Space, Win+D, Win+T and its Shift/Alt variants,
Win+Shift+/, Win+Ctrl+T, Win+Ctrl+Shift+T and Win+Shift+E. All of these
follow the `[binds]` table; the list above is the default one.

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
- **Programs** (`spawn` binds) are resolved by `ResolveProgram()` the way
  the daemon resolves `[startup]` entries: a bare name is tried as
  `%ProgramFiles%\<name>\<name>.exe` (and the x86 folder),
  `%LOCALAPPDATA%\Programs\<name>\<name>.exe`, the Store's execution alias
  `%LOCALAPPDATA%\Microsoft\WindowsApps\<name>.exe`, then each `PATH`
  entry — never the working directory; a path is taken only when absolute.
  A non-admin `.exe` is then started through the daemon (`flow spawn`), so
  its first window opens on the focused screen; when `flow spawn` cannot be
  run or exits non-zero the script starts it itself.
  Admin launches use the shell's `RunAs` verb, so UAC prompts as usual; a
  declined prompt is not reported. Programs start in the home folder.
- **Exit codes without blocking a hotkey:** `DispatchDone` opens the
  process with `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE` (enough to
  read the exit code) and polls `WaitForSingleObject` with a zero timeout.

## Mod + left-button drag

Hold Mod and press the left button anywhere inside a window, then move: a
tiled window drops into the zone under the cursor when the button is
released (the other tiles reflow while you drag, as with a title-bar drag),
a floating window just moves. The Win key may be released before the button;
the drag goes on until the button is up.

How it works (`ModDrag()` in `flow.ahk`):

1. **The window under the cursor** is the first entry of `WinGetList()`
   (top-down z-order) whose rect contains the point, skipping the daemon's
   `FlowBorderOverlay` windows (they sit above every managed window, so
   `MouseGetPos` / `WindowFromPoint` would return one of them), click-through
   windows (`WS_EX_TRANSPARENT`, e.g. the OSD), DWM-cloaked windows (other
   virtual desktops), the desktop (`Progman`, `WorkerW`) and the taskbar. A
   maximized window is not dragged.
2. The window is activated. The script then waits for the cursor to leave
   an **8 px circle** around the press point while the button is still
   held (`DragDeadZonePx`, niri's grab threshold); a release inside it is a
   plain Mod+click that activated the window, and nothing is sent to the
   daemon. Only then is **`flow dispatch drag begin`** run synchronously
   (`RunWait`, hidden). The daemon puts the focused window into the state a
   title-bar grab would give it; a non-zero exit (a drag already in
   progress, nothing draggable focused, tiling off, daemon gone) means no
   move loop — the click is simply swallowed.
3. **The script moves the window itself:** `while GetKeyState("LButton",
   "P")` it `WinMove`s the window by the cursor delta every ~10 ms. Windows'
   own move loop cannot be used — see the lesson below.
4. **`flow dispatch drag end`** commits the drop (or snaps the tile back) and
   records a float's final position.

Lessons:

- **A hook hotkey cannot hand the gesture to Windows.** `#LButton` swallows
  the button-down, and every way of starting the native title-bar move loop
  afterwards ends it before any movement: `WM_NCLBUTTONDOWN` / `HTCAPTION`,
  `WM_SYSCOMMAND` `SC_MOVE` (`0xF010` and `0xF012`), re-injecting a
  button-down once the window holds capture, a decoy window taking the
  injected click. Windows checks the physical button state as the loop
  starts and sees it up. A manual `WinMove` loop is the only thing that
  moves the window, hence `drag begin` / `drag end` in the daemon.
- **Start menu.** AutoHotkey's automatic Win-key masking covers keyboard
  chords; for a mouse hotkey the script sends its own masking key
  (`Send "{Blind}{vkE8}"`, an unassigned virtual key) as the very first thing,
  so Windows has seen "another key" between Win-down and Win-up and does not
  open Start on release — whether the Win key goes up before or after the
  button.
- **Screen coordinates.** AutoHotkey v2 defaults mouse coordinates to the
  active window's client area, so `ModDrag()` sets `CoordMode("Mouse",
  "Screen")` before `MouseGetPos`: `WindowAt()` and `WinMove` reason in the
  same screen space as `WinGetPos`. Without it the grab point is read
  relative to the focused window and lands in the wrong window (it
  happened to work only when the active window sat near the screen origin).
- **Elevated windows** (UIPI) cannot be moved by this script unless it runs
  elevated too. The daemon leaves them unmanaged (`Ignored(Elevated)`,
  ADR-0025), so `drag begin` refuses them and the click is swallowed.
- **The dead zone belongs in the script.** Before it, every Mod+click opened
  and closed a `drag` session, and on a tile the daemon re-committed the
  layout for a gesture that never moved. The threshold is where the cursor
  is read; a session that never began needs no `drag end`.

## Launcher

Needs nothing from the daemon — it only uses `ShowCentered()` and
`AhkLog()` from `flow.ahk` and still works when `flow` itself is
unavailable. Showing it again while it is open hides it. Keys while it is
active: Up/Down/PgUp/PgDn move the selection (Tab / Shift+Tab too), Enter
runs, Esc hides, double-click runs. Enter runs or activates the selection;
with nothing matched, the typed text is passed to `Run` as a command or
path.

**Query syntax.** The query searches one kind of entry, chosen by a
prefix: `app - x`, `window - x` or `file - x` (any case; the spaces
around the dash are optional). Without a prefix it is an app query, so
`jellyfin-tui` and `app - jellyfin-tui` are the same search. An empty
text after the prefix lists the whole kind by name. Enter with nothing
matched passes the text after the prefix to `Run`.

**Index.** *Apps:* every `.lnk` under the user and common Start menus
(`A_StartMenu`, `A_StartMenuCommon`, `A_Programs`, `A_ProgramsCommon`,
nested roots skipped), deduplicated by display name case-insensitively,
`Uninstall*` skipped, rebuilt lazily every 60 s; plus Settings, Lock and
Task Manager as built-ins. *Windows:* the open top-level windows (empty
titles, tool windows, cloaked windows — other virtual desktops, suspended
UWP — the shell's own frames and the script's own windows are skipped),
rebuilt on every show. *Files:* scanned the first time a `file - ` query
asks and kept for 60 s: every file under `LauncherFileRoots` (Desktop,
Documents, Downloads, Pictures, Videos, Music) down to `LauncherFileDepth`
(3) folder levels, hidden and system entries and dot-folders skipped, at
most `LauncherFileCap` (5000) files in all; plus the targets of the
shell's Recent shortcuts that still exist and are not folders. The three
constants sit at the top of `launcher.ahk`. A file is matched on its name
only and the row shows the tail of its folder; Enter opens it with its
default program.

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
`LauncherFilter` runs `Critical`: AutoHotkey lets a later `Change` event
interrupt a running one, and without it the first-character refilter of
a pasted or fast-typed query could finish *after* the full-text one and
overwrite the list with its stale rows (seen in the VM, 2026-10-26).

## Selection buffer

The X11 "primary selection": whatever you highlight with the mouse is
remembered in a buffer of its own, and a middle click pastes it. The
clipboard (Ctrl+C / Ctrl+V, Win+V history) is left as it was, and there is
no history: the buffer holds one text. Off by default; to turn it on:

```toml
[keymap]
selection_buffer = true
```

It applies when `flow.toml` is saved (the keymap re-reads the daemon's
listing, which carries the option as an `option selection_buffer 0|1`
record); turning it off forgets the buffer. With it off, `selection.ahk`
registers no hotkey at all. It is part of `flow.ahk` (an `#Include`), so it
starts and stops with the keymap.

**Capture.** On a left-button release after a drag of more than 8 px, or
after a double or triple click (Shift+click too), the script asks UI
Automation for the selected text of the focused element, then of the
element under the pointer, then of their ancestors (a web page keeps its
selection on its Document; a chat-style page focuses a scroller that has no
text). The first press in a browser window wakes its accessibility tree
(Firefox builds it on the first request), and a capture right after that
which finds nothing is read once more 400 ms later. A classic Edit control (Win32 dialogs, a WinForms `TextBox`)
often answers UI Automation through its own MSAA object, which has no text
pattern; for the focused one the script reads `EM_GETSEL` and `WM_GETTEXT`
instead. It never uses the clipboard (Alacritty aside, below). An empty answer leaves the
buffer as it was. Nothing is read from a password field (UIA `IsPassword`).
A selection made with the keyboard is not captured.

**Alacritty.** Alacritty draws its own text and gives UI Automation
nothing to read, so the script uses Alacritty's copy-on-select instead.
Turn that on in `%APPDATA%\alacritty\alacritty.toml`:

```toml
[selection]
save_to_clipboard = true
```

Alacritty then writes the highlighted text to the clipboard when you
release the button (drag, double click, triple click, Shift+click). On the
first press in an Alacritty window the script saves the clipboard
(`ClipboardAll`). After the release it waits up to 300 ms for Alacritty's
write, takes that text into the buffer and puts the saved clipboard back,
so the clipboard ends up as it was. If something else writes to the
clipboard between the read and the restore (your own Ctrl+Shift+C, say),
that write is kept and nothing is restored. Ctrl+Shift+C after a highlight
still copies as usual.

What you see:

- For a moment (under 100 ms) the clipboard holds the highlighted
  text. A clipboard manager or Win+V history may record it: that is
  Alacritty's own write, which the script cannot mark. The restore is an
  ordinary write too, so history may show the earlier entry again on top.
- With `save_to_clipboard` off (Alacritty's default), Alacritty writes
  nothing. The buffer stays as it was and the trace says `alacritty did not
  copy within 300 ms; buffer kept`.
- Every first left press in Alacritty saves the clipboard once. A large
  clipboard (an image) makes that a copy of the image.

**Paste.** A middle click with no modifier, with a non-empty buffer:

| Under the pointer | What happens |
|---|---|
| A terminal (Windows Terminal, conhost, Alacritty, WezTerm, mintty, PuTTY, ConEmu) | The window is activated and the buffer pasted. |
| A text field: UIA `Edit` that is not read-only, or a `Document` that says it is writable (ValuePattern), or (outside browsers) a focusable `Document` with a text pattern; up to three parents of the element hit are tried | A left click there (focus, caret under the pointer), then the buffer is pasted. |
| A password field, a read-only field, anything else (a link, a button, a page in Edge / Chrome / Firefox / Brave / Vivaldi / Opera) | The middle click is passed on unchanged (replayed: press now, release with the button, so autoscroll still works). |
| Remote Desktop (`mstsc.exe`, `msrdc.exe`), Hyper-V's `vmconnect.exe`, the taskbar, the desktop, the keymap's own windows | Not even looked at: the middle click is the app's. |

With an empty buffer, or with Ctrl / Shift / Alt / Win held, the middle
click is never touched. Pasting puts the buffer on the clipboard (marked
`ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory = 0`,
`CanUploadToCloudClipboard = 0`, so Win+V history and the cloud clipboard
ignore it), sends the paste key (Shift+Insert; Ctrl+Shift+V for Alacritty),
waits 300 ms and puts the saved clipboard back (`ClipboardAll`). The restore
is an ordinary clipboard write: history may show the original entry again
on top.

**Limits.**

- Apps that draw their own text and expose no UIA text pattern give
  nothing: most games, some Electron / custom editors (VS Code's editor
  surface). A highlight there is not captured. Alacritty is the exception, through its
  copy-on-select (above). Its selection extended with the right button,
  or made with the keyboard (vi mode), is not captured, and Alacritty's
  write then stays on the clipboard.
- A web page that turns text selection off (`user-select: none`, common
  on app login screens) has nothing to capture. When a highlight is not
  captured, the trace line names the element types that were asked
  (`no selected text (focus ...; pointer ...)`).
- An app whose text field is not an UIA `Edit` / `Document` under the
  pointer gets its normal middle click instead of a paste.
- An elevated window: UI Automation and the paste keys are refused (UIPI);
  nothing is captured and nothing is pasted, silently.
- A copy through Ctrl+C with the clipboard restored afterwards is not built
  (a fallback for apps without a text pattern); ask if you want it.
- The trace (`keymap_log`) records `selection:` lines with lengths and
  executable names, never the text.

Why it is built this way: `docs/adr/0026-selection-buffer.md`.

## Tracing

The script traces every dispatch and its exit code, the liveness gate
transitions, the Remote Desktop hook re-installs (`rdp:` lines), the
launcher's fuzzy self-check and every refilter (query and top row), and the
OSD messages, with millisecond times. The trace is off by default, since it
records launcher queries. To turn it on for the script the daemon starts
(`flow start --ahk`), add to `flow.toml`

```toml
[logging]
keymap_log = true
```

and restart the daemon: it then writes to `logs\flow-ahk-YYYY-MM-DD.log`
next to the daemon's `flowd-YYYY-MM-DD.log` (same folder, same
`[logging] keep_days`). Setting `FLOW_AHK_LOG=<path>` in the environment
also turns it on and sends it elsewhere (it wins over `keymap_log`), and is
the way to trace a script started by hand.
