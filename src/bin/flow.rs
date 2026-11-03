//! flow — FlowWM CLI client.
//!
//! Sends commands to the `flowd` daemon via a Windows named pipe. Commands fall
//! into four groups:
//!
//! | Group | Commands |
//! |-------|----------|
//! | Lifecycle | `start`, `stop`, `ping`, `enable-autostart`, `disable-autostart` |
//! | Loadout | `loadout save|load [path]` |
//! | Config | `config init` / `reload` / `edit` / `path` / `check` |
//! | Query | `query all` |
//! | Dispatch | `dispatch focus\|swap-column\|move-window\|merge-column\|promote\|consume-or-expel\|expand-column\|shrink-column\|width\|center\|close-window\|minimize-window\|set-window\|switch-workspace\|move-to-workspace`, plus stub `swap-workspace` |
//!
//! See the developer guide's *IPC & Watchdog* chapter
//! (`docs/src/dev-guide/ipc-and-watchdog.md`) for the full command reference.
//!
//! # Configuration
//!
//! The config directory is resolved via a priority chain:
//!
//! 1. `--config <dir>` flag on `flow start` (passed to the daemon via the
//!    `FLOW_CONFIG_DIR` env var)
//! 2. `FLOW_CONFIG_DIR` environment variable
//! 3. Default: `%USERPROFILE%\.config\flow\`
//!
//! The `flow config init/reload/edit/path/check` commands resolve the config
//! directory without contacting the daemon — they operate on local files only.
//! Only `flow config reload` sends an IPC message to the running daemon.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use clap::{Parser, Subcommand};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

use flow_wm::autostart;
use flow_wm::build_info;
use flow_wm::common::Direction;
use flow_wm::config;
use flow_wm::daemon::startup::{self, StartupState, StartupStatus};
use flow_wm::ipc::message::LayoutRequest;
use flow_wm::ipc::message::SocketResponse;
use flow_wm::ipc::message::WidthPresetRequest;
use flow_wm::ipc::message::WindowMode;
use flow_wm::ipc::message::{LayerTarget, SocketMessage};
use flow_wm::ipc::transport;
use flow_wm::logging;
use flow_wm::rescue;
use flow_wm::updater;

/// Maximum time to wait for the daemon to become ready after spawning.
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval between readiness polls in [`wait_for_daemon`].
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Parser)]
#[command(name = "flow", version = build_info::version_line_static(), about = "FlowWM CLI")]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Top-level commands for the flow CLI client.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the flowd daemon in the background.
    Start {
        /// Config directory path. Overrides FLOW_CONFIG_DIR env var and default path.
        #[arg(long)]
        config: Option<String>,
        /// Optional log file path.
        ///
        /// Forwarded to the spawned daemon as `--log-file`. When set, flowd
        /// redirects all of its logging to this exact file (truncated on each
        /// start) instead of the default date-stamped log — useful for
        /// capturing a clean, isolated debug log for a single run. The log
        /// level comes from `--verbose` / `--log-level` (else `RUST_LOG`).
        #[arg(long, value_name = "PATH")]
        log_file: Option<String>,
        /// Also launch the user's `flow.ahk` keybinding script once the daemon
        /// is ready. Forwarded to the daemon (`flowd --ahk`), which starts the
        /// AutoHotkey v2 interpreter inside a kill-on-close job object: if the
        /// daemon dies for any reason the script dies with it and the Win key
        /// is released. `flow stop` always stops it.
        #[arg(long)]
        ahk: bool,
        /// Skip restoring the last saved loadout after the daemon starts.
        ///
        /// By default `flow start` asks the daemon to restore the most recent
        /// loadout. Pass `--no-restore` to start with whatever windows are
        /// already on screen, untiled.
        #[arg(long)]
        no_restore: bool,
        /// Run the daemon at `debug` log level (default is `info` in release
        /// builds). Forwarded as `flowd --verbose`. Shorthand for
        /// `--log-level debug`.
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Explicit daemon log level / filter (e.g. `trace`,
        /// `flow_wm::registry=trace`). Forwarded as `flowd --log-level`;
        /// overrides `--verbose` and `RUST_LOG`.
        #[arg(long, value_name = "LEVEL")]
        log_level: Option<String>,
    },
    /// Stop the running flowd daemon (and the hotkey script, if one is
    /// recorded in `flow.ahk.pid`). Returns once the daemon process has
    /// exited, so `flow start` can follow immediately.
    Stop {
        /// Config directory path (where `flowd.status` and `flow.ahk.pid`
        /// live). Overrides FLOW_CONFIG_DIR env var and default path; resolve
        /// it the same way the daemon was started.
        #[arg(long)]
        config: Option<String>,
        /// Accepted for compatibility; the hotkey script is always stopped.
        #[arg(long, hide = true)]
        ahk: bool,
    },
    /// Bring every off-screen window back onto a monitor — without a daemon.
    ///
    /// Use it after a crash or `taskkill` left windows parked beyond the
    /// screen edge. `flow start` runs the same pass before launching the
    /// daemon. Refuses to run while a daemon is answering (it would fight
    /// the daemon's own layout) unless `--force` is given.
    Rescue {
        /// Run even if a daemon is currently answering on the pipe.
        #[arg(long)]
        force: bool,
    },
    /// Check that the daemon is alive: prints its version, pid, monitor
    /// count, registered-hook count and — when the daemon launched the
    /// hotkey script — the flow.ahk pid. Exits non-zero if it does not
    /// answer.
    Ping,
    /// Save or restore a window arrangement (a "loadout").
    Loadout {
        #[command(subcommand)]
        command: LoadoutCommands,
    },
    /// Manage configuration files.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Query daemon state.
    Query {
        #[command(subcommand)]
        command: QueryCommands,
    },
    /// Dispatch a command to the daemon (focus, swap, scroll, etc.).
    ///
    /// Use `flow dispatch help` to see available subcommands. New action
    /// categories will be added here as needed.
    Dispatch {
        #[command(subcommand)]
        command: DispatchCommands,
    },
    /// Start a program so that its first window opens on the focused
    /// screen (the keymap's spawn binds use this).
    ///
    /// The daemon resolves PROGRAM like a `[startup] spawn` entry (Program
    /// Files, %LOCALAPPDATA%\Programs, the Store's aliases, PATH; a path
    /// must be absolute), starts it from the home folder and asks its first
    /// window to open on the active monitor. Put `--` before the program
    /// when an argument starts with `-`. Exits non-zero when the daemon does
    /// not answer or cannot start the program.
    Spawn {
        /// The program and its arguments.
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "PROGRAM [ARGS]..."
        )]
        argv: Vec<String>,
    },
    /// Create the login autostart shortcut in `shell:startup`.
    EnableAutostart {
        /// Bake `--ahk` into the shortcut's args so login also launches
        /// `flow.ahk` alongside the daemon.
        #[arg(long)]
        ahk: bool,
    },
    /// Remove the login autostart shortcut from `shell:startup`.
    DisableAutostart,
    /// Check GitHub for something newer than this build; never installs.
    ///
    /// Compares the commit this binary was built from with the `[update]`
    /// branch (default: the build's branch of the repository its `origin`
    /// pointed at), or the crate version with the latest release when
    /// `branch = ""`. Prints the result and how to update. Safe to run
    /// while the daemon is up. Exit codes: 0 up to date, 10 something
    /// newer exists, 2 api.github.com unreachable, 3 repository / branch /
    /// release not found, 4 other GitHub error, 5 no commit embedded.
    Update {
        /// Accepted for compatibility; `flow update` only ever checks, so
        /// this changes nothing.
        #[arg(long)]
        check: bool,
    },
}

/// Configuration management subcommands.
///
/// These commands (except `reload`) operate on local config files without
/// contacting the daemon. Only `reload` sends an IPC message.
#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Initialize config directory with default files.
    ///
    /// Pass `--ahk` to also write `flow.ahk`, a ready-to-use AutoHotkey v2
    /// keybinding script that drives FlowWM via the `flow` CLI. See the
    /// developer guide's *IPC & Watchdog* chapter
    /// (`docs/src/dev-guide/ipc-and-watchdog.md`) for the command reference.
    Init {
        /// Also write `flow.ahk` (a bundled AutoHotkey v2 keybinding script)
        /// into the config directory. Existing files are never overwritten.
        #[arg(long)]
        ahk: bool,
    },
    /// Reload daemon configuration from disk.
    Reload,
    /// Open config directory in the system editor.
    Edit,
    /// Print the resolved config directory path.
    Path,
    /// Validate configuration files.
    Check,
}

/// Query subcommands.
#[derive(Debug, Subcommand)]
enum QueryCommands {
    /// Dump all tracked windows with full debug info (state, rect, col/row, etc.).
    All,
    /// The key bindings the daemon resolved from `[binds]` (or the keymap's
    /// built-in table): chord, AutoHotkey hotkey, action, what it runs, and
    /// the help-overlay rows. JSON by default; `--plain` prints the
    /// tab-separated listing the hotkey script reads.
    Binds {
        /// One record per line, tab-separated (what `flow.ahk` parses).
        #[arg(long)]
        plain: bool,
    },
}

/// Dispatch subcommands — one per action category.
///
/// Each variant wraps a further subcommand tree so the CLI stays organized as
/// more actions are added (swap-column, swap-window, etc.).
///
/// # Layout pipeline
///
/// Every dispatch command that changes layout flows through the same 3-step
/// pipeline inside the daemon:
///
/// 1. **Mutate** the virtual layout (e.g. widen the focused column).
///    Widening a column naturally pushes every column to its right further
///    along the virtual canvas — no explicit per-window shift is needed.
/// 2. **Project** the virtual layout into actual screen coordinates, adjusting
///    the viewport so the focused column stays visible (`ensure_column_visible`).
/// 3. **Animate** the new actual layout — the animation layer compares each
///    window's target rect against its real on-screen position and tweens
///    only the windows that differ.
///
/// The CLI's only job is to send the right [`SocketMessage`]; the daemon
/// owns the entire pipeline above.
#[derive(Debug, Subcommand)]
enum DispatchCommands {
    /// Focus a window in the given direction.
    Focus {
        #[command(subcommand)]
        direction: FocusDirection,
    },
    /// Swap the focused column with its left/right neighbour.
    SwapColumn {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },
    /// Move the focused window in the given direction.
    ///
    /// This is a semantic command — the daemon decides what "move" means
    /// based on window state and direction:
    /// - tiled left/right → column swap (cross-column move is deferred);
    /// - tiled up/down → within-column window swap;
    /// - floating → pixel nudge (deferred).
    MoveWindow {
        #[command(subcommand)]
        direction: MoveDirection,
    },
    /// Merge the focused window's row into the adjacent column.
    ///
    /// Maps to [`SocketMessage::MergeColumn`]. The focused window is
    /// detached from its column and appended as a new bottom row of the
    /// neighbour column; both columns' row heights are redistributed.
    MergeColumn {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },
    /// Promote the focused window out of its column into a new standalone column.
    ///
    /// Maps to [`SocketMessage::Promote`]. The focused window is extracted
    /// into a new single-row column placed to the left or right of the
    /// source column. No-op when the window is already alone in its column.
    Promote {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },
    /// Consume the focused window into the neighbouring column, or expel it
    /// into a column of its own.
    ///
    /// Maps to [`SocketMessage::ConsumeOrExpel`]. When the focused window
    /// shares its column with other windows this is `promote <dir>`; when
    /// it is alone in its column it is `merge-column <dir>`. An error when
    /// there is no neighbour to consume into.
    ConsumeOrExpel {
        #[command(subcommand)]
        direction: HorizontalDirection,
    },

    /// Expand the focused column width by one column step.
    ///
    /// Sends [`SocketMessage::ExpandColumn`]. The daemon widens the focused
    /// column to the next `column_width` boundary and animates the result.
    ExpandColumn,
    /// Shrink the focused column width by one column step.
    ///
    /// Sends [`SocketMessage::ShrinkColumn`]. The daemon narrows the focused
    /// column to the previous `column_width` boundary and animates the result.
    ShrinkColumn,
    /// Switch the focused monitor to another column width preset.
    ///
    /// Sends [`SocketMessage::SetWidthPreset`]. `PRESET` is `next`, `prev`,
    /// `cycle` (same as `next`, wrapping) or a preset from `[columns]
    /// presets` in flow.toml such as `1/2`. Every column on every workspace
    /// of the focused monitor takes the new width, and new columns there
    /// are created at it. Prints the preset the monitor landed on.
    Width {
        /// `next`, `prev`, `cycle` or a configured preset such as `1/2`.
        preset: String,
    },
    /// Switch the focused window's screen to strip or book layout.
    ///
    /// Sends [`SocketMessage::SetLayout`]. `MODE` is `strip` (columns out
    /// of view parked off-screen), `book` (piled behind the edge columns)
    /// or `toggle`. Every workspace of that screen switches; a config
    /// reload puts it back to `[columns] layout` in flow.toml. Prints the
    /// mode the screen landed on.
    SetLayout {
        /// `strip`, `book` or `toggle`.
        mode: String,
    },
    /// Center the viewport so the focused column lands at the monitor midpoint.
    ///
    /// Sends [`SocketMessage::Center`]. The daemon slides the viewport so the
    /// focused column lands at the monitor midpoint using the variable-width
    /// prefix sum — works correctly even with expanded or shrunk columns.
    Center,
    /// Focus the floating windows of the active workspace: the one used
    /// last, else the top-most. Sends [`SocketMessage::FocusLayer`]; an
    /// empty layer is reported and exits 0.
    FocusFloating,
    /// Focus the tiled windows of the active workspace: the strip's
    /// last-focused window, else its first. Sends
    /// [`SocketMessage::FocusLayer`].
    FocusTiling,
    /// Focus whichever of the two is not focused now (niri's
    /// `switch-focus-between-floating-and-tiling`, Mod+Shift+V in the
    /// win-niri keymap). Sends [`SocketMessage::FocusLayer`].
    SwitchFocusBetweenFloatingAndTiling,
    /// Close the currently focused window.
    ///
    /// Sends [`SocketMessage::CloseWindow`]. The daemon asks the focused
    /// window to close itself gently via Win32 `WM_CLOSE` — the same message
    /// Windows sends when the user clicks the window's ✕ button — so the
    /// application can run its normal shutdown logic (prompt to save unsaved
    /// work, release resources, etc.). The window is removed from the layout
    /// automatically once Win32 reports its destruction.
    CloseWindow,
    /// Minimize the currently focused window (an active tile or float).
    ///
    /// Sends [`SocketMessage::MinimizeWindow`]. The daemon asks Windows to
    /// minimize it (`SW_MINIMIZE`); it leaves the layout like a window the
    /// user minimized, the focus moves to its successor on the same screen,
    /// and a restore puts it back in its slot.
    MinimizeWindow,
    /// Set the focused window's tiling mode.
    ///
    /// Maps to [`SocketMessage::SetWindow`]. Sends
    /// `flow dispatch set-window float|tile|cycle|fullscreen` to the daemon,
    /// which transitions the focused window between floating, tiling and
    /// fullscreen modes (`fullscreen` toggles: a fullscreen window goes
    /// back where it came from).
    SetWindow {
        #[command(subcommand)]
        mode: WindowMode,
    },

