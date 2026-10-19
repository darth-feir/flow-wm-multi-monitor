//! flowd — FlowWM daemon
//!
//! This is the main daemon process that owns all state, manages windows,
//! and responds to IPC commands from the `flow` CLI client.
//!
//! The daemon:
//! 1. Optionally switches to a test desktop (`--desktop` flag, debug only)
//! 2. Loads configuration from `flow.toml` and `flow-rules.toml`
//! 3. Constructs a [`FlowWM`](flow_wm::daemon::FlowWM)
//!    which performs all initialization (window scan, layout init, hook setup)
//! 4. Enters the IPC event loop via `flow.run()`
//!
//! # Staged startup and exit codes
//!
//! Every startup step is a [`StartupStage`]. Progress is logged as
//! `startup: [n/N] <stage> ok` and mirrored into `<config_dir>/flowd.status`
//! so `flow start` can report which step failed. On failure the daemon logs
//! `startup: FAILED at <stage>: <reason>`, rescues any windows it already
//! moved, and exits with **`10 + stage index`** (see
//! [`StartupStage::exit_code`]):
//!
//! | Exit code | Stage |
//! |-----------|-------|
//! | 10 | `logging` |
//! | 11 | `config` |
//! | 12 | `monitors` |
//! | 13 | `scan` |
//! | 14 | `pipe` |
//! | 15 | `hooks` |
//! | 16 | `animator` |
//! | 17 | `initial_layout` |
//! | 18 | `borders` |
//! | 19 | `loadout_restore` |
//! | 20 | `ready` |
//!
//! Exit code `0` means a clean stop (`flow stop`). `FLOW_FAIL_AT=<stage>`
//! injects a failure at that stage for testing (see
//! [`flow_wm::daemon::startup`]).

use std::path::{Path, PathBuf};

use clap::Parser;

use flow_wm::config::{
    WindowRulesConfig, dirs, init_config_dir, load_app_config, load_default_rules,
    load_rules_config,
};
use flow_wm::daemon::startup::{self, AtStage};
use flow_wm::daemon::{FlowWM, StartupError, StartupReporter, StartupStage};
use flow_wm::logging;

#[cfg(debug_assertions)]
use flow_wm::registry::desktop;

/// Daemon CLI arguments.
#[derive(Parser)]
#[command(name = "flowd", version, about = "FlowWM daemon")]
#[command(propagate_version = true)]
struct Args {
    /// Config directory path. Overrides `FLOW_CONFIG_DIR` env var and default path.
    /// Usually set by `flow start --config` which passes it via `FLOW_CONFIG_DIR`.
    #[arg(long)]
    config: Option<String>,

    /// Optional log file path. When set, ALL logging is redirected to this
    /// exact file (truncated on each start) instead of the default
    /// date-stamped daily log under `<config_dir>/logs/`. Intended for
    /// capturing a clean, isolated log for a single debugging run — the
    /// file starts empty each launch so only that run's output is recorded.
    /// The log level is controlled by `--verbose` / `--log-level`, then
    /// `RUST_LOG` (defaulting to `debug` in debug builds, `info` in release).
    #[arg(long, value_name = "PATH")]
    log_file: Option<String>,

    /// Log at `debug` level (default is `info` in release builds). Shorthand
    /// for `--log-level debug`; `--log-level` wins when both are given.
    #[arg(long, short = 'v')]
    verbose: bool,

    /// Explicit log level / `env_logger` filter (e.g. `trace`,
    /// `flow_wm::registry=trace`). Overrides `--verbose` and `RUST_LOG`.
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

    /// Skip auto-restore of the last loadout at startup. Forwarded by
    /// `flow start --no-restore`. When absent, the daemon restores the
    /// config-default loadout after initialization; a missing loadout
    /// silently starts fresh.
    #[arg(long)]
    no_restore: bool,

    /// Desktop name for test mode (opens and switches to this desktop).
    /// Only available in debug builds.
    #[cfg(debug_assertions)]
    #[arg(long)]
    desktop: Option<String>,
}

impl Args {
    /// The effective log-level override: `--log-level` beats `--verbose`;
    /// `None` leaves the choice to `RUST_LOG` / the build default.
    fn level_override(&self) -> Option<&str> {
        match (&self.log_level, self.verbose) {
            (Some(level), _) => Some(level.as_str()),
            (None, true) => Some("debug"),
            (None, false) => None,
        }
    }
}

/// Daemon entry point.
///
/// Exits `0` on a clean stop and `10 + stage index` when startup fails at a
/// [`StartupStage`] (see the module docs for the table).
fn main() {
    // Parse CLI arguments first so the `--log-file` / level overrides are
    // available to the logger. Clap handles `--help`/`--version` and usage
    // errors by printing to stderr and exiting, which needs no logger.
    let args = Args::parse();

    // Resolve the config directory before anything else: the logger writes
    // under it, and so does the status file. Priority chain:
    // --config flag > FLOW_CONFIG_DIR env var > ~/.config/flow/ default.
    let config_dir = dirs::resolve_config_dir(args.config.as_deref().map(Path::new));

    // Logging is always brought up first so every later stage — including a
    // config parse error — lands in the log file. It never fails: a bad log
    // directory falls back to stderr only.
    let log_path = logging::init(
        &config_dir,
        args.log_file.as_deref().map(Path::new),
        args.level_override(),
    );
    log::info!("using config directory: {}", config_dir.display());

    let reporter = StartupReporter::new(config_dir.clone(), log_path, startup::fail_at_from_env());

    if let Err(err) = run(args, config_dir, &reporter) {
        reporter.fail(&err);
        log::error!("flowd: fatal error: {err}");
        std::process::exit(err.exit_code());
    }
}

