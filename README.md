# FlowWM — multi-monitor fork

A scrolling tiling window manager for Windows, in the style of [niri](https://github.com/YaLTeR/niri): windows are columns on a canvas wider than the screen, and the view slides sideways between them instead of cramming everything into a grid.

FlowWM is written by [CCpcalvin](https://github.com/CCpcalvin/flow-wm) (MIT). Upstream's README explains the idea, the motivation and the comparison with GlazeWM and komorebi, and upstream's book at <https://ccpcalvin.github.io/flow-wm/> is the reference for the parts this fork did not touch. This repository is the fork: it takes upstream's single-ultrawide design to a two-monitor desktop, adds a Windows-key keymap that follows niri's defaults, and fixes what a daily driver runs into. Everything below is about those changes.

> **Status:** in daily use on the author's two-screen work box; developed on branch `feat/multi-monitor`, with `master` tracking upstream. Every change is proven in a Hyper-V guest with two virtual displays before it reaches a real desktop.

## What the fork changes

| Area | What changed |
|---|---|
| **Multiple monitors** | Every monitor has its own scrolling strip and its own workspaces. `focus-monitor` and `move-column-to-monitor` cross screens; columns scrolled out of view park beyond the virtual screen, never on the neighbour. Each screen's work area is measured on its own, so a vertical taskbar on one screen is respected. Monitors can be plugged and unplugged while the daemon runs. |
| **Column widths** | `[columns] presets = ["1/3", "1/2", "1/1"]` and slot patterns such as `"2/3+1/3"`: `Win+R` cycles the width of every column on the focused screen, each screen can have its own list, and a column moved to another screen takes that screen's width. |
| **Stacked columns** | `[columns] visible_rows` / `max_rows`: a column shows two rows at a time and scrolls through up to four. `Win+[` and `Win+]` merge the window into the neighbouring column or pop it out again. |
| **Book layout** | `[columns] layout = "book"` (per screen too, or `Win+Shift+B` at runtime) piles the columns out of view behind the leftmost and rightmost visible ones, so every window stays on screen and one scroll flips the next card to the top. `wrap_around` lets focus wrap round the strip. |
| **Fullscreen** | `Win+F` makes the window's column fullscreen as niri does: it covers the whole screen including the taskbar, focus moves through it like any other column, and `Win+F` again puts it back. Apps with a full screen of their own (Remote Desktop) are asked for it instead. |
| **Floating layer** | `[floating] always_on_top`: floating windows, and dialogs owned by a managed window, stay above the tiled strip so nothing hides behind the tiles. `Win+V` floats or tiles the focused window, `Win+Shift+V` switches focus between the two layers. |
| **New windows** | A new window opens on the focused screen, already in its slot: programs started from the keymap, by `flow spawn` or by `[startup] spawn` are asked to put their first window there, so nothing flashes on the other screen first. |
| **Minimize and restore** | `Win+M` minimizes. A restored tile goes back to the column and row it left, the view scrolls to it, and a window that hid itself to the tray comes back the same way. |
| **Windows that were already open** | Windows that exist before the daemon starts are adopted, maximized ones included; Store apps are picked up once they are uncloaked. No reopening apps so the tiler sees them. |
| **Elevated windows** | A window running as administrator while FlowWM is not is left alone, since Windows refuses to let FlowWM move it. |
| **Borders** | The ring follows the window's own shape: a frameless window such as Alacritty without decorations gets a square ring. Windows that grow their frame late (SSMS, Visual Studio) are re-measured when placed, so there is no gap between app and ring. |
| **Mouse** | Win + left-button drag moves a tile to a new slot, on any screen, with a drop preview. Win + mouse wheel mirrors the arrow chords: wheel alone moves focus, with Shift moves the column, with Ctrl changes screen, with Alt moves the column to the other screen. |
| **Selection buffer** | An opt-in X11-style buffer (`[keymap] selection_buffer`): highlight text with the mouse, then middle-click to paste it, without touching the clipboard. Works in terminals, editors and browsers through UI Automation; Alacritty needs its copy-on-select option. |
| **Key bindings are configuration** | Chords live in a `[binds]` table in `flow.toml`, in niri's spelling, laid over a built-in default table. The keymap registers whatever the daemon lists and re-reads it when the file is saved. |
| **Fail-safe exit** | Every way the daemon can stop, including a crash, puts the desktop back the way Windows had it: borders gone, windows restored, nothing left parked off-screen. Tiling can be paused without stopping. |
| **Hardening** | The IPC pipe has a real ACL, rejects remote clients and caps message size; the daemon trusts only its own user and only files it wrote; window titles are logged at debug level only and logs are pruned. The self-updater is gone: `flow update` only reports whether the repository has something newer than the commit the binaries were built from. |

Each row has a design record in `docs/adr` (ADR-0007 onwards) and a page in the developer guide under `docs/src/dev-guide`; `docs/src/dev-guide/design-decisions.md` is the index.

## The keymap

`keymaps/win-niri/` is an AutoHotkey v2 keymap with the Windows key as the modifier and the arrow keys instead of hjkl, following niri's defaults. niri writes the modifier as `Mod` because it may be Super or Alt depending on where niri runs; here it is always the Windows key, in a virtual machine too, so this table says `Win`. In the `[binds]` table it is still spelled `Mod`, niri's spelling. The chords Windows reserves below the hook layer stay with Windows: `Win+L` always locks.

| Chord | What it does |
|---|---|
| `Win+Arrows` | Focus the column to the left or right, the window above or below |
| `Win+Shift+Arrows` | Move the column or window that way |
| `Win+Ctrl+Left/Right` | Focus the other screen |
| `Win+Alt+Left/Right` | Move the column to the other screen |
| `Win+Wheel` and the same modifiers | The same as the Left and Right arrow chords |
| `Win` + left-button drag | Move the tile to a new slot, on either screen, with a drop preview (hardwired, not in `[binds]`) |
| `Win+1..9`, `Win+0` | Workspace 1 to 10 on the focused screen; with `Ctrl`, move the column there |
| `Win+R` | Cycle the column width presets |
| `Win+F` | Fullscreen the column |
| `Win+V`, `Win+Shift+V` | Float or tile the window; switch focus between the layers |
| `Win+[`, `Win+]` | Merge into, or pop out of, the neighbouring column |
| `Win+C` | Center the column |
| `Win+M`, `Win+Q` | Minimize, close |
| `Win+Shift+B` | Toggle the book layout on this screen |
| `Win+T`, `Win+Shift+T` | Alacritty, Alacritty as administrator (`Win+Alt+T` for Windows Terminal) |
| `Win+Space` | Fuzzy launcher over the Start menu, open windows and files |
| `Win+Shift+/` | Key-binding overlay, built from the same list the hotkeys come from |
| `Win+Ctrl+Shift+T` | Pause tiling; the daemon keeps running |
| `Win+Ctrl+T` | FlowWM off and on again (through the resident starter) |
| `Win+Shift+E` | Quit |

`Alt+hjkl` and `Alt+Shift+hjkl` are bound too, for upstream's muscle memory. Any chord can be changed, added or set to `"none"` in `[binds]`, except the drag and the two `Win+Ctrl` switches, which are hardwired; the full grammar and action list are in the user guide's config reference.

## Install

Rust MSVC toolchain and the Visual Studio "Desktop development with C++" workload, as for upstream. Then:

```powershell
git clone -b feat/multi-monitor https://github.com/darth-feir/flow-wm-multi-monitor.git
cd flow-wm-multi-monitor
cargo build --release --locked
target\release\flow.exe config init   # writes %USERPROFILE%\.config\flow\flow.toml and flow-rules.toml
```

1. Copy `flow.exe` and `flowd.exe` from `target\release\` together into one folder, for example `%LOCALAPPDATA%\Programs\flow`. `flow start` looks for `flowd.exe` next to `flow.exe`, and the keymap looks for `flow.exe` in that folder.
2. Copy every `.ahk` file from `keymaps\win-niri\` into `%USERPROFILE%\.config\flow\`. They include each other, so they must sit together.
3. Merge `keymaps\win-niri\flow.toml.example` into your `flow.toml`. It carries the width presets, the stack rows, `layout` and `wrap_around`, `adopt_maximized`, `always_on_top`, `new_window_monitor`, and commented `[binds]`, `[keymap]` and `[startup]` examples.
4. Install [AutoHotkey v2](https://www.autohotkey.com/), then `flow start --ahk`. For every login, `flow enable-autostart --ahk` writes one shortcut into your Startup folder. A shortcut to `flow-resident.ahk` in the Startup folder instead gives you `Win+Ctrl+T` as an on/off switch for the whole thing.

`flow config check` reports every problem in both config files without applying anything; `flow query binds` shows the resolved chord table; `flow query all` shows what the daemon sees.

Upstream's `cargo install flow-wm` and its bundled Alt-based `flow.ahk` install upstream FlowWM, not this fork.

## How it is tested

Every stage of the fork was built and proven in a Hyper-V guest with two virtual 1920×1080 displays before it was installed on a real desktop: each change ships with test cases that fail on the previous build and pass on the new one, plus the regression suites of the earlier stages. The daemon never has to run on a developer's own machine to be verified. The harness is not part of this repository.

## Docs and contributing

The fork's guides live in `docs/src` and build with `mdbook build docs`. The published book linked above is upstream's and does not cover the fork's pages. Issues and pull requests go to [darth-feir/flow-wm-multi-monitor](https://github.com/darth-feir/flow-wm-multi-monitor), against `feat/multi-monitor`; see [Contributing](docs/src/user-guide/contributing.md) for the workflow and [Building from source](docs/src/dev-guide/building-from-source.md) for the gates:

```powershell
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

MIT, as upstream: see [LICENSE](LICENSE). FlowWM is CCpcalvin's work; the fork's changes are listed above and recorded in `docs/adr`.