    // --- Workspace (niri-style virtual desktop) ---
    //
    // These three subcommands form the CLI surface for the upcoming
    // vertical-scrolling workspace system. The daemon currently returns a
    // "not yet implemented" error for each — the protocol shape is locked in
    // now so keybindings and documentation can stabilise while the workspace
    // animation design is finalised.
    /// Switch the active workspace.
    ///
    /// Maps to [`SocketMessage::SwitchWorkspace`]. Sends
    /// `flow dispatch switch-workspace <id>` to the daemon, which slides the
    /// requested workspace into the viewport and parks the previously active
    /// one above or below it in a single coordinated animation.
    SwitchWorkspace {
        /// Identifier of the workspace to switch to (niri-style `u32`).
        workspace_id: u32,
    },
    /// Swap the active workspace with another workspace.
    ///
    /// Maps to [`SocketMessage::SwapWorkspace`]. Sends
    /// `flow dispatch swap-workspace <id>` to the daemon, which will
    /// (eventually) exchange the positions of the active workspace and the
    /// target in the monitor's vertical workspace stack, with focus following
    /// the originally active workspace.
    SwapWorkspace {
        /// Identifier of the workspace to swap with the active one.
        workspace_id: u32,
    },
    /// Move the focused window to another workspace.
    ///
    /// Maps to [`SocketMessage::MoveWindowToWorkspace`]. Sends
    /// `flow dispatch move-to-workspace <id>` to the daemon, which detaches the
    /// focused window from the active workspace's `ScrollingSpace` (with
    /// local focus succession — no OS foreground focus push) and re-inserts
    /// it into the target workspace's `ScrollingSpace` after its focused
    /// column. Focus stays on the source workspace.
    ///
    /// The CLI command name is the shorter `move-to-workspace` for brevity,
    /// even though the underlying IPC variant is `MoveWindowToWorkspace`
    /// (mirroring the sibling `move-window` operation).
    #[command(name = "move-to-workspace")]
    MoveWindowToWorkspace {
        /// Identifier of the destination workspace.
        workspace_id: u32,
    },
    /// Focus the monitor in the given direction (by screen geometry) and
    /// its last-focused window.
    #[command(name = "focus-monitor")]
    FocusMonitor {
        #[command(subcommand)]
        direction: FocusDirection,
    },
    /// Move the focused window to the monitor in the given direction. A
    /// tiled window joins that monitor's strip at the near edge; a float
    /// keeps its position relative to the work area. Focus follows.
    ///
    /// `move-window left/right` at the edge of a strip does the same hop.
    #[command(name = "move-to-monitor")]
    MoveWindowToMonitor {
        #[command(subcommand)]
        direction: FocusDirection,
    },
    /// Turn tiling off (hand the desktop back to Windows, every window
    /// moved back on-screen), on again, or toggle it — without stopping
    /// the daemon or the hotkey script.
    ///
    /// Maps to [`SocketMessage::SetTiling`] / [`SocketMessage::ToggleTiling`].
    /// While tiling is off the daemon never moves a window and every
    /// layout command is refused with `tiling is off`; `flow ping` and
    /// `flow query all` report the state. Prints `flow: tiling on` or
    /// `flow: tiling off` — the state after the command, so `toggle` tells
    /// you where it landed.
    Tiling {
        #[command(subcommand)]
        state: TilingSwitch,
    },
    /// Begin or end a keymap-driven drag of the focused window.
    ///
    /// Maps to [`SocketMessage::DragBegin`] / [`SocketMessage::DragEnd`].
    /// A hotkey script that moves a window itself (Mod + left-button drag)
    /// brackets the gesture with `drag begin` (the daemon enters the state a
    /// title-bar grab would give the focused window: a tile previews its
    /// drop zone, a float is left to the script) and `drag end` (the drop is
    /// committed or snapped back, a float's final position recorded).
    /// `begin` fails while another drag is in progress, when nothing
    /// draggable has the focus or while tiling is off; `end` without a
    /// session is a no-op that says so.
    Drag {
        #[command(subcommand)]
        action: DragAction,
    },
}

/// `flow dispatch drag <begin|end>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
enum DragAction {
    /// Start a session on the focused window; prints `flow: drag begun (tile|float)`.
    Begin,
    /// End the session; prints `flow: drag ended (tile|float)` or `flow: no drag session to end`.
    End,
}

/// Target state for `flow dispatch tiling <on|off|toggle>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
enum TilingSwitch {
    /// Resume tiling: rebuild the layout from the live desktop.
    On,
    /// Pause tiling: put every window back on-screen and go inert.
    Off,
    /// Flip between on and off.
    Toggle,
}

/// Cardinal direction for `flow dispatch focus <dir>`.
#[derive(Debug, Subcommand)]
enum FocusDirection {
    /// Focus the window to the left.
    Left,
    /// Focus the window to the right.
    Right,
    /// Focus the window above.
    Up,
    /// Focus the window below.
    Down,
}

/// Horizontal direction for `flow dispatch swap-column|merge-column|promote|consume-or-expel <dir>`.
///
/// Only left/right is offered: column swaps, merges, and promotes are
/// inherently horizontal operations between adjacent columns.
#[derive(Debug, Subcommand)]
enum HorizontalDirection {
    /// Left.
    Left,
    /// Right.
    Right,
}

/// Cardinal direction for `flow dispatch move-window <dir>`.
///
/// All four directions are accepted: left/right resolve to a column swap
/// (until a real cross-column move lands), and up/down resolve to a
/// within-column window swap.
#[derive(Debug, Subcommand)]
enum MoveDirection {
    /// Left.
    Left,
    /// Right.
    Right,
    /// Up.
    Up,
    /// Down.
    Down,
}

/// Loadout subcommands.
#[derive(Debug, Subcommand)]
enum LoadoutCommands {
    /// Save the current window arrangement to a loadout file.
    Save {
        /// Optional output path (a .json file, no `..`; relative to the
        /// config dir). Defaults to the config `loadout.default_path`.
        #[arg(value_name = "PATH")]
        path: Option<String>,
    },
    /// Restore a previously saved arrangement from a loadout file.
    Load {
        /// Optional input path (a .json file, no `..`; relative to the
        /// config dir). Defaults to the config `loadout.default_path`.
        #[arg(value_name = "PATH")]
        path: Option<String>,
    },
}

fn main() {
    // `flow dispatch <niri action>` (focus-column-left, focus-workspace 3)
    // is spelled into our own subcommand before clap sees it, so a bind
    // line and a shell command read the same.
    let args: Vec<String> = std::env::args().collect();
    let cli = match config::binds::niri_alias(&args) {
        Some(aliased) => Cli::parse_from(aliased),
        None => Cli::parse(),
    };

    let result = match cli.command {
        Commands::Start {
            config,
            log_file,
            ahk,
            no_restore,
            verbose,
            log_level,
        } => {
            let opts = StartOptions {
                config_override: config,
                log_file_override: log_file,
                ahk,
                no_restore,
                verbose,
                log_level,
            };
            match cmd_start(opts) {
                Ok(()) => Ok(()),
                Err(StartError { message, exit_code }) => {
                    eprintln!("flow: {message}");
                    std::process::exit(exit_code);
                }
            }
        }
        Commands::Stop { config, ahk: _ } => cmd_stop(config),
        Commands::Rescue { force } => cmd_rescue(force),
        Commands::Ping => cmd_ping(),
        Commands::Loadout { command } => cmd_loadout(command),
        Commands::Config { command } => cmd_config(command),
        Commands::Query { command } => cmd_query(command),
        Commands::Dispatch { command } => cmd_dispatch(command),
        Commands::Spawn { argv } => cmd_spawn(argv),
        Commands::EnableAutostart { ahk } => cmd_enable_autostart(ahk),
        Commands::DisableAutostart => cmd_disable_autostart(),
        Commands::Update { check: _ } => {
            std::process::exit(cmd_update());
        }
    };

    if let Err(e) = result {
        eprintln!("flow: {e}");
        std::process::exit(1);
    }
}

/// Options for `flow start`, mirroring the `Commands::Start` flags.
#[derive(Debug, Default)]
struct StartOptions {
    /// `--config <dir>`: config directory override.
    config_override: Option<String>,
    /// `--log-file <path>`: forwarded to the daemon.
    log_file_override: Option<String>,
    /// `--ahk`: forwarded to the daemon, which launches `flow.ahk` once ready.
    ahk: bool,
    /// `--no-restore`: forwarded to the daemon.
    no_restore: bool,
    /// `--verbose`: forwarded to the daemon.
    verbose: bool,
    /// `--log-level <lvl>`: forwarded to the daemon.
    log_level: Option<String>,
}

/// A `flow start` failure: the message to print (after `flow: `) and the
/// process exit code — the daemon's own stage exit code when it failed at a
/// known startup stage, else `1`.
#[derive(Debug, PartialEq, Eq)]
struct StartError {
    /// Message printed to stderr, without the `flow: ` prefix.
    message: String,
    /// Exit code for the `flow` process.
    exit_code: i32,
}

impl From<String> for StartError {
    fn from(message: String) -> Self {
        Self {
            message,
            exit_code: 1,
        }
    }
}

impl StartError {
    /// The user-facing failure line for a daemon that reported a failing
    /// startup stage: `FlowWM failed to start at step '<stage>': <reason>
    /// (log: <path>)`, exiting with that stage's code.
    fn at_stage(stage: &str, reason: &str, log_path: &Path, exit_code: i32) -> Self {
        Self {
            message: format!(
                "FlowWM failed to start at step '{stage}': {reason} (log: {})",
                log_path.display()
            ),
            exit_code,
        }
    }
}

/// What a successful daemon start looks like, for the final status line.
#[derive(Debug, PartialEq, Eq)]
struct StartSummary {
    /// Daemon pid (from the `Pong`, cross-checked against the spawned child).
    pid: u32,
    /// Monitor count reported by the daemon.
    monitors: usize,
    /// Registered WinEvent hook count reported by the daemon.
    hooks_registered: usize,
    /// Pid of the hotkey script the daemon launched (`--ahk`), if running.
    ahk_pid: Option<u32>,
}

/// Start the daemon and wait for it to become ready.
///
/// If a `--config <dir>` override is provided, sets the `FLOW_CONFIG_DIR`
/// environment variable before spawning the daemon process. The daemon reads
/// this variable on startup to locate its config files, so the override is
/// propagated transparently through process inheritance. The rest of the
/// current environment is inherited as well (`RUST_LOG`, `FLOW_PIPE_NAME`,
/// and the test-only `FLOW_FAIL_AT` fault injector all reach the daemon).
///
/// `--log-file`, `--no-restore`, `--verbose` and `--log-level` are forwarded
/// to the daemon as CLI arguments (see [`spawn_daemon`]); `--ahk` is
/// forwarded too, after checking that `flow.ahk` exists. Readiness is
/// decided by [`wait_for_daemon`]; the daemon-less rescue
/// ([`rescue::rescue_offscreen_windows`]) runs before spawning. See
/// `docs/src/dev-guide/ipc-and-watchdog.md`, *The `flow` CLI Client*.
///
/// # Design Decision
///
/// We use [`std::env::set_var`] rather than `Command::env()` because the
/// daemon is spawned via [`spawn_daemon`] which also needs to handle detached
/// process creation flags. Setting the env var in the current process ensures
/// it is inherited by the child regardless of the spawn path.
///
/// # Errors
///
/// Returns a [`StartError`] if:
/// - The daemon is already running.
/// - The user's `flow.toml` cannot be parsed (pre-flight config check fails).
/// - The daemon binary cannot be found.
/// - The daemon fails to spawn.
/// - The daemon reports a failed startup stage or exits early.
/// - The daemon does not become ready within [`DAEMON_START_TIMEOUT`].
/// - `ahk` is true and `flow.ahk` does not exist.
fn cmd_start(opts: StartOptions) -> Result<(), StartError> {
    // Set env var before any daemon interaction so the spawned child inherits it.
    if let Some(ref dir) = opts.config_override {
        // SAFETY: This is called in the CLI process before spawning the daemon
        // child. There are no other threads reading this env var at this point,
        // and the CLI is a short-lived process with no concurrent Rust code.
        unsafe { std::env::set_var(config::dirs::CONFIG_DIR_ENV, dir) };
    }

    if transport::is_daemon_running() {
        return Err("daemon is already running".to_owned().into());
    }

    // Pre-flight config check: surface `flow.toml`/`flow-rules.toml` errors on the
    // user's terminal before spawning, because the detached daemon's
    // stdout/stderr are discarded. Reuses the daemon's own loaders (same crate),
    // so the verdict matches — no desync between client and daemon.
    let config_dir =
        config::dirs::resolve_config_dir(opts.config_override.as_deref().map(Path::new));
    let config = preflight_config_check(&config_dir)?;

    if opts.ahk {
        let script = autostart::flow_ahk_path_in(&config_dir);
        if !script.exists() {
            return Err(format!(
                "flow.ahk not found at {}; run `flow config init --ahk` to create it",
                script.display()
            )
            .into());
        }
    }

    // A previous daemon that died without its shutdown rescue leaves windows
    // parked beyond the screen. Clean that up before the new daemon scans
    // them (it would otherwise adopt the parked rects as their anchors).
    match rescue::rescue_offscreen_windows() {
        Ok(report) if !report.moved.is_empty() => {
            println!(
                "flow: rescued {} off-screen window(s) left by a previous run",
                report.moved.len()
            );
        }
        Ok(_) => {}
        Err(e) => eprintln!("flow: warning: pre-start rescue failed: {e}"),
    }

    // Where the daemon will log, so a failure line can point at it even if
    // the daemon died before writing its status file.
    let expected_log_path = match opts.log_file_override.as_deref() {
        Some(p) => PathBuf::from(p),
        None => logging::default_log_path(&config_dir),
    };

    // A stale status file from a previous daemon must never be read as this
    // one's. The pid check in `wait_for_daemon` is the real guard; removing
    // the file first just avoids confusing a human looking at it.
    startup::remove_status(&config_dir);

    let mut child = spawn_daemon(&opts)?;
    let summary = wait_for_daemon(&mut child, &config_dir, &expected_log_path)?;
    // The daemon keeps running detached; dropping the handle does not kill it.
    drop(child);

    println!(
        "flow: daemon started (pid {}, {} monitor(s), log: {})",
        summary.pid,
        summary.monitors,
        expected_log_path.display()
    );
    // The daemon auto-restores the last loadout during its own init (it is
    // passed `--no-restore` when the user opted out), so no IPC round-trip
    // happens here — this sidesteps the startup pipe race.

    if opts.ahk {
        // The daemon launched the script itself (inside its job object)
        // after reporting ready; the Pong tells us whether it is running.
        match summary.ahk_pid {
            Some(pid) => println!("flow: flow.ahk running (pid {pid}, tied to the daemon)"),
            None => eprintln!(
                "flow: warning: the daemon could not launch flow.ahk (see the log); hotkeys are unavailable"
            ),
        }
    }

    maybe_notify_update(&config);

    Ok(())
}