/// Run the daemon: load config, build flow, run event loop.
///
/// This function is the thin orchestration layer between CLI argument
/// parsing and the [`FlowWM`] constructor. All actual daemon
/// logic lives inside `FlowWM`.
///
/// # Stages
///
/// 1. `logging` — the logger is already up (see [`main`]); this is where a
///    `FLOW_FAIL_AT=logging` injection fires.
/// 2. `config` — optional test-desktop switch (debug builds only), config
///    directory init, `flow.toml` + rules parsing.
/// 3. `monitors` … `borders` — inside [`FlowWM::new`].
/// 4. `loadout_restore` — unless `--no-restore`, auto-restore the last
///    loadout (best-effort).
/// 5. `ready` — the status file flips to `ready`, then `flow.run()` enters
///    the IPC event loop.
///
/// Returns `Ok` on `Stop`; `Err` names the failing stage. Once a `FlowWM`
/// exists, every error path rescues stranded windows before returning.
fn run(args: Args, config_dir: PathBuf, reporter: &StartupReporter) -> Result<(), StartupError> {
    use StartupStage as S;

    // 1. Logging is up (main). Report it so the status file exists from
    //    the first stage on, and honour an injected failure.
    reporter.begin(S::Logging)?;
    reporter.ok(S::Logging);

    // 2. Config.
    reporter.begin(S::Config)?;

    // Optional: switch to test desktop (debug builds only).
    #[cfg(debug_assertions)]
    if let Some(ref name) = args.desktop {
        desktop::switch_to_desktop(name).at(S::Config)?;
    }

    // Ensure default config files exist (idempotent — safe to call every startup).
    init_config_dir(&config_dir).at(S::Config)?;

    // CODE is the single source of truth: every field carries a serde default
    // (see `config::defaults`), so the user's `flow.toml` may be partial or even
    // empty. There is no shipped-defaults TOML merged at runtime.
    //
    // `flow.toml` is authoritative: a parse/IO error means the user's stated
    // preferences are unreadable, so refuse to start rather than silently
    // running with defaults. The `flow` client surfaces the same error to the
    // user's terminal via a pre-flight check before we get here; this is the
    // daemon-side backstop for a config change in the race window.
    let app_config = load_app_config(&dirs::user_app_config_path_in(&config_dir)).at(S::Config)?;
    // `flow-rules.toml` is advisory: a parse error falls back to default rules
    // rather than aborting — the daemon stays usable (classifies windows as float).
    let user_rules =
        load_rules_config(&dirs::user_rules_path_in(&config_dir)).unwrap_or_else(|e| {
            log::warn!("flowd: {e}; using default window rules");
            WindowRulesConfig::default()
        });
    let default_rules = load_default_rules();
    reporter.ok(S::Config);

    // Resolve desktop name for hook thread (debug builds pass it through;
    // release builds always use None).
    #[cfg(debug_assertions)]
    let desktop_name = args.desktop.clone();
    #[cfg(not(debug_assertions))]
    let desktop_name = None;

    // 3. Build the daemon — flow owns everything from here. `new` reports
    //    its own stages and rescues on its own failure paths.
    let mut flow = FlowWM::new(
        app_config,
        user_rules,
        default_rules,
        config_dir,
        desktop_name,
        reporter,
    )?;

    // 4./5. Loadout restore + ready. Windows have been moved by now, so any
    //       failure here must rescue them before the process exits.
    if let Err(err) = finish_startup(&mut flow, args.no_restore, reporter) {
        flow.rescue_stranded_windows();
        return Err(err);
    }

    flow.run();

    // Graceful exit (Stop command or fatal wait error): rescue any windows
    // stranded off-screen before tearing down. See `daemon::shutdown`.
    flow.rescue_stranded_windows();

    Ok(())
}

/// The post-construction stages: `loadout_restore` and `ready`.
///
/// Split out of [`run`] so a failure in either can be rescued uniformly by
/// the caller, which still owns the [`FlowWM`].
fn finish_startup(
    flow: &mut FlowWM,
    no_restore: bool,
    reporter: &StartupReporter,
) -> Result<(), StartupError> {
    use StartupStage as S;

    // Auto-restore the last loadout unless the user opted out. Runs
    // in-process (not via IPC), after the registry + layout are initialized
    // and before the event loop starts, so there is no startup pipe race.
    // Best-effort: a missing/unmatched loadout never blocks startup.
    reporter.begin(S::LoadoutRestore)?;
    if no_restore {
        log::info!("startup: loadout restore skipped (--no-restore)");
    } else {
        flow.try_restore_loadout_default();
    }
    reporter.ok(S::LoadoutRestore);

    reporter.begin(S::Ready)?;
    log::info!("flowd: daemon initialized successfully");
    reporter.ok(S::Ready);
    Ok(())
}