/// `flow ping`: one `Ping` round-trip, printing the daemon's identity.
///
/// # Errors
///
/// Returns an error string if the daemon is not running, does not answer,
/// or answers with anything but `Pong`.
fn cmd_ping() -> Result<(), String> {
    let response = transport::send_message(&SocketMessage::Ping)
        .map_err(|e| format!("daemon not reachable: {e}"))?;
    match response {
        SocketResponse::Pong {
            version,
            pid,
            monitors,
            hooks_registered,
            ahk_pid,
            tiling_enabled,
            floating_layer,
        } => {
            let ahk = match ahk_pid {
                Some(p) => format!(", flow.ahk pid {p}"),
                None => String::new(),
            };
            let tiling = startup::TilingStatus::from_enabled(tiling_enabled);
            let layer = match floating_layer {
                Some(state) => format!(", floating layer {state}"),
                None => String::new(),
            };
            println!(
                "flow: pong from flowd {version} (pid {pid}, {monitors} monitor(s), {hooks_registered} hook(s) registered{ahk}, tiling {tiling}{layer})"
            );
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        other => Err(format!("unexpected reply to ping: {other:?}")),
    }
}

/// Best-effort start-time update notice, gated by `check_for_updates`.
/// Runs after the daemon is ready, bounded by a short timeout, silent on
/// every failure, prints one line only when something newer exists. See
/// `docs/src/dev-guide/updater.md`, *Start-time notice*.
fn maybe_notify_update(config: &config::FlowConfig) {
    if !config.check_for_updates {
        return;
    }
    if let Ok(report) = updater::check(&config.update)
        && report.verdict == updater::Verdict::Newer
    {
        println!("flow: {} — see \"flow update\"", report.summary);
    }
}

/// Run a pre-flight config check on the user's terminal before spawning the daemon.
///
/// The detached daemon's stdout/stderr are discarded, so its config-load errors
/// are invisible; this surfaces them on the user's terminal before spawn. The
/// load-error policy and the race-window rationale live in
/// `docs/src/dev-guide/config-and-persistence.md`.
///
/// # Errors
///
/// `Err(String)` only if `flow.toml` cannot be loaded (identifying file and
/// cause). A `flow-rules.toml` failure is warned on stderr and does *not* error.
/// On success returns the parsed app config so the caller can reuse it without
/// a second file read (e.g. for the start-time update-notification flag).
fn preflight_config_check(config_dir: &Path) -> Result<config::FlowConfig, String> {
    let app_path = config::dirs::user_app_config_path_in(config_dir);
    let config = match config::load_app_config(&app_path) {
        Ok(c) => c,
        // Fatal: surface why+where and refuse to spawn.
        Err(e) => return Err(format!("cannot start: {e}")),
    };

    let rules_path = config::dirs::user_rules_path_in(config_dir);
    if let Err(e) = config::load_rules_config(&rules_path) {
        // Non-fatal: warn on stderr but allow startup with default rules.
        eprintln!("flow: warning: {e}; using default window rules");
    }
    Ok(config)
}

/// How long `flow stop` waits for the daemon to finish its fail-back (status
/// file no longer `ready`) before cleaning up a leftover hotkey script.
const DAEMON_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Send a Stop message to the daemon, wait for it to be gone, then clean
/// up a leftover hotkey script.
///
/// `config` is resolved exactly like `flow start --config`, so the status
/// file and pidfile consulted are the running daemon's. The daemon kills its
/// own script during fail-back, so this waits (up to [`DAEMON_STOP_TIMEOUT`])
/// for its status to leave `ready` and then for the process to exit, and
/// only then runs [`autostart::stop_ahk_script_in`] for a pidfile the daemon
/// did not own or could not remove. See
/// `docs/src/dev-guide/ipc-and-watchdog.md`, *The hotkey script is tied to
/// the daemon*.
///
/// # Errors
///
/// Returns an error string if the daemon cannot be reached, or if
/// terminating a still-live recorded AHK process fails.
fn cmd_stop(config: Option<String>) -> Result<(), String> {
    let config_dir = config::dirs::resolve_config_dir(config.as_deref().map(Path::new));
    // The pid to wait on: ask the daemon itself, else trust its status file.
    let daemon_pid = match transport::send_message(&SocketMessage::Ping) {
        Ok(SocketResponse::Pong { pid, .. }) => Some(pid),
        _ => startup::read_status(&config_dir).map(|st| st.pid),
    };
    send_command(SocketMessage::Stop, "daemon stopped")?;
    let deadline = std::time::Instant::now() + DAEMON_STOP_TIMEOUT;
    if !wait_for_daemon_stop(&config_dir, DAEMON_STOP_TIMEOUT) {
        eprintln!("flow: warning: the daemon has not reported its stop yet; cleaning up anyway");
    }
    if let Some(pid) = daemon_pid {
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .max(DAEMON_EXIT_MIN_WAIT);
        if !autostart::wait_for_process_exit(pid, remaining) {
            eprintln!(
                "flow: warning: the daemon (pid {pid}) is still running; `flow start` may fail until it exits"
            );
        }
    }
    if autostart::stop_ahk_script_in(&config_dir)? {
        println!("flow: stopped flow.ahk");
    }
    Ok(())
}

/// Least time `flow stop` grants the daemon process to exit after its
/// status flipped, even when the status wait used up the whole budget.
const DAEMON_EXIT_MIN_WAIT: Duration = Duration::from_millis(500);

/// Poll the status file until it no longer says `ready`/`starting`, or the
/// pipe has gone away, or `timeout` elapses. Returns `true` when the daemon
/// is known to have finished.
fn wait_for_daemon_stop(config_dir: &Path, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match startup::read_status(config_dir) {
            // No status file at all: nothing to wait for.
            None => return true,
            Some(st) if daemon_finished(st.state) => return true,
            Some(_) => {}
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(DAEMON_POLL_INTERVAL);
    }
}

/// `true` for the status states a daemon writes once it is done.
fn daemon_finished(state: StartupState) -> bool {
    matches!(state, StartupState::Stopped | StartupState::Error)
}

/// `flow rescue [--force]`: daemon-less off-screen window rescue.
///
/// Prints how many windows were moved and their titles. Refuses to run
/// while a daemon answers on the pipe unless `force` is set, because the
/// daemon's layout would immediately disagree with the moved rects.
///
/// # Errors
///
/// Returns an error string if a daemon is running (without `--force`) or
/// if windows / monitors cannot be enumerated.
fn cmd_rescue(force: bool) -> Result<(), String> {
    let text = cmd_rescue_with(
        force,
        transport::is_daemon_running,
        rescue::rescue_offscreen_windows,
    )?;
    print!("{text}");
    Ok(())
}

/// [`cmd_rescue`] over injectable probes: `daemon_running` is consulted
/// only when `force` is false, and `run_rescue` only when the rescue is
/// allowed to go ahead. Returns the text to print.
fn cmd_rescue_with(
    force: bool,
    daemon_running: impl FnOnce() -> bool,
    run_rescue: impl FnOnce() -> Result<rescue::RescueReport, String>,
) -> Result<String, String> {
    if !force && daemon_running() {
        return Err(
            "a daemon is running; use `flow stop` (it rescues on exit) or `flow rescue --force`"
                .to_owned(),
        );
    }
    Ok(format_rescue_report(&run_rescue()?))
}

/// The lines `flow rescue` prints for a report.
fn format_rescue_report(report: &rescue::RescueReport) -> String {
    let mut out = String::new();
    if report.moved.is_empty() && report.minimized.is_empty() {
        out.push_str(&format!(
            "flow: no off-screen windows ({} window(s) checked)\n",
            report.scanned
        ));
        return out;
    }
    if !report.moved.is_empty() {
        out.push_str(&format!(
            "flow: moved {} off-screen window(s) back on-screen ({} checked):\n",
            report.moved.len(),
            report.scanned
        ));
        for w in &report.moved {
            out.push_str(&format!(
                "  {:?}  ({},{}) -> ({},{})\n",
                w.title, w.from.x, w.from.y, w.to.x, w.to.y
            ));
        }
    }
    if !report.minimized.is_empty() {
        out.push_str(&format!(
            "flow: re-anchored {} minimized window(s) whose restore position was off-screen ({} checked):\n",
            report.minimized.len(),
            report.scanned
        ));
        for w in &report.minimized {
            out.push_str(&format!(
                "  {:?}  ({},{}) -> ({},{})  (still minimized)\n",
                w.title, w.from.x, w.from.y, w.to.x, w.to.y
            ));
        }
    }
    out
}

/// Create the login autostart shortcut in `shell:startup`.
///
/// When `ahk` is true, the shortcut's args become `start --ahk` so login also
/// launches `flow.ahk`.
///
/// # Errors
///
/// Returns an error string on shortcut-creation failure (see
/// [`autostart::enable_autostart`]).
fn cmd_enable_autostart(ahk: bool) -> Result<(), String> {
    let report = autostart::enable_autostart(ahk)?;
    println!(
        "flow: autostart {} at {}",
        if ahk { "enabled (--ahk)" } else { "enabled" },
        report.shortcut.display()
    );
    Ok(())
}

/// Remove the login autostart shortcut from `shell:startup`.
///
/// Idempotent: silently succeeds if no shortcut exists.
///
/// # Errors
///
/// Returns an error string only on filesystem errors other than `NotFound`.
fn cmd_disable_autostart() -> Result<(), String> {
    let report = autostart::disable_autostart()?;
    println!("flow: autostart disabled ({})", report.shortcut.display());
    Ok(())
}

/// `flow update`: ask GitHub whether something newer than this build exists
/// and say how to update; never installs anything. Returns the process exit
/// code (the `EXIT_*` constants in [`updater`]): `0` up to date, `10` newer,
/// `2`-`5` for the failure kinds, `3` also when `flow.toml` cannot be read.
///
/// The `[update]` table comes from the config directory resolved like every
/// other local command (`FLOW_CONFIG_DIR`, else the default); the daemon is
/// not contacted.
fn cmd_update() -> i32 {
    let config_dir = config::dirs::resolve_config_dir(None);
    let app_path = config::dirs::user_app_config_path_in(&config_dir);
    let config = match config::load_app_config(&app_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("flow: update check failed: {e}");
            return updater::EXIT_NOT_FOUND;
        }
    };
    let target = match updater::target(&config.update) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("flow: update check failed: {e}");
            return e.exit_code();
        }
    };
    let local = updater::LocalBuild::this();
    let build = if local.commit_label.is_empty() {
        format!("your build: {}", local.version)
    } else {
        format!(
            "your build: {} ({}, {})",
            local.version,
            local.commit_label,
            if local.commit_date.is_empty() {
                "unknown date"
            } else {
                &local.commit_date
            }
        )
    };
    match &target {
        updater::Target::Branch { repo, branch } => {
            println!("flow: checking {repo} branch {branch}; {build}");
        }
        updater::Target::Release { repo } => {
            println!("flow: checking the latest release of {repo}; {build}");
        }
    }
    let report = match updater::check_target(&target, &local) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("flow: update check failed: {e}");
            return e.exit_code();
        }
    };
    println!("flow: {}", report.summary);
    for line in &report.details {
        println!("flow: {line}");
    }
    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.display().to_string()));
    for line in updater::how_to_update(&target, install_dir.as_deref()) {
        println!("flow: {line}");
    }
    report.verdict.exit_code()
}

/// Dispatch a loadout subcommand.
///
/// Routes [`LoadoutCommands::Save`] and [`LoadoutCommands::Load`] to the
/// daemon via IPC. An explicit path must name a `.json` file and may not
/// contain `..` (checked here for a prompt error, and again by the daemon);
/// a relative path is taken relative to the config directory, an absolute
/// one as given.
fn cmd_loadout(command: LoadoutCommands) -> Result<(), String> {
    let checked = |path: Option<String>| -> Result<Option<std::path::PathBuf>, String> {
        path.map(|p| {
            let p = std::path::PathBuf::from(p);
            flow_wm::loadout::validate_loadout_path(&p)?;
            Ok(p)
        })
        .transpose()
    };
    match command {
        LoadoutCommands::Save { path } => send_command(
            SocketMessage::LoadoutSave {
                path: checked(path)?,
            },
            "loadout saved",
        ),
        LoadoutCommands::Load { path } => send_command(
            SocketMessage::LoadoutLoad {
                path: checked(path)?,
            },
            "loadout loaded",
        ),
    }
}

/// Dispatch a configuration subcommand.
///
/// Most config commands operate locally (no daemon contact). Only `Reload`
/// sends an IPC message to the running daemon.
fn cmd_config(command: ConfigCommands) -> Result<(), String> {
    match command {
        ConfigCommands::Init { ahk } => cmd_config_init(ahk),
        ConfigCommands::Reload => cmd_config_reload(),
        ConfigCommands::Edit => cmd_config_edit(),
        ConfigCommands::Path => cmd_config_path(),
        ConfigCommands::Check => cmd_config_check(),
    }
}

/// Initialize the config directory with default files.
///
/// Calls [`config::dirs::config_dir`] to resolve the config directory, then
/// [`config::init_config_dir`] to create it and write default config files
/// (`flow.toml`, `flow-rules.toml`) and JSON Schemas. Existing files are never
/// overwritten.
///
/// When `ahk` is set, also writes `flow.ahk` — a bundled AutoHotkey v2
/// keybinding script — via [`config::write_ahk_template`]. This is a
/// convenience for users who want to drive FlowWM via AutoHotkey without
/// authoring their own script.
///
/// # Errors
///
/// Returns an error string if directory creation or file writing fails.
fn cmd_config_init(ahk: bool) -> Result<(), String> {
    let dir = config::dirs::config_dir();
    config::init_config_dir(&dir)?;
    println!("flow: config initialized at {}", dir.display());
    if ahk {
        let ahk_path = dir.join("flow.ahk");
        match config::write_ahk_template(&dir)? {
            true => println!("flow: wrote flow.ahk at {}", ahk_path.display()),
            false => println!(
                "flow: flow.ahk already exists at {} (left untouched)",
                ahk_path.display()
            ),
        }
    }
    Ok(())
}

/// Send a ReloadConfig message to the daemon.
///
/// This is the only `flow config` subcommand that requires the daemon to be
/// running. It tells the daemon to re-read all config files from disk.
fn cmd_config_reload() -> Result<(), String> {
    send_command(SocketMessage::ReloadConfig, "configuration reloaded")
}

/// Open the config directory in the system editor.
///
/// Resolves the editor command from:
/// 1. `EDITOR` environment variable
/// 2. `VISUAL` environment variable
/// 3. `notepad.exe` (Windows default)
///
/// Opens the **directory** (not a specific file) so the user can browse all
/// config files. Waits for the editor to exit before returning.
///
/// # Errors
///
/// Returns an error string if:
/// - No editor can be determined.
/// - The editor process fails to start.
/// - The editor exits with a non-zero status.
fn cmd_config_edit() -> Result<(), String> {
    let dir = config::dirs::config_dir();
    let editor = resolve_editor()?;

    println!("flow: opening {} in {}", dir.display(), editor);

    let status = Command::new(&editor)
        .arg(&dir)
        .status()
        .map_err(|e| format!("failed to start editor '{}': {e}", editor))?;

    if !status.success() {
        return Err(format!(
            "editor '{}' exited with status {}",
            editor,
            status
                .code()
                .map_or_else(|| "unknown".to_string(), |c| c.to_string())
        ));
    }

    Ok(())
}

/// Print the resolved config directory path.
///
/// Outputs just the path (one line) to stdout, making it useful for scripting
/// and shell integration.
fn cmd_config_path() -> Result<(), String> {
    let dir = config::dirs::config_dir();
    println!("{}", dir.display());
    Ok(())
}

/// Validate the configuration files without applying them.
///
/// Asks the running daemon (`CheckConfig`), which lists every problem —
/// parse errors, validation, each bad `[binds]` entry, preset lists that do
/// not fit the attached screens — and the rules file. Without a daemon the
/// files are checked locally ([`config::check_config`]), which cannot see
/// the screens. Missing files are not errors: defaults apply.
///
/// # Errors
///
/// One line per problem on stderr and an error naming their count.
fn cmd_config_check() -> Result<(), String> {
    match transport::send_message(&SocketMessage::CheckConfig) {
        Ok(SocketResponse::Data { payload }) => {
            let problems: Vec<String> = payload
                .get("problems")
                .and_then(|p| serde_json::from_value(p.clone()).ok())
                .unwrap_or_default();
            let screens = payload.get("screens").and_then(serde_json::Value::as_u64);
            if problems.is_empty() {
                println!(
                    "flow: configuration is valid (checked by the running daemon{})",
                    screens.map_or_else(String::new, |n| format!(" against {n} screen(s)"))
                );
                return Ok(());
            }
            for p in &problems {
                eprintln!("flow: {p}");
            }
            Err(format!("configuration has {} problem(s)", problems.len()))
        }
        Ok(SocketResponse::Error { message }) => Err(format!("daemon error: {message}")),
        Ok(other) => Err(format!("unexpected reply to check: {other:?}")),
        Err(_) => {
            let dir = config::dirs::config_dir();
            config::check_config(&dir)?;
            println!("flow: configuration is valid (no daemon running; checked locally)");
            Ok(())
        }
    }
}

/// `flow spawn`: ask the daemon to start `argv` on the focused screen and
/// print what it started.
fn cmd_spawn(argv: Vec<String>) -> Result<(), String> {
    let response = transport::send_message(&SocketMessage::Spawn { argv })
        .map_err(|e| format!("failed to send command: {e}"))?;
    match response {
        SocketResponse::Data { payload } => {
            println!("{}", spawn_reply_line(&payload));
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        other => Err(format!("unexpected reply to spawn: {other:?}")),
    }
}

/// The line `flow spawn` prints for the daemon's `Spawn` reply.
fn spawn_reply_line(payload: &serde_json::Value) -> String {
    let path = payload["path"].as_str().unwrap_or("?");
    let pid = payload["pid"].as_u64().unwrap_or(0);
    match (
        payload["x"].as_i64(),
        payload["y"].as_i64(),
        payload["monitor"].as_u64(),
    ) {
        (Some(x), Some(y), Some(m)) => {
            format!("started {path} (pid {pid}) at ({x},{y}) on monitor {m}")
        }
        _ => format!("started {path} (pid {pid}) at the default position"),
    }
}

/// Dispatch a query subcommand.
fn cmd_query(command: QueryCommands) -> Result<(), String> {
    match command {
        QueryCommands::All => cmd_query_all(),
        QueryCommands::Binds { plain } => cmd_query_binds(plain),
    }
}

/// Print the daemon's resolved key bindings: pretty JSON, or with `plain`
/// the tab-separated listing (`config::binds::plain_text`) the hotkey
/// script parses.
fn cmd_query_binds(plain: bool) -> Result<(), String> {
    let response = transport::send_message(&SocketMessage::QueryBinds)
        .map_err(|e| format!("failed to send command: {e}"))?;
    match response {
        SocketResponse::Data { payload } => {
            if plain {
                let report: config::BindsReport = serde_json::from_value(payload)
                    .map_err(|e| format!("daemon sent an unexpected binds payload: {e}"))?;
                print!("{}", config::binds::plain_text(&report));
            } else {
                let formatted =
                    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
                println!("{formatted}");
            }
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        other => Err(format!("unexpected reply to query binds: {other:?}")),
    }
}

/// Dump all tracked windows from the daemon as pretty-printed JSON.
fn cmd_query_all() -> Result<(), String> {
    let response = transport::send_message(&SocketMessage::QueryWindowsAll)
        .map_err(|e| format!("failed to send command: {e}"))?;

    match response {
        SocketResponse::Data { payload } => {
            let formatted =
                serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
            println!("{formatted}");
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Ok => {
            println!("flow: ok");
            Ok(())
        }
        SocketResponse::Busy => Err("daemon is busy (drag in progress), retry shortly".to_string()),
        SocketResponse::Pong { .. } => Err("unexpected pong reply to a query".to_string()),
    }
}

/// Dispatch a dispatch subcommand.
///
/// Routes each [`DispatchCommands`] variant to its handler. New action
/// categories will be added here as match arms.
fn cmd_dispatch(command: DispatchCommands) -> Result<(), String> {
    match command {
        DispatchCommands::Focus { direction } => cmd_dispatch_focus(direction),
        DispatchCommands::SwapColumn { direction } => cmd_dispatch_swap_column(direction),
        DispatchCommands::MoveWindow { direction } => cmd_dispatch_move_window(direction),
        DispatchCommands::MergeColumn { direction } => cmd_dispatch_merge_column(direction),
        DispatchCommands::Promote { direction } => cmd_dispatch_promote(direction),
        DispatchCommands::ConsumeOrExpel { direction } => send_command(
            SocketMessage::ConsumeOrExpel {
                direction: horizontal_to_direction(direction),
            },
            "window consumed or expelled",
        ),
        DispatchCommands::ExpandColumn => {
            send_command(SocketMessage::ExpandColumn, "column expanded")
        }
        DispatchCommands::ShrinkColumn => {
            send_command(SocketMessage::ShrinkColumn, "column shrunk")
        }
        DispatchCommands::Width { preset } => cmd_dispatch_width(&preset),
        DispatchCommands::SetLayout { mode } => cmd_dispatch_set_layout(&mode),
        DispatchCommands::Center => send_command(SocketMessage::Center, "viewport centered"),
        DispatchCommands::FocusFloating => cmd_dispatch_focus_layer(LayerTarget::Floating),
        DispatchCommands::FocusTiling => cmd_dispatch_focus_layer(LayerTarget::Tiling),
        DispatchCommands::SwitchFocusBetweenFloatingAndTiling => {
            cmd_dispatch_focus_layer(LayerTarget::Other)
        }
        DispatchCommands::CloseWindow => send_command(SocketMessage::CloseWindow, "window closed"),
        DispatchCommands::MinimizeWindow => {
            send_command(SocketMessage::MinimizeWindow, "window minimized")
        }
        DispatchCommands::SetWindow { mode } => cmd_dispatch_set_window(mode),
        DispatchCommands::SwitchWorkspace { workspace_id } => send_command(
            SocketMessage::SwitchWorkspace { workspace_id },
            "workspace switched",
        ),
        DispatchCommands::SwapWorkspace { workspace_id } => send_command(
            SocketMessage::SwapWorkspace { workspace_id },
            "workspace swapped",
        ),
        DispatchCommands::FocusMonitor { direction } => send_command(
            SocketMessage::FocusMonitor {
                direction: focus_direction_to_direction(direction),
            },
            "monitor focused",
        ),
        DispatchCommands::MoveWindowToMonitor { direction } => send_command(
            SocketMessage::MoveWindowToMonitor {
                direction: focus_direction_to_direction(direction),
            },
            "window moved to monitor",
        ),
        DispatchCommands::MoveWindowToWorkspace { workspace_id } => send_command(
            SocketMessage::MoveWindowToWorkspace { workspace_id },
            "window moved to workspace",
        ),
        DispatchCommands::Tiling { state } => cmd_dispatch_tiling(state),
        DispatchCommands::Drag { action } => cmd_dispatch_drag(action),
    }
}

/// Send `drag begin` / `drag end` and print what the daemon did.
///
/// `begin` answers `{"drag": kind}` (printed as `flow: drag begun (kind)`)
/// or an error, which is the exit code the hotkey script keys its move
/// loop on. `end` answers `{"ended": true, "drag": kind}` or
/// `{"ended": false}` (`flow: no drag session to end`, still exit 0).
fn cmd_dispatch_drag(action: DragAction) -> Result<(), String> {
    let msg = match action {
        DragAction::Begin => SocketMessage::DragBegin,
        DragAction::End => SocketMessage::DragEnd,
    };
    grant_foreground_permission();
    let response =
        transport::send_message(&msg).map_err(|e| format!("failed to send command: {e}"))?;
    match response {
        SocketResponse::Data { payload } => {
            println!("flow: {}", describe_drag_reply(action, &payload));
            Ok(())
        }
        SocketResponse::Ok => {
            println!(
                "flow: drag {}",
                if action == DragAction::Begin {
                    "begun"
                } else {
                    "ended"
                }
            );
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Busy => Err("daemon is busy (drag in progress), retry shortly".to_string()),
        SocketResponse::Pong { .. } => Err("unexpected pong reply to a command".to_string()),
    }
}

/// `flow dispatch focus-floating | focus-tiling |
/// switch-focus-between-floating-and-tiling`: the daemon answers `Data`
/// with a `message` either way (`focused the floating windows`, `no tiling
/// window to focus`); an empty side is exit 0, since the chord is a toggle.
fn cmd_dispatch_focus_layer(target: LayerTarget) -> Result<(), String> {
    grant_foreground_permission();
    let msg = SocketMessage::FocusLayer { target };
    let response =
        transport::send_message(&msg).map_err(|e| format!("failed to send command: {e}"))?;
    match response {
        SocketResponse::Data { payload } => {
            println!("flow: {}", describe_focus_layer_reply(&payload));
            Ok(())
        }
        SocketResponse::Ok => {
            println!("flow: focus changed");
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Busy => Err("daemon is busy (drag in progress), retry shortly".to_string()),
        SocketResponse::Pong { .. } => Err("unexpected pong reply to a command".to_string()),
    }
}

/// The line printed for a `FocusLayer` reply payload: its `message`, else a
/// generic line.
fn describe_focus_layer_reply(payload: &serde_json::Value) -> String {
    payload
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| "focus changed".to_owned(), str::to_owned)
}

/// The line printed for a `drag begin` / `drag end` reply payload.
fn describe_drag_reply(action: DragAction, payload: &serde_json::Value) -> String {
    let kind = payload
        .get("drag")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("window");
    match action {
        DragAction::Begin => format!("drag begun ({kind})"),
        DragAction::End => {
            if payload.get("ended").and_then(serde_json::Value::as_bool) == Some(false) {
                "no drag session to end".to_owned()
            } else {
                format!("drag ended ({kind})")
            }
        }
    }
}

/// Send a tiling on/off/toggle command and print the state it landed in.
///
/// Maps `flow dispatch tiling on|off` to [`SocketMessage::SetTiling`] and
/// `toggle` to [`SocketMessage::ToggleTiling`]. The daemon answers with
/// `{"tiling_enabled": bool}`, which is what gets printed (`flow: tiling
/// on` / `flow: tiling off`), so a toggle reports the resulting state.
fn cmd_dispatch_tiling(state: TilingSwitch) -> Result<(), String> {
    let msg = match state {
        TilingSwitch::On => SocketMessage::SetTiling { enabled: true },
        TilingSwitch::Off => SocketMessage::SetTiling { enabled: false },
        TilingSwitch::Toggle => SocketMessage::ToggleTiling,
    };
    grant_foreground_permission();
    let response =
        transport::send_message(&msg).map_err(|e| format!("failed to send command: {e}"))?;
    match response {
        SocketResponse::Data { payload } => {
            let enabled = payload
                .get("tiling_enabled")
                .and_then(serde_json::Value::as_bool)
                .ok_or_else(|| format!("malformed reply to tiling command: {payload}"))?;
            println!(
                "flow: tiling {}",
                startup::TilingStatus::from_enabled(enabled)
            );
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Busy => Err("daemon is busy (drag in progress), retry shortly".to_string()),
        other => Err(format!("unexpected reply to tiling command: {other:?}")),
    }
}

/// Send a column width preset command and print the preset it landed on.
///
/// Maps `flow dispatch width next|prev|cycle|<preset>` to
/// [`SocketMessage::SetWidthPreset`]. The daemon answers with
/// `{"width_preset": "<label>"}`, printed as `flow: column width <label>`.
fn cmd_dispatch_width(preset: &str) -> Result<(), String> {
    let msg = SocketMessage::SetWidthPreset {
        preset: WidthPresetRequest::from_cli(preset),
    };
    grant_foreground_permission();
    let response =
        transport::send_message(&msg).map_err(|e| format!("failed to send command: {e}"))?;
    match response {
        SocketResponse::Data { payload } => {
            let label = payload
                .get("width_preset")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("malformed reply to width command: {payload}"))?;
            println!("flow: column width {label}");
            Ok(())
        }
        SocketResponse::Ok => {
            println!("flow: column width set");
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Busy => Err("daemon is busy (drag in progress), retry shortly".to_string()),
        SocketResponse::Pong { .. } => Err("unexpected pong reply to a command".to_string()),
    }
}

/// Send a layout mode command and print the mode the screen landed on.
///
/// Maps `flow dispatch set-layout strip|book|toggle` to
/// [`SocketMessage::SetLayout`]; the daemon answers `{"layout": "<mode>",
/// "monitor": <index>}`, printed as `flow: layout <mode> on monitor <index>`.
fn cmd_dispatch_set_layout(mode: &str) -> Result<(), String> {
    let msg = SocketMessage::SetLayout {
        layout: LayoutRequest::from_cli(mode)?,
    };
    grant_foreground_permission();
    let response =
        transport::send_message(&msg).map_err(|e| format!("failed to send command: {e}"))?;
    match response {
        SocketResponse::Data { payload } => {
            let layout = payload
                .get("layout")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("malformed reply to set-layout: {payload}"))?;
            let monitor = payload
                .get("monitor")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            println!("flow: layout {layout} on monitor {monitor}");
            Ok(())
        }
        SocketResponse::Ok => Ok(()),
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Busy => Err("daemon is busy (drag in progress), retry shortly".to_string()),
        SocketResponse::Pong { .. } => Err("unexpected pong reply to a command".to_string()),
    }
}

/// Map the CLI's `HorizontalDirection` subcommand onto the IPC [`Direction`].
fn horizontal_to_direction(direction: HorizontalDirection) -> Direction {
    match direction {
        HorizontalDirection::Left => Direction::Left,
        HorizontalDirection::Right => Direction::Right,
    }
}

/// Map the CLI's `FocusDirection` subcommand onto the IPC [`Direction`].
fn focus_direction_to_direction(direction: FocusDirection) -> Direction {
    match direction {
        FocusDirection::Left => Direction::Left,
        FocusDirection::Right => Direction::Right,
        FocusDirection::Up => Direction::Up,
        FocusDirection::Down => Direction::Down,
    }
}

/// Send a focus-direction command to the daemon.
fn cmd_dispatch_focus(direction: FocusDirection) -> Result<(), String> {
    let msg = match direction {
        FocusDirection::Left => SocketMessage::FocusLeft,
        FocusDirection::Right => SocketMessage::FocusRight,
        FocusDirection::Up => SocketMessage::FocusUp,
        FocusDirection::Down => SocketMessage::FocusDown,
    };
    send_command(msg, "focus changed")
}

/// Send a column-swap command to the daemon.
///
/// Maps `flow dispatch swap-column left|right` to [`SocketMessage::SwapColumn`].
fn cmd_dispatch_swap_column(direction: HorizontalDirection) -> Result<(), String> {
    let msg = match direction {
        HorizontalDirection::Left => SocketMessage::SwapColumn {
            direction: Direction::Left,
        },
        HorizontalDirection::Right => SocketMessage::SwapColumn {
            direction: Direction::Right,
        },
    };
    send_command(msg, "column swapped")
}

/// Send a semantic move-window command to the daemon.
///
/// Maps `flow dispatch move-window left|right|up|down` to [`SocketMessage::MoveWindow`].
/// The daemon translates this into a concrete action based on window state
/// and direction (column swap horizontally, row swap vertically).
fn cmd_dispatch_move_window(direction: MoveDirection) -> Result<(), String> {
    let msg = match direction {
        MoveDirection::Left => SocketMessage::MoveWindow {
            direction: Direction::Left,
        },
        MoveDirection::Right => SocketMessage::MoveWindow {
            direction: Direction::Right,
        },
        MoveDirection::Up => SocketMessage::MoveWindow {
            direction: Direction::Up,
        },
        MoveDirection::Down => SocketMessage::MoveWindow {
            direction: Direction::Down,
        },
    };
    send_command(msg, "window moved")
}

/// Send a merge-column command to the daemon.
///
/// Maps `flow dispatch merge-column left|right` to [`SocketMessage::MergeColumn`].
/// The focused window is merged into the adjacent column as a new bottom row.
fn cmd_dispatch_merge_column(direction: HorizontalDirection) -> Result<(), String> {
    let msg = match direction {
        HorizontalDirection::Left => SocketMessage::MergeColumn {
            direction: Direction::Left,
        },
        HorizontalDirection::Right => SocketMessage::MergeColumn {
            direction: Direction::Right,
        },
    };
    send_command(msg, "column merged")
}

/// Send a promote command to the daemon.
///
/// Maps `flow dispatch promote left|right` to [`SocketMessage::Promote`]. The
/// focused window is extracted into a new single-row column on the chosen side.
fn cmd_dispatch_promote(direction: HorizontalDirection) -> Result<(), String> {
    let msg = match direction {
        HorizontalDirection::Left => SocketMessage::Promote {
            direction: Direction::Left,
        },
        HorizontalDirection::Right => SocketMessage::Promote {
            direction: Direction::Right,
        },
    };
    send_command(msg, "window promoted")
}

/// Send a set-window-mode command to the daemon.
///
/// Maps `flow dispatch set-window float|tile|cycle` to [`SocketMessage::SetWindow`].
fn cmd_dispatch_set_window(mode: WindowMode) -> Result<(), String> {
    let msg = SocketMessage::SetWindow { mode };
    let label = match mode {
        WindowMode::Float => "float",
        WindowMode::Tile => "tile",
        WindowMode::Cycle => "cycle",
        WindowMode::Fullscreen => "fullscreen",
    };
    send_command(msg, &format!("window set to {label}"))
}

/// Grant the daemon one-shot foreground-activation permission via
/// `AllowSetForegroundWindow(ASFW_ANY)`, so its next `SetForegroundWindow`
/// isn't refused by the foreground lock. Must be re-granted per command (the
/// grant is consumed by the next foreground change or auto-expires after a few
/// seconds; there is no unset API).
///
/// Failures are intentionally ignored (`let _ =`): a failed grant still leaves
/// the daemon's `AttachThreadInput` fallback (in
/// `registry::win32::set_foreground_window`) available, and erroring here
/// would break the user's hotkey flow for a non-fatal permission.
///
/// See (`docs/src/dev-guide/ipc-and-watchdog.md`) for the foreground-lock
/// diagnosis and why this is the documented PowerToys pattern.
fn grant_foreground_permission() {
    // ASFW_ANY == (DWORD)-1 == 0xFFFFFFFF — any thread may take the foreground.
    let _ = unsafe { AllowSetForegroundWindow(u32::MAX) };
}

/// Send a command to the daemon and print a success message on Ok.
///
/// A shared helper used by `cmd_stop`, `cmd_config_reload`, and every
/// dispatch command. Before sending, it grants the daemon one-shot foreground
/// activation permission (see [`grant_foreground_permission`]) so the
/// daemon's `SetForegroundWindow` calls aren't blocked by the Windows
/// foreground lock. Handles the three response variants:
/// Sends a command and maps the response to a [`Result`].
///
/// Handles [`SocketResponse::Ok`], [`SocketResponse::Error`],
/// [`SocketResponse::Data`], and [`SocketResponse::Busy`].
fn send_command(msg: SocketMessage, success_msg: &str) -> Result<(), String> {
    grant_foreground_permission();

    let response =
        transport::send_message(&msg).map_err(|e| format!("failed to send command: {e}"))?;

    match response {
        SocketResponse::Ok => {
            println!("flow: {success_msg}");
            Ok(())
        }
        SocketResponse::Error { message } => Err(format!("daemon error: {message}")),
        SocketResponse::Data { .. } => {
            println!("flow: {success_msg}");
            Ok(())
        }
        SocketResponse::Busy => Err("daemon is busy (drag in progress), retry shortly".to_string()),
        SocketResponse::Pong { .. } => Err("unexpected pong reply to a command".to_string()),
    }
}

/// Resolve the editor command for opening config files.
///
/// Checks environment variables in order:
/// 1. `EDITOR` — the user's preferred editor.
/// 2. `VISUAL` — an alternative editor variable (common in Unix-like shells).
/// 3. `notepad.exe` — the Windows default.
///
/// # Errors
///
/// Returns an error string only if all three resolution paths fail (which
/// should never happen since `notepad.exe` is always available on Windows).
fn resolve_editor() -> Result<String, String> {
    if let Ok(editor) = std::env::var("EDITOR")
        && !editor.is_empty()
    {
        return Ok(editor);
    }

    if let Ok(editor) = std::env::var("VISUAL")
        && !editor.is_empty()
    {
        return Ok(editor);
    }

    // Windows default — always available.
    Ok("notepad.exe".to_string())
}

/// Spawn the daemon executable as a background process.
///
/// Uses `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` on native Windows so
/// the daemon runs fully detached from the terminal. No explicit `Stdio` is
/// set so that Rust's `Command::spawn()` calls `CreateProcessW` with
/// `bInheritHandles = FALSE` — this prevents the daemon from inheriting the
/// parent's kernel handles (e.g., stdout/stderr pipes created by test
/// harnesses like `assert_cmd`), which would otherwise keep those pipes open
/// after the parent exits.
///
/// Falls back to a plain `spawn()` when the detached spawn fails (e.g., under
/// WSL interop).
///
/// The [`Child`] is returned (not dropped) so [`wait_for_daemon`] can notice
/// an early exit via `try_wait`; dropping it later does not kill the daemon.
fn spawn_daemon(opts: &StartOptions) -> Result<Child, String> {
    let exe = find_daemon_exe()?;

    // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
    const DETACHED: u32 = 0x00000200 | 0x08000000;

    // Try detached spawn first (native Windows), fall back to plain spawn (WSL).
    // Both paths forward the same daemon flags.
    daemon_command(&exe, opts)
        .creation_flags(DETACHED)
        .spawn()
        .or_else(|_| daemon_command(&exe, opts).spawn())
        .map_err(|e| format!("failed to spawn daemon ({}): {e}", exe.display()))
}

/// Build the daemon [`Command`] with the forwarded flags applied
/// (`--log-file`, `--no-restore`, `--verbose`, `--log-level`, `--ahk`).
///
/// Factored out so [`spawn_daemon`] can construct an identical command for
/// both the native detached spawn and the WSL fallback spawn — the forwarded
/// arguments must be present on both paths for the overrides to take effect
/// regardless of which spawn path succeeds.
///
/// The config directory is NOT passed here; it is propagated to the daemon
/// via the inherited `FLOW_CONFIG_DIR` environment variable (see [`cmd_start`]).
/// The environment is inherited unchanged, so `FLOW_FAIL_AT` (test-only fault
/// injection) and `RUST_LOG` reach the daemon too.
fn daemon_command(exe: &Path, opts: &StartOptions) -> Command {
    let mut cmd = Command::new(exe);
    cmd.args(daemon_args(opts));
    cmd
}

/// The daemon CLI arguments derived from [`StartOptions`], in a stable order.
fn daemon_args(opts: &StartOptions) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(path) = &opts.log_file_override {
        args.push("--log-file".to_owned());
        args.push(path.clone());
    }
    if opts.no_restore {
        args.push("--no-restore".to_owned());
    }
    if opts.verbose {
        args.push("--verbose".to_owned());
    }
    if let Some(level) = &opts.log_level {
        args.push("--log-level".to_owned());
        args.push(level.clone());
    }
    if opts.ahk {
        args.push("--ahk".to_owned());
    }
    args
}

/// Locate the `flowd.exe` binary next to the current executable.
fn find_daemon_exe() -> Result<std::path::PathBuf, String> {
    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot determine current executable: {e}"))?;

    let dir = current_exe
        .parent()
        .ok_or_else(|| "cannot determine executable directory".to_string())?;

    let daemon = dir.join("flowd.exe");
    if daemon.exists() {
        return Ok(daemon);
    }

    Err(format!("daemon binary not found at {}", daemon.display()))
}

/// Wait for the spawned daemon to finish initialization, or explain why it
/// did not.
///
/// Every [`DAEMON_POLL_INTERVAL`] this checks `child.try_wait()`, then the
/// daemon's status file (only with `pid == child.id()`), then a `Ping` whose
/// `Pong.pid` matches the child; the daemon counts as started only after
/// the ping. A reported `state: error` fails at once with the stage and the
/// daemon's exit code. Times out after [`DAEMON_START_TIMEOUT`], naming the
/// last stage seen. See `docs/src/dev-guide/ipc-and-watchdog.md`, *The
/// `flow` CLI Client*.
fn wait_for_daemon(
    child: &mut Child,
    config_dir: &Path,
    expected_log_path: &Path,
) -> Result<StartSummary, StartError> {
    let child_pid = child.id();
    let deadline = std::time::Instant::now() + DAEMON_START_TIMEOUT;
    let mut last_stage: Option<String> = None;

    loop {
        // 1. Did the daemon exit?
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("failed to poll daemon process: {e}"))?
        {
            let exit_code = status.code().unwrap_or(1);
            return Err(match own_status(config_dir, child_pid) {
                Some(st) if st.state == StartupState::Error => StartError::at_stage(
                    st.stage.name(),
                    &st.reason,
                    log_path_of(&st, expected_log_path),
                    if exit_code > 1 {
                        exit_code
                    } else {
                        st.stage.exit_code()
                    },
                ),
                Some(st) => StartError::at_stage(
                    st.stage.name(),
                    &format!("daemon exited with {status} after this step"),
                    log_path_of(&st, expected_log_path),
                    exit_code.max(1),
                ),
                None => StartError::at_stage(
                    "unknown",
                    &format!("daemon exited with {status} before reporting any step"),
                    expected_log_path,
                    exit_code.max(1),
                ),
            });
        }

        // 2. What does the status file say (ours only)?
        match own_status(config_dir, child_pid) {
            Some(st) => {
                last_stage = Some(st.stage.name().to_owned());
                match st.state {
                    StartupState::Error => {
                        return Err(StartError::at_stage(
                            st.stage.name(),
                            &st.reason,
                            log_path_of(&st, expected_log_path),
                            st.stage.exit_code(),
                        ));
                    }
                    StartupState::Ready => {
                        // 3. Must actually answer on the pipe.
                        if let Some(summary) = ping_own_daemon(child_pid) {
                            return Ok(summary);
                        }
                    }
                    StartupState::Starting => {}
                    // Our own pid says it already stopped: the next `try_wait`
                    // poll reports the exit with its code.
                    StartupState::Stopped => {}
                }
            }
            // No status file of ours (unwritable config dir, or the write
            // lost its retries): the daemon answering with our pid is ready
            // all the same.
            None => {
                if let Some(summary) = ping_own_daemon(child_pid) {
                    return Ok(summary);
                }
            }
        }

        if std::time::Instant::now() >= deadline {
            let stage = last_stage.unwrap_or_else(|| "unknown".to_owned());
            return Err(StartError::at_stage(
                &stage,
                &format!(
                    "timed out after {}s waiting for the daemon to become ready",
                    DAEMON_START_TIMEOUT.as_secs()
                ),
                expected_log_path,
                1,
            ));
        }
        std::thread::sleep(DAEMON_POLL_INTERVAL);
    }
}

/// One `Ping` round-trip; `Some` only if a daemon answered **and** its
/// `Pong.pid` is `child_pid` (an older daemon on the pipe does not count).
fn ping_own_daemon(child_pid: u32) -> Option<StartSummary> {
    match transport::send_message(&SocketMessage::Ping) {
        Ok(SocketResponse::Pong {
            pid,
            monitors,
            hooks_registered,
            ahk_pid,
            ..
        }) if pid == child_pid => Some(StartSummary {
            pid,
            monitors,
            hooks_registered,
            ahk_pid,
        }),
        _ => None,
    }
}

/// Read the status file, keeping it only if it was written by `pid`.
fn own_status(config_dir: &Path, pid: u32) -> Option<StartupStatus> {
    startup::read_status(config_dir).filter(|st| st.pid == pid)
}

/// The log path to print for a status: the daemon's own if it recorded one,
/// else the path the client expected it to use.
fn log_path_of<'a>(status: &'a StartupStatus, fallback: &'a Path) -> &'a Path {
    status.log_path.as_deref().unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    use clap::Parser;

    // --- Win32 foreground-permission helper ---

    #[test]
    fn grant_foreground_permission_does_not_panic() {
        // Smoke test: the helper wraps an unsafe Win32 call that returns a
        // result code and never panics. End-to-end effectiveness (the daemon's
        // `SetForegroundWindow` now succeeds) is validated manually by
        // dispatching focus with the daemon running. This test guards against
        // future changes that alter the call path or the ASFW_ANY constant in a
        // way that breaks compilation of this path.
        grant_foreground_permission();
    }

    // --- Positive: each subcommand parses correctly ---

    #[test]
    fn parse_start() {
        let cli = Cli::try_parse_from(["flow", "start"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Start {
                config: None,
                log_file: None,
                ahk: false,
                no_restore: false,
                verbose: false,
                log_level: None,
            }
        ));
    }

    #[test]
    fn parse_start_verbose_and_log_level() {
        // Positive: both spellings of the verbose flag, and an explicit level.
        for flag in ["--verbose", "-v"] {
            let cli = Cli::try_parse_from(["flow", "start", flag]).unwrap();
            assert!(
                matches!(cli.command, Commands::Start { verbose: true, .. }),
                "{flag} should set verbose"
            );
        }
        let cli = Cli::try_parse_from(["flow", "start", "--log-level", "trace"]).unwrap();
        match cli.command {
            Commands::Start {
                log_level: Some(ref l),
                verbose: false,
                ..
            } => assert_eq!(l, "trace"),
            other => panic!("expected Start with --log-level, got: {other:?}"),
        }
        // Negative: --log-level needs a value.
        assert!(Cli::try_parse_from(["flow", "start", "--log-level"]).is_err());
    }

    #[test]
    fn daemon_args_forward_every_start_flag() {
        // Positive: each forwarded flag appears exactly as flowd expects it.
        let opts = StartOptions {
            config_override: Some("C:\\custom\\flow".to_owned()),
            log_file_override: Some("C:\\tmp\\debug.log".to_owned()),
            ahk: true,
            no_restore: true,
            verbose: true,
            log_level: Some("trace".to_owned()),
        };
        assert_eq!(
            daemon_args(&opts),
            [
                "--log-file",
                "C:\\tmp\\debug.log",
                "--no-restore",
                "--verbose",
                "--log-level",
                "trace",
                "--ahk",
            ]
            .map(String::from)
        );
        // --config is NOT a daemon flag (it travels as an env var).
        assert_eq!(daemon_args(&StartOptions::default()), Vec::<String>::new());
    }

    #[test]
    fn rescue_report_formatting() {
        use flow_wm::common::Rect;
        let empty = rescue::RescueReport {
            scanned: 3,
            moved: vec![],
            minimized: vec![],
        };
        assert_eq!(
            format_rescue_report(&empty),
            "flow: no off-screen windows (3 window(s) checked)\n"
        );
        let r = |x, y| Rect {
            x,
            y,
            width: 800,
            height: 600,
        };
        let moved = rescue::RescueReport {
            scanned: 4,
            moved: vec![rescue::RescuedWindow {
                title: "Notepad".to_owned(),
                from: r(-960, 0),
                to: r(0, 0),
            }],
            minimized: vec![],
        };
        assert_eq!(
            format_rescue_report(&moved),
            "flow: moved 1 off-screen window(s) back on-screen (4 checked):\n  \"Notepad\"  (-960,0) -> (0,0)\n"
        );
        let minimized = rescue::RescueReport {
            scanned: 5,
            moved: vec![],
            minimized: vec![rescue::RescuedWindow {
                title: "Terminal".to_owned(),
                from: r(1936, 0),
                to: r(1120, 0),
            }],
        };
        assert_eq!(
            format_rescue_report(&minimized),
            "flow: re-anchored 1 minimized window(s) whose restore position was off-screen (5 checked):\n  \"Terminal\"  (1936,0) -> (1120,0)  (still minimized)\n"
        );
    }

    #[test]
    fn rescue_refuses_while_a_daemon_is_running_unless_forced() {
        // The refusal must come before any window is touched.
        let err = cmd_rescue_with(false, || true, || panic!("rescue must not run"))
            .expect_err("a running daemon blocks the rescue");
        assert!(err.contains("a daemon is running"), "{err}");
        assert!(err.contains("--force"), "{err}");

        let report = || {
            Ok(rescue::RescueReport {
                scanned: 2,
                moved: vec![],
                minimized: vec![],
            })
        };
        // --force skips the probe entirely.
        let out = cmd_rescue_with(true, || panic!("probe must not run"), report).expect("forced");
        assert_eq!(out, "flow: no off-screen windows (2 window(s) checked)\n");
        // No daemon: the rescue runs.
        let out = cmd_rescue_with(false, || false, report).expect("no daemon");
        assert_eq!(out, "flow: no off-screen windows (2 window(s) checked)\n");
        // A rescue failure is passed through.
        let err = cmd_rescue_with(false, || false, || Err("EnumWindows failed".to_owned()))
            .expect_err("rescue error propagates");
        assert_eq!(err, "EnumWindows failed");
    }

    #[test]
    fn stop_waits_for_the_status_file_to_leave_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No file: nothing to wait for.
        assert!(wait_for_daemon_stop(dir.path(), Duration::from_millis(50)));
        let mut st = StartupStatus {
            pid: 1,
            stage: flow_wm::daemon::StartupStage::Ready,
            state: StartupState::Ready,
            reason: String::new(),
            log_path: None,
            tiling: startup::TilingStatus::On,
        };
        startup::write_status(dir.path(), &st).unwrap();
        let t0 = std::time::Instant::now();
        assert!(!wait_for_daemon_stop(
            dir.path(),
            Duration::from_millis(150)
        ));
        assert!(t0.elapsed() >= Duration::from_millis(150));
        st.state = StartupState::Stopped;
        st.stage = flow_wm::daemon::StartupStage::Runtime;
        startup::write_status(dir.path(), &st).unwrap();
        assert!(wait_for_daemon_stop(dir.path(), Duration::from_millis(50)));
        assert!(daemon_finished(StartupState::Error));
        assert!(!daemon_finished(StartupState::Starting));
    }

    #[test]
    fn parse_spawn_keeps_each_argument() {
        // Positive: the program and every argument arrive as separate items,
        // hyphenated ones included, with or without `--`.
        let cli = Cli::try_parse_from([
            "flow",
            "spawn",
            "--",
            r"C:\Program Files\Alacritty\alacritty.exe",
            "-e",
            "a b",
        ])
        .unwrap();
        match cli.command {
            Commands::Spawn { argv } => assert_eq!(
                argv,
                [r"C:\Program Files\Alacritty\alacritty.exe", "-e", "a b"]
            ),
            other => panic!("{other:?}"),
        }
        let cli = Cli::try_parse_from(["flow", "spawn", "wt", "-p", "x"]).unwrap();
        assert!(matches!(cli.command, Commands::Spawn { argv } if argv == ["wt", "-p", "x"]));
        // Negative: a program is required.
        assert!(Cli::try_parse_from(["flow", "spawn"]).is_err());
    }

    #[test]
    fn spawn_reply_line_names_the_position_or_the_default() {
        let at =
            serde_json::json!({"pid": 42, "path": r"C:\a.exe", "x": -1904, "y": 16, "monitor": 1});
        assert_eq!(
            spawn_reply_line(&at),
            r"started C:\a.exe (pid 42) at (-1904,16) on monitor 1"
        );
        let default = serde_json::json!({"pid": 7, "path": r"C:\b.cmd", "x": null, "y": null, "monitor": null});
        assert_eq!(
            spawn_reply_line(&default),
            r"started C:\b.cmd (pid 7) at the default position"
        );
    }

    #[test]
    fn parse_rescue() {
        let cli = Cli::try_parse_from(["flow", "rescue"]).unwrap();
        assert!(matches!(cli.command, Commands::Rescue { force: false }));
        let cli = Cli::try_parse_from(["flow", "rescue", "--force"]).unwrap();
        assert!(matches!(cli.command, Commands::Rescue { force: true }));
        // Negative: no positionals.
        assert!(Cli::try_parse_from(["flow", "rescue", "now"]).is_err());
    }

    #[test]
    fn start_error_message_format_is_exact() {
        // Pin the failure line `flow start` prints (after the `flow: ` prefix).
        let err = StartError::at_stage(
            "hooks",
            "mandatory WinEvent hook(s) failed to register: FOREGROUND",
            Path::new("C:\\logs\\flowd-2026-10-19.log"),
            15,
        );
        assert_eq!(
            err.message,
            "FlowWM failed to start at step 'hooks': mandatory WinEvent hook(s) failed to register: FOREGROUND (log: C:\\logs\\flowd-2026-10-19.log)"
        );
        assert_eq!(err.exit_code, 15);
        // Plain string errors keep the generic exit code.
        let generic: StartError = "daemon is already running".to_owned().into();
        assert_eq!(generic.exit_code, 1);
    }

    #[test]
    fn own_status_ignores_other_pids() {
        // A stale status file from a previous daemon (different pid) must not
        // be mistaken for the daemon we just spawned.
        let dir = tempfile::tempdir().expect("tempdir");
        let stale = StartupStatus {
            pid: 1,
            stage: flow_wm::daemon::StartupStage::Ready,
            state: StartupState::Ready,
            reason: String::new(),
            log_path: None,
            tiling: startup::TilingStatus::On,
        };
        startup::write_status(dir.path(), &stale).unwrap();
        assert!(own_status(dir.path(), 2).is_none());
        assert_eq!(own_status(dir.path(), 1), Some(stale));
        assert!(own_status(Path::new("C:\\does\\not\\exist"), 1).is_none());
    }

    #[test]
    fn parse_ping() {
        let cli = Cli::try_parse_from(["flow", "ping"]).unwrap();
        assert!(matches!(cli.command, Commands::Ping));
        // Negative: ping takes no arguments.
        assert!(Cli::try_parse_from(["flow", "ping", "now"]).is_err());
    }

    #[test]
    fn parse_start_with_config_flag() {
        let cli = Cli::try_parse_from(["flow", "start", "--config", "C:\\custom\\flow"]).unwrap();
        match cli.command {
            Commands::Start {
                config: Some(ref c),
                ..
            } => {
                assert_eq!(c, "C:\\custom\\flow");
            }
            other => panic!("expected Start with --config, got: {other:?}"),
        }
    }

    #[test]
    fn parse_start_without_config_flag() {
        let cli = Cli::try_parse_from(["flow", "start"]).unwrap();
        match cli.command {
            Commands::Start { config: None, .. } => {}
            other => panic!("expected Start with no --config, got: {other:?}"),
        }
    }

    #[test]
    fn parse_start_with_log_file_flag() {
        let cli =
            Cli::try_parse_from(["flow", "start", "--log-file", "C:\\tmp\\debug.log"]).unwrap();
        match cli.command {
            Commands::Start {
                log_file: Some(ref p),
                ..
            } => {
                assert_eq!(p, "C:\\tmp\\debug.log");
            }
            other => panic!("expected Start with --log-file, got: {other:?}"),
        }
    }

    #[test]
    fn parse_start_with_config_and_log_file_flags() {
        let cli = Cli::try_parse_from([
            "flow",
            "start",
            "--config",
            "C:\\custom\\flow",
            "--log-file",
            "C:\\tmp\\debug.log",
        ])
        .unwrap();
        match cli.command {
            Commands::Start {
                config: Some(ref c),
                log_file: Some(ref p),
                ahk: false,
                ..
            } => {
                assert_eq!(c, "C:\\custom\\flow");
                assert_eq!(p, "C:\\tmp\\debug.log");
            }
            other => panic!("expected Start with both flags, got: {other:?}"),
        }
    }

    #[test]
    fn parse_start_with_ahk_config_and_log_file_combined() {
        // Composition lock: all three Start flags must coexist on one command
        // line without ordering sensitivity. Each flag is independently tested
        // above; this pins that the parser does not reject the combination.
        let cli = Cli::try_parse_from([
            "flow",
            "start",
            "--ahk",
            "--config",
            "C:\\custom\\flow",
            "--log-file",
            "C:\\tmp\\debug.log",
        ])
        .unwrap();
        match cli.command {
            Commands::Start {
                ahk: true,
                config: Some(ref c),
                log_file: Some(ref p),
                ..
            } => {
                assert_eq!(c, "C:\\custom\\flow");
                assert_eq!(p, "C:\\tmp\\debug.log");
            }
            other => panic!("expected Start with all three flags, got: {other:?}"),
        }
    }

    #[test]
    fn parse_stop() {
        // `flow stop` needs no flag: the hotkey script is always stopped.
        let cli = Cli::try_parse_from(["flow", "stop"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Stop {
                ahk: false,
                config: None
            }
        ));
        let cli = Cli::try_parse_from(["flow", "stop", "--config", "C:\\cfg"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Stop {
                ahk: false,
                config: Some(ref c)
            } if c == "C:\\cfg"
        ));
        // Negative: no positionals.
        assert!(Cli::try_parse_from(["flow", "stop", "now"]).is_err());
    }

    #[test]
    fn parse_start_with_ahk_flag() {
        let cli = Cli::try_parse_from(["flow", "start", "--ahk"]).unwrap();
        match cli.command {
            Commands::Start {
                ahk: true,
                config: None,
                log_file: None,
                ..
            } => {}
            other => panic!("expected Start {{ ahk: true, .. }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_stop_with_ahk_flag() {
        // Still accepted (hidden) for scripts written against the old CLI.
        let cli = Cli::try_parse_from(["flow", "stop", "--ahk"]).unwrap();
        assert!(matches!(cli.command, Commands::Stop { ahk: true, .. }));
    }

    #[test]
    fn parse_start_positional_arg_fails() {
        // Negative: `start` takes no positional args (only --config/--log-file/--ahk).
        let result = Cli::try_parse_from(["flow", "start", "unexpected"]);
        assert!(
            result.is_err(),
            "'flow start' with a positional arg should fail"
        );
    }

    #[test]
    fn parse_start_no_restore_flag() {
        // Positive: `flow start --no-restore` parses with no_restore = true.
        let cli = Cli::try_parse_from(["flow", "start", "--no-restore"]).unwrap();
        match cli.command {
            Commands::Start {
                no_restore: true, ..
            } => {}
            other => panic!("expected Start {{ no_restore: true, .. }}, got: {other:?}"),
        }
    }

    // --- Loadout command parsing ---

    #[test]
    fn parse_loadout_save() {
        // Positive: `flow loadout save` with no path.
        let cli = Cli::try_parse_from(["flow", "loadout", "save"]).unwrap();
        match cli.command {
            Commands::Loadout {
                command: LoadoutCommands::Save { path: None },
            } => {}
            other => panic!("expected Loadout::Save {{ path: None }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_loadout_save_with_path() {
        // Positive: `flow loadout save C:\x.json` with explicit path.
        let cli = Cli::try_parse_from(["flow", "loadout", "save", "C:\\x.json"]).unwrap();
        match cli.command {
            Commands::Loadout {
                command: LoadoutCommands::Save { path: Some(ref p) },
            } => {
                assert_eq!(p, "C:\\x.json");
            }
            other => panic!("expected Loadout::Save {{ path: Some(..) }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_loadout_load() {
        // Positive: `flow loadout load` with no path.
        let cli = Cli::try_parse_from(["flow", "loadout", "load"]).unwrap();
        match cli.command {
            Commands::Loadout {
                command: LoadoutCommands::Load { path: None },
            } => {}
            other => panic!("expected Loadout::Load {{ path: None }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_loadout_load_with_path() {
        // Positive: `flow loadout load C:\x.json` with explicit path.
        let cli = Cli::try_parse_from(["flow", "loadout", "load", "C:\\x.json"]).unwrap();
        match cli.command {
            Commands::Loadout {
                command: LoadoutCommands::Load { path: Some(ref p) },
            } => {
                assert_eq!(p, "C:\\x.json");
            }
            other => panic!("expected Loadout::Load {{ path: Some(..) }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_loadout_unknown_subcommand_fails() {
        // Negative: `flow loadout bogus` should fail.
        let result = Cli::try_parse_from(["flow", "loadout", "bogus"]);
        assert!(
            result.is_err(),
            "'flow loadout bogus' with unknown subcommand should fail"
        );
    }

    #[test]
    fn parse_enable_autostart() {
        let cli = Cli::try_parse_from(["flow", "enable-autostart"]).unwrap();
        match cli.command {
            Commands::EnableAutostart { ahk: false } => {}
            other => panic!("expected EnableAutostart {{ ahk: false }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_enable_autostart_ahk() {
        let cli = Cli::try_parse_from(["flow", "enable-autostart", "--ahk"]).unwrap();
        match cli.command {
            Commands::EnableAutostart { ahk: true } => {}
            other => panic!("expected EnableAutostart {{ ahk: true }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_enable_autostart_extra_arg_fails() {
        // Negative: enable-autostart takes only --ahk, no positionals.
        let result = Cli::try_parse_from(["flow", "enable-autostart", "bogus"]);
        assert!(
            result.is_err(),
            "'flow enable-autostart' with a positional arg should fail"
        );
    }

    #[test]
    fn parse_enable_autostart_unknown_flag_fails() {
        // Negative: only --ahk is accepted.
        let result = Cli::try_parse_from(["flow", "enable-autostart", "--bogus"]);
        assert!(result.is_err(), "unknown flag should fail");
    }

    #[test]
    fn parse_disable_autostart() {
        let cli = Cli::try_parse_from(["flow", "disable-autostart"]).unwrap();
        assert!(matches!(cli.command, Commands::DisableAutostart));
    }

    #[test]
    fn parse_disable_autostart_ahk_flag_fails() {
        // Negative: disable-autostart takes no flags (single shortcut — see
        // option (a) design). This pins that --ahk is NOT accepted here.
        let result = Cli::try_parse_from(["flow", "disable-autostart", "--ahk"]);
        assert!(
            result.is_err(),
            "'flow disable-autostart --ahk' should fail (no --ahk flag)"
        );
    }

    #[test]
    fn parse_disable_autostart_extra_arg_fails() {
        // Negative: disable-autostart takes no arguments at all.
        let result = Cli::try_parse_from(["flow", "disable-autostart", "bogus"]);
        assert!(
            result.is_err(),
            "'flow disable-autostart' with a positional arg should fail"
        );
    }

    // --- update parsing ---
    //
    // `flow update` has one optional flag (`--check`, a compatibility no-op:
    // the command only ever checks). The cases below
    // pin both flag states and that no positional or unknown flag is accepted.

    #[test]
    fn parse_update_default() {
        // Positive: `flow update` parses with check = false.
        let cli = Cli::try_parse_from(["flow", "update"]).unwrap();
        match cli.command {
            Commands::Update { check: false } => {}
            other => panic!("expected Update {{ check: false }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_update_check_flag() {
        // Positive: `flow update --check` parses with check = true.
        let cli = Cli::try_parse_from(["flow", "update", "--check"]).unwrap();
        match cli.command {
            Commands::Update { check: true } => {}
            other => panic!("expected Update {{ check: true }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_update_extra_arg_fails() {
        // Negative: update takes no positional args.
        let result = Cli::try_parse_from(["flow", "update", "unexpected"]);
        assert!(
            result.is_err(),
            "'flow update' with a positional arg should fail"
        );
    }

    #[test]
    fn parse_update_unknown_flag_fails() {
        // Negative: only --check is accepted.
        let result = Cli::try_parse_from(["flow", "update", "--bogus"]);
        assert!(result.is_err(), "unknown flag should fail");
    }

    #[test]
    fn parse_config_init() {
        let cli = Cli::try_parse_from(["flow", "config", "init"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Init { ahk },
            } => {
                // --ahk defaults to false when omitted.
                assert!(!ahk, "--ahk should default to false");
            }
            other => panic!("expected Config::Init, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_init_ahk() {
        let cli = Cli::try_parse_from(["flow", "config", "init", "--ahk"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Init { ahk },
            } => {
                assert!(ahk, "--ahk should set ahk to true");
            }
            other => panic!("expected Config::Init {{ ahk: true }}, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_reload() {
        let cli = Cli::try_parse_from(["flow", "config", "reload"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Reload,
            } => {}
            other => panic!("expected Config::Reload, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_edit() {
        let cli = Cli::try_parse_from(["flow", "config", "edit"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Edit,
            } => {}
            other => panic!("expected Config::Edit, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_path() {
        let cli = Cli::try_parse_from(["flow", "config", "path"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Path,
            } => {}
            other => panic!("expected Config::Path, got: {other:?}"),
        }
    }

    #[test]
    fn parse_config_check() {
        let cli = Cli::try_parse_from(["flow", "config", "check"]).unwrap();
        match cli.command {
            Commands::Config {
                command: ConfigCommands::Check,
            } => {}
            other => panic!("expected Config::Check, got: {other:?}"),
        }
    }

    #[test]
    fn parse_query_binds() {
        let cli = Cli::try_parse_from(["flow", "query", "binds"]).unwrap();
        match cli.command {
            Commands::Query {
                command: QueryCommands::Binds { plain },
            } => assert!(!plain, "--plain defaults to false"),
            other => panic!("expected Query::Binds, got: {other:?}"),
        }
        let cli = Cli::try_parse_from(["flow", "query", "binds", "--plain"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Query {
                command: QueryCommands::Binds { plain: true }
            }
        ));
    }

    /// `main` rewrites niri action names before clap sees them; the rewritten
    /// line must parse as the ordinary subcommand.
    #[test]
    fn niri_alias_lines_parse_as_our_subcommands() {
        let args: Vec<String> = ["flow", "dispatch", "focus-column-left"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let aliased = config::binds::niri_alias(&args).expect("aliased");
        let cli = Cli::try_parse_from(aliased).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Dispatch {
                command: DispatchCommands::Focus {
                    direction: FocusDirection::Left
                }
            }
        ));
        let args: Vec<String> = ["flow", "dispatch", "focus-workspace", "3"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let cli = Cli::try_parse_from(config::binds::niri_alias(&args).expect("aliased")).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Dispatch {
                command: DispatchCommands::SwitchWorkspace { workspace_id: 3 }
            }
        ));
    }

    #[test]
    fn parse_query_all() {
        let cli = Cli::try_parse_from(["flow", "query", "all"]).unwrap();
        match cli.command {
            Commands::Query {
                command: QueryCommands::All,
            } => {}
            other => panic!("expected Query::All, got: {other:?}"),
        }
    }

    // --- Dispatch command parsing ---

    #[test]
    fn parse_dispatch_drag_begin_and_end() {
        for (word, action) in [("begin", DragAction::Begin), ("end", DragAction::End)] {
            let cli = Cli::try_parse_from(["flow", "dispatch", "drag", word]).unwrap();
            match cli.command {
                Commands::Dispatch {
                    command: DispatchCommands::Drag { action: got },
                } => assert_eq!(got, action),
                other => panic!("expected Dispatch::Drag::{word}, got: {other:?}"),
            }
        }
        assert!(Cli::try_parse_from(["flow", "dispatch", "drag"]).is_err());
        assert!(Cli::try_parse_from(["flow", "dispatch", "drag", "cancel"]).is_err());
    }

    #[test]
    fn drag_reply_lines_name_the_kind_and_the_no_session_case() {
        let begin = serde_json::json!({ "drag": "tile", "hwnd": 7 });
        assert_eq!(
            describe_drag_reply(DragAction::Begin, &begin),
            "drag begun (tile)"
        );
        let ended = serde_json::json!({ "ended": true, "drag": "float" });
        assert_eq!(
            describe_drag_reply(DragAction::End, &ended),
            "drag ended (float)"
        );
        let none = serde_json::json!({ "ended": false });
        assert_eq!(
            describe_drag_reply(DragAction::End, &none),
            "no drag session to end"
        );
    }

    #[test]
    fn parse_dispatch_focus_left() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "focus", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Focus {
                        direction: FocusDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::Focus::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_focus_right() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "focus", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Focus {
                        direction: FocusDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::Focus::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_focus_up() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "focus", "up"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Focus {
                        direction: FocusDirection::Up,
                    },
            } => {}
            other => panic!("expected Dispatch::Focus::Up, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_focus_down() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "focus", "down"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Focus {
                        direction: FocusDirection::Down,
                    },
            } => {}
            other => panic!("expected Dispatch::Focus::Down, got: {other:?}"),
        }
    }

    // --- swap-column parsing ---

    #[test]
    fn parse_dispatch_swap_column_left() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "swap-column", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SwapColumn {
                        direction: HorizontalDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::SwapColumn::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_swap_column_right() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "swap-column", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SwapColumn {
                        direction: HorizontalDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::SwapColumn::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_swap_column_without_direction_fails() {
        // Negative: swap-column needs a direction subcommand.
        let result = Cli::try_parse_from(["flow", "dispatch", "swap-column"]);
        assert!(
            result.is_err(),
            "'flow dispatch swap-column' without a direction should fail"
        );
    }

    #[test]
    fn parse_dispatch_swap_column_vertical_fails() {
        // Negative: swap-column only accepts left/right (HorizontalDirection).
        let result = Cli::try_parse_from(["flow", "dispatch", "swap-column", "up"]);
        assert!(
            result.is_err(),
            "'swap-column up' should fail (only left/right)"
        );
    }

    // --- move-window parsing ---

    #[test]
    fn parse_dispatch_move_window_left() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "move-window", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: MoveDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_window_right() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "move-window", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: MoveDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_window_up() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "move-window", "up"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: MoveDirection::Up,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Up, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_window_down() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "move-window", "down"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MoveWindow {
                        direction: MoveDirection::Down,
                    },
            } => {}
            other => panic!("expected Dispatch::MoveWindow::Down, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_window_without_direction_fails() {
        // Negative: move-window needs a direction subcommand.
        let result = Cli::try_parse_from(["flow", "dispatch", "move-window"]);
        assert!(
            result.is_err(),
            "'flow dispatch move-window' without a direction should fail"
        );
    }

    // --- merge-column parsing ---

    #[test]
    fn parse_dispatch_merge_column_left() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "merge-column", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MergeColumn {
                        direction: HorizontalDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::MergeColumn::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_merge_column_right() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "merge-column", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::MergeColumn {
                        direction: HorizontalDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::MergeColumn::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_merge_column_vertical_fails() {
        // Negative: merge-column only accepts left/right (HorizontalDirection).
        let result = Cli::try_parse_from(["flow", "dispatch", "merge-column", "up"]);
        assert!(
            result.is_err(),
            "'merge-column up' should fail (only left/right)"
        );
    }

    // --- promote parsing ---

    #[test]
    fn parse_dispatch_promote_left() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "promote", "left"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Promote {
                        direction: HorizontalDirection::Left,
                    },
            } => {}
            other => panic!("expected Dispatch::Promote::Left, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_promote_right() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "promote", "right"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::Promote {
                        direction: HorizontalDirection::Right,
                    },
            } => {}
            other => panic!("expected Dispatch::Promote::Right, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_promote_vertical_fails() {
        // Negative: promote only accepts left/right (HorizontalDirection).
        let result = Cli::try_parse_from(["flow", "dispatch", "promote", "down"]);
        assert!(
            result.is_err(),
            "'promote down' should fail (only left/right)"
        );
    }

    #[test]
    fn parse_dispatch_without_subcommand_fails() {
        // Negative: `flow dispatch` without a subcommand should fail to parse
        // (clap requires a subcommand).
        let result = Cli::try_parse_from(["flow", "dispatch"]);
        assert!(
            result.is_err(),
            "'flow dispatch' without a subcommand should fail"
        );
    }

    #[test]
    fn parse_dispatch_focus_without_direction_fails() {
        // Negative: `flow dispatch focus` without a direction should fail.
        let result = Cli::try_parse_from(["flow", "dispatch", "focus"]);
        assert!(
            result.is_err(),
            "'flow dispatch focus' without a direction should fail"
        );
    }

    #[test]
    fn parse_dispatch_invalid_subcommand_fails() {
        // Negative: unknown dispatch subcommand should fail.
        let result = Cli::try_parse_from(["flow", "dispatch", "nonexistent"]);
        assert!(result.is_err(), "unknown dispatch subcommand should fail");
    }

    // --- Dispatch expand-column / shrink-column ---

    #[test]
    fn parse_dispatch_expand_column() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "expand-column"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::ExpandColumn,
            } => {}
            other => panic!("expected Dispatch::ExpandColumn, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_shrink_column() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "shrink-column"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::ShrinkColumn,
            } => {}
            other => panic!("expected Dispatch::ShrinkColumn, got: {other:?}"),
        }
    }

    // --- Dispatch center ---
    //
    // `center` is the one variant whose explicit `#[command(name = "center")]`
    // attribute was *removed* in the kebab-case rename (clap derives the same
    // single-word name, so the override was redundant). This test pins that
    // derivation: if a future clap version or refactor changes the derived
    // name, this is the canary that fails.

    #[test]
    fn parse_dispatch_center() {
        // Positive: `flow dispatch center` parses to the Center variant.
        let cli = Cli::try_parse_from(["flow", "dispatch", "center"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::Center,
            } => {}
            other => panic!("expected Dispatch::Center, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_focus_layer_commands() {
        for (word, expect) in [
            ("focus-floating", "FocusFloating"),
            ("focus-tiling", "FocusTiling"),
            (
                "switch-focus-between-floating-and-tiling",
                "SwitchFocusBetweenFloatingAndTiling",
            ),
        ] {
            let cli = Cli::try_parse_from(["flow", "dispatch", word]).unwrap();
            let got = match cli.command {
                Commands::Dispatch {
                    command: DispatchCommands::FocusFloating,
                } => "FocusFloating",
                Commands::Dispatch {
                    command: DispatchCommands::FocusTiling,
                } => "FocusTiling",
                Commands::Dispatch {
                    command: DispatchCommands::SwitchFocusBetweenFloatingAndTiling,
                } => "SwitchFocusBetweenFloatingAndTiling",
                other => panic!("expected {expect}, got: {other:?}"),
            };
            assert_eq!(got, expect, "{word}");
            // Takes no argument.
            assert!(Cli::try_parse_from(["flow", "dispatch", word, "x"]).is_err());
        }
    }

    #[test]
    fn focus_layer_reply_prints_the_daemon_message() {
        let focused = serde_json::json!({
            "focused": "floating", "hwnd": 7, "message": "focused the floating windows"
        });
        assert_eq!(
            describe_focus_layer_reply(&focused),
            "focused the floating windows"
        );
        let empty = serde_json::json!({ "focused": null, "message": "no tiling window to focus" });
        assert_eq!(
            describe_focus_layer_reply(&empty),
            "no tiling window to focus"
        );
        assert_eq!(
            describe_focus_layer_reply(&serde_json::json!({})),
            "focus changed"
        );
    }

    #[test]
    fn parse_dispatch_close_window() {
        // Positive: `flow dispatch close-window` parses to the CloseWindow variant.
        let cli = Cli::try_parse_from(["flow", "dispatch", "close-window"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::CloseWindow,
            } => {}
            other => panic!("expected Dispatch::CloseWindow, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_minimize_window() {
        // Positive: `flow dispatch minimize-window` parses to MinimizeWindow.
        let cli = Cli::try_parse_from(["flow", "dispatch", "minimize-window"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::MinimizeWindow,
            } => {}
            other => panic!("expected Dispatch::MinimizeWindow, got: {other:?}"),
        }
        // Negative: it takes no argument.
        assert!(Cli::try_parse_from(["flow", "dispatch", "minimize-window", "now"]).is_err());
    }

    #[test]
    fn parse_dispatch_close_window_extra_arg_fails() {
        // Negative: close-window takes no arguments.
        let result = Cli::try_parse_from(["flow", "dispatch", "close-window", "extra"]);
        assert!(
            result.is_err(),
            "'flow dispatch close-window' with an extra arg should fail"
        );
    }

    // --- set-window parsing ---

    #[test]
    fn parse_dispatch_set_window_float() {
        // Positive: `flow dispatch set-window float` parses with mode = Float.
        let cli = Cli::try_parse_from(["flow", "dispatch", "set-window", "float"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SetWindow {
                        mode: WindowMode::Float,
                    },
            } => {}
            other => panic!("expected Dispatch::SetWindow::Float, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_set_window_tile() {
        // Positive: `flow dispatch set-window tile` parses with mode = Tile.
        let cli = Cli::try_parse_from(["flow", "dispatch", "set-window", "tile"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SetWindow {
                        mode: WindowMode::Tile,
                    },
            } => {}
            other => panic!("expected Dispatch::SetWindow::Tile, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_set_window_cycle() {
        // Positive: `flow dispatch set-window cycle` parses with mode = Cycle.
        let cli = Cli::try_parse_from(["flow", "dispatch", "set-window", "cycle"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SetWindow {
                        mode: WindowMode::Cycle,
                    },
            } => {}
            other => panic!("expected Dispatch::SetWindow::Cycle, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_set_window_fullscreen() {
        let cli = Cli::try_parse_from(["flow", "dispatch", "set-window", "fullscreen"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command:
                    DispatchCommands::SetWindow {
                        mode: WindowMode::Fullscreen,
                    },
            } => {}
            other => panic!("expected Dispatch::SetWindow::Fullscreen, got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_set_window_without_mode_fails() {
        // Negative: set-window needs a mode subcommand.
        let result = Cli::try_parse_from(["flow", "dispatch", "set-window"]);
        assert!(
            result.is_err(),
            "'flow dispatch set-window' without a mode should fail"
        );
    }

    #[test]
    fn parse_dispatch_set_window_invalid_mode_fails() {
        // Negative: set-window only accepts float/tile/cycle.
        let result = Cli::try_parse_from(["flow", "dispatch", "set-window", "invalid"]);
        assert!(
            result.is_err(),
            "'flow dispatch set-window invalid' should fail"
        );
    }

    #[test]
    fn parse_dispatch_expand_column_extra_arg_fails() {
        // Negative: expand-column takes no arguments.
        let result = Cli::try_parse_from(["flow", "dispatch", "expand-column", "extra"]);
        assert!(
            result.is_err(),
            "'flow dispatch expand-column' with extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_shrink_column_extra_arg_fails() {
        // Negative: shrink-column takes no arguments — extra positional arg rejected.
        let result = Cli::try_parse_from(["flow", "dispatch", "shrink-column", "extra"]);
        assert!(
            result.is_err(),
            "'flow dispatch shrink-column' with extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_expand_column_multiple_extra_args_fails() {
        // Negative: expand-column rejects more than one extra argument.
        let result = Cli::try_parse_from(["flow", "dispatch", "expand-column", "extra1", "extra2"]);
        assert!(
            result.is_err(),
            "'flow dispatch expand-column' with multiple extra args should fail"
        );
    }

    #[test]
    fn parse_dispatch_shrink_column_multiple_extra_args_fails() {
        // Negative: shrink-column rejects more than one extra argument.
        let result = Cli::try_parse_from(["flow", "dispatch", "shrink-column", "extra1", "extra2"]);
        assert!(
            result.is_err(),
            "'flow dispatch shrink-column' with multiple extra args should fail"
        );
    }

    // --- switch-workspace / swap-workspace / move-to-workspace parsing ---

    #[test]
    fn parse_dispatch_switch_workspace() {
        // Positive: `flow dispatch switch-workspace 3` parses with workspace_id = 3.
        let cli = Cli::try_parse_from(["flow", "dispatch", "switch-workspace", "3"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::SwitchWorkspace { workspace_id: 3 },
            } => {}
            other => panic!("expected Dispatch::SwitchWorkspace(3), got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_switch_workspace_without_id_fails() {
        // Negative: switch-workspace needs a workspace_id argument.
        let result = Cli::try_parse_from(["flow", "dispatch", "switch-workspace"]);
        assert!(
            result.is_err(),
            "'flow dispatch switch-workspace' without an id should fail"
        );
    }

    #[test]
    fn parse_dispatch_switch_workspace_zero() {
        // Positive: workspace_id = 0 is accepted by the parser (boundary value).
        // The daemon decides whether 0 is a valid workspace; the CLI does not.
        let cli = Cli::try_parse_from(["flow", "dispatch", "switch-workspace", "0"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::SwitchWorkspace { workspace_id: 0 },
            } => {}
            other => panic!("expected Dispatch::SwitchWorkspace(0), got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_tiling() {
        // Positive: the three switches parse to their variants.
        for (word, state) in [
            ("on", TilingSwitch::On),
            ("off", TilingSwitch::Off),
            ("toggle", TilingSwitch::Toggle),
        ] {
            let cli = Cli::try_parse_from(["flow", "dispatch", "tiling", word]).unwrap();
            match cli.command {
                Commands::Dispatch {
                    command: DispatchCommands::Tiling { state: parsed },
                } => assert_eq!(parsed, state, "{word}"),
                other => panic!("expected Dispatch::Tiling({word}), got: {other:?}"),
            }
        }
        // Negative: the switch is mandatory and must be one of the three.
        assert!(Cli::try_parse_from(["flow", "dispatch", "tiling"]).is_err());
        assert!(Cli::try_parse_from(["flow", "dispatch", "tiling", "maybe"]).is_err());
    }

    #[test]
    fn parse_dispatch_consume_or_expel() {
        for (word, expected) in [("left", Direction::Left), ("right", Direction::Right)] {
            let cli = Cli::try_parse_from(["flow", "dispatch", "consume-or-expel", word]).unwrap();
            match cli.command {
                Commands::Dispatch {
                    command: DispatchCommands::ConsumeOrExpel { direction },
                } => assert_eq!(horizontal_to_direction(direction), expected, "{word}"),
                other => panic!("expected Dispatch::ConsumeOrExpel({word}), got: {other:?}"),
            }
        }
        // Negative: horizontal only, and the direction is mandatory.
        assert!(Cli::try_parse_from(["flow", "dispatch", "consume-or-expel", "up"]).is_err());
        assert!(Cli::try_parse_from(["flow", "dispatch", "consume-or-expel"]).is_err());
    }

    #[test]
    fn parse_dispatch_width() {
        // Positive: the step words and a preset name all parse to `Width`.
        for (word, expected) in [
            ("next", WidthPresetRequest::Next),
            ("prev", WidthPresetRequest::Prev),
            ("cycle", WidthPresetRequest::Cycle),
            ("1/2", WidthPresetRequest::Preset("1/2".into())),
        ] {
            let cli = Cli::try_parse_from(["flow", "dispatch", "width", word]).unwrap();
            match cli.command {
                Commands::Dispatch {
                    command: DispatchCommands::Width { preset },
                } => assert_eq!(WidthPresetRequest::from_cli(&preset), expected, "{word}"),
                other => panic!("expected Dispatch::Width({word}), got: {other:?}"),
            }
        }
        // Negative: the preset argument is mandatory.
        assert!(Cli::try_parse_from(["flow", "dispatch", "width"]).is_err());
    }

    #[test]
    fn parse_dispatch_swap_workspace() {
        // Positive: `flow dispatch swap-workspace 7` parses with workspace_id = 7.
        let cli = Cli::try_parse_from(["flow", "dispatch", "swap-workspace", "7"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::SwapWorkspace { workspace_id: 7 },
            } => {}
            other => panic!("expected Dispatch::SwapWorkspace(7), got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_swap_workspace_without_id_fails() {
        // Negative: swap-workspace needs a workspace_id argument.
        let result = Cli::try_parse_from(["flow", "dispatch", "swap-workspace"]);
        assert!(
            result.is_err(),
            "'flow dispatch swap-workspace' without an id should fail"
        );
    }

    #[test]
    fn parse_dispatch_move_to_workspace() {
        // Positive: `flow dispatch move-to-workspace 11` parses with workspace_id = 11.
        // The CLI command name stays `move-to-workspace` (short form) even though
        // the underlying Rust variant is `MoveWindowToWorkspace`.
        let cli = Cli::try_parse_from(["flow", "dispatch", "move-to-workspace", "11"]).unwrap();
        match cli.command {
            Commands::Dispatch {
                command: DispatchCommands::MoveWindowToWorkspace { workspace_id: 11 },
            } => {}
            other => panic!("expected Dispatch::MoveWindowToWorkspace(11), got: {other:?}"),
        }
    }

    #[test]
    fn parse_dispatch_move_to_workspace_without_id_fails() {
        // Negative: move-to-workspace needs a workspace_id argument.
        let result = Cli::try_parse_from(["flow", "dispatch", "move-to-workspace"]);
        assert!(
            result.is_err(),
            "'flow dispatch move-to-workspace' without an id should fail"
        );
    }

    #[test]
    fn parse_dispatch_workspace_command_rejects_non_numeric_id() {
        // Negative: workspace_id must be a u32; a non-numeric token is rejected.
        let result = Cli::try_parse_from(["flow", "dispatch", "switch-workspace", "abc"]);
        assert!(
            result.is_err(),
            "non-numeric workspace_id should be rejected"
        );
    }

    // --- Negative: invalid invocations ---

    #[test]
    fn parse_no_subcommand_fails() {
        // Negative: no subcommand should fail to parse
        let result = Cli::try_parse_from(["flow"]);
        assert!(result.is_err(), "parsing with no subcommand should fail");
    }

    #[test]
    fn parse_invalid_subcommand_fails() {
        // Negative: unknown subcommand should fail
        let result = Cli::try_parse_from(["flow", "nonexistent"]);
        assert!(result.is_err(), "unknown subcommand should fail");
    }

    #[test]
    fn parse_extra_arg_fails() {
        // Negative: extra argument to a subcommand that takes none should fail
        let result = Cli::try_parse_from(["flow", "stop", "unexpected"]);
        assert!(result.is_err(), "extra argument should fail");
    }

    #[test]
    fn parse_empty_args_fails() {
        // Negative: empty argument list should fail
        let result = Cli::try_parse_from([""]);
        assert!(result.is_err(), "empty args should fail");
    }

    // --- Negative: old flat commands should no longer parse ---

    #[test]
    fn parse_old_reload_config_fails() {
        // Negative: the old `flow reload-config` flat command no longer exists
        let result = Cli::try_parse_from(["flow", "reload-config"]);
        assert!(
            result.is_err(),
            "old 'reload-config' command should no longer parse"
        );
    }

    #[test]
    fn parse_old_check_config_fails() {
        // Negative: the old `flow check-config` flat command no longer exists
        let result = Cli::try_parse_from(["flow", "check-config"]);
        assert!(
            result.is_err(),
            "old 'check-config' command should no longer parse"
        );
    }

    // --- Negative: old squished dispatch subcommands should no longer parse ---
    //
    // The `flow dispatch <sub>` surface was renamed from squished-lowercase
    // (e.g. `swapcolumn`) to kebab-case (`swap-column`). Existing user scripts
    // and keybindings that still spell the old form must now be rejected by
    // clap rather than silently mis-route. Each case below is the OLD form of
    // a subcommand that has a sibling positive test above proving the NEW
    // kebab form parses; together they pin both ends of the rename.
    // `center` is intentionally absent here — it was never renamed.

    #[test]
    fn parse_old_squished_dispatch_subcommands_fail() {
        let old_forms = [
            "swapcolumn",
            "movewindow",
            "expandcolumn",
            "shrinkcolumn",
            "closewindow",
            "setwindow",
            "switchworkspace",
            "swapworkspace",
            "movetoworkspace",
        ];
        for old in old_forms {
            let result = Cli::try_parse_from(["flow", "dispatch", old]);
            assert!(
                result.is_err(),
                "old squished dispatch subcommand '{old}' should no longer parse"
            );
        }
    }

    // --- Negative: invalid config subcommand ---

    #[test]
    fn parse_config_invalid_subcommand_fails() {
        let result = Cli::try_parse_from(["flow", "config", "nonexistent"]);
        assert!(result.is_err(), "unknown config subcommand should fail");
    }

    // --- Negative: config subcommand with extra args ---

    #[test]
    fn parse_config_subcommand_extra_arg_fails() {
        let result = Cli::try_parse_from(["flow", "config", "path", "unexpected"]);
        assert!(
            result.is_err(),
            "extra argument to config subcommand should fail"
        );
    }

    // --- DAEMON_START_TIMEOUT wiring tests ---

    // Positive: DAEMON_START_TIMEOUT is a reasonable bounded value
    #[test]
    fn daemon_start_timeout_is_reasonable() {
        assert!(
            DAEMON_START_TIMEOUT.as_secs() > 0,
            "DAEMON_START_TIMEOUT must be > 0"
        );
        assert!(
            DAEMON_START_TIMEOUT.as_secs() <= 30,
            "DAEMON_START_TIMEOUT must be <= 30s (user shouldn't wait longer)"
        );
    }

    // Negative: DAEMON_START_TIMEOUT is not zero (would mean no wait)
    #[test]
    fn daemon_start_timeout_is_not_zero() {
        assert_ne!(
            DAEMON_START_TIMEOUT,
            Duration::ZERO,
            "zero timeout would skip waiting entirely"
        );
    }

    // Positive: wait_for_daemon() reports an early daemon exit as a staged
    // failure. A child that exits immediately (`cmd.exe /c exit 3`) and never
    // writes a status file must yield the "before reporting any step" line
    // with the expected log path and the child's exit code.
    #[test]
    fn wait_for_daemon_reports_early_exit_without_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("flowd.log");
        let mut child = Command::new("cmd.exe")
            .args(["/c", "exit", "3"])
            .spawn()
            .expect("spawn cmd");
        let err = wait_for_daemon(&mut child, dir.path(), &log).unwrap_err();
        assert_eq!(err.exit_code, 3);
        assert!(
            err.message
                .starts_with("FlowWM failed to start at step 'unknown': daemon exited with"),
            "{}",
            err.message
        );
        assert!(err.message.ends_with(&format!("(log: {})", log.display())));
    }

    // Positive: a status file written by the spawned pid with `state: error`
    // is reported with that stage's exit code, even though the child is
    // still alive (the daemon writes the file just before exiting).
    #[test]
    fn wait_for_daemon_reports_error_status_for_own_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("flowd.log");
        // A child that outlives the poll: wait long enough for one iteration.
        let mut child = Command::new("cmd.exe")
            .args(["/c", "ping -n 3 127.0.0.1 > nul"])
            .spawn()
            .expect("spawn cmd");
        let status = StartupStatus {
            pid: child.id(),
            stage: flow_wm::daemon::StartupStage::Hooks,
            state: StartupState::Error,
            reason: "injected failure (FLOW_FAIL_AT)".to_owned(),
            log_path: Some(dir.path().join("daemon-chosen.log")),
            tiling: startup::TilingStatus::On,
        };
        startup::write_status(dir.path(), &status).unwrap();
        let err = wait_for_daemon(&mut child, dir.path(), &log).unwrap_err();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(err.exit_code, 15);
        assert_eq!(
            err.message,
            format!(
                "FlowWM failed to start at step 'hooks': injected failure (FLOW_FAIL_AT) (log: {})",
                dir.path().join("daemon-chosen.log").display()
            )
        );
    }

    // --- resolve_editor tests ---

    #[test]
    fn resolve_editor_returns_string() {
        let result = resolve_editor();
        assert!(result.is_ok(), "resolve_editor should always return Ok");
        let editor = result.unwrap();
        assert!(!editor.is_empty(), "editor command should not be empty");
    }

    // --- preflight_config_check tests ---
    //
    // `preflight_config_check` was changed in the start-time update-notification
    // feature to RETURN the parsed `FlowConfig` (previously `Result<(), String>`)
    // so `cmd_start` can reuse it for `maybe_notify_update` without a second file
    // read. These tests pin that new contract: the returned config reflects the
    // user's `flow.toml`, including the `check_for_updates` flag that gates the
    // notification. All cases use temp dirs -- no network, no Win32, fully
    // deterministic. `flow-rules.toml` is intentionally absent (benign -> default
    // rules), matching the fresh-install path.
    use std::fs;

    /// Positive (nominal): a valid `flow.toml` that opts out of update checks is
    /// parsed and returned with `check_for_updates = false`.
    ///
    /// This is the exact seam feeding `maybe_notify_update`: a regression that
    /// discards the parsed config (e.g. returns `FlowConfig::default()`) would
    /// flip this back to `true` and silently re-enable notifications despite the
    /// user's opt-out.
    #[test]
    fn preflight_config_check_returns_parsed_check_for_updates_flag() {
        // Arrange: valid flow.toml opting OUT of update checks.
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join("flow.toml"), "check_for_updates = false\n")
            .expect("write flow.toml");

        // Act: run the pre-flight check against the temp config dir.
        let config = preflight_config_check(tmp.path()).expect("valid config should load");

        // Assert: the returned config carries the file's flag through.
        assert!(
            !config.check_for_updates,
            "preflight_config_check must return the parsed config, not a default"
        );
    }

    /// Positive (edge): an empty config dir (fresh install, no `flow.toml`) yields
    /// the compiled defaults, so `check_for_updates` is OFF and nothing is
    /// contacted at start. Confirms the fork's opt-in policy on a clean
    /// install.
    #[test]
    fn preflight_config_check_missing_flow_toml_yields_default_disabled() {
        // Arrange: empty config dir (neither config file present).
        let tmp = tempfile::tempdir().expect("tempdir");

        // Act: pre-flight on a dir with no config files at all.
        let config = preflight_config_check(tmp.path()).expect("missing flow.toml is benign");

        // Assert: compiled defaults apply verbatim.
        assert!(!config.check_for_updates);
        assert_eq!(config, config::FlowConfig::default());
    }

    /// Positive: a `flow.toml` that opts IN is honoured — the key still works.
    #[test]
    fn preflight_config_check_honours_opt_in() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join("flow.toml"), "check_for_updates = true\n")
            .expect("write flow.toml");
        let config = preflight_config_check(tmp.path()).expect("valid config should load");
        assert!(config.check_for_updates);
    }

    /// Negative: a `flow.toml` that exists but cannot parse is fatal -- startup
    /// must be refused with the canonical reason so the user sees why on their
    /// terminal (the detached daemon's own error output is discarded). The
    /// `flow: ` prefix is added once, by `main`'s error printer, so the
    /// reason itself must not carry it (it used to print `flow: flow: …`).
    #[test]
    fn preflight_config_check_rejects_malformed_flow_toml() {
        // Arrange: a flow.toml that exists but is unparseable.
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join("flow.toml"), "not toml {{{{").expect("write flow.toml");

        // Act: pre-flight must refuse to start.
        let result = preflight_config_check(tmp.path());

        // Assert: fatal error with the canonical reason, no doubled prefix.
        assert!(result.is_err(), "malformed flow.toml must be fatal");
        let err = result.unwrap_err();
        assert!(
            err.starts_with("cannot start:"),
            "error must use the canonical 'cannot start:' reason, got {err:?}"
        );
        assert!(
            !err.contains("flow:"),
            "the caller adds the single 'flow: ' prefix: {err:?}"
        );
    }
}
