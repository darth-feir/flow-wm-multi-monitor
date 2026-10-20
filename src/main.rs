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
//! Exit code `0` means a clean stop (`flow stop`, logoff, console signal).
//! Exit code **`2`** means a runtime failure — a panic on any thread, a dead
//! animator or hook thread — after which the daemon has already **failed
//! back** (see [`FlowWM::fail_back`]): rescued windows, dropped its border
//! overlays, stopped the hook thread and killed the hotkey script, so the
//! user is left with a plain Windows desktop rather than a half-managed
//! one. `FLOW_FAIL_AT=<stage>` injects a failure at that stage for testing
//! (see [`flow_wm::daemon::startup`]); `FLOW_FAIL_AT=runtime` injects a
//! panic in the event loop to exercise the fail-back path.
//!
//! # Panics
//!
//! A process-wide panic hook rewrites `flowd.status` with `stage: runtime`,
//! `state: error` and then logs the panic location and message, before
//! unwinding starts (the status file first: it is what `flow start` and a
//! human read, and logging can block on a wedged file). `flow.run()` runs
//! under `catch_unwind`, so a main-thread panic still reaches `fail_back`; a
//! panic on the animator worker marks it dead and the event loop fails back
//! on its next wake with the panic message in the reason. The Win32
//! callbacks (hook thread, session sink, border overlays) catch their own
//! panics and request a shutdown, so they reach `fail_back` too; the
//! kill-on-close job object remains the backstop for an outright abort.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use clap::Parser;

use flow_wm::animation::panic_payload_message;
use flow_wm::config::{
    WindowRulesConfig, dirs, init_config_dir, load_app_config, load_default_rules,
    load_rules_config,
};
use flow_wm::daemon::session;
use flow_wm::daemon::startup::{self, AtStage, EXIT_CODE_RUNTIME};
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

    /// Launch `<config_dir>/flow.ahk` once the daemon is ready, inside a
    /// kill-on-close job object so the script dies with the daemon.
    /// Forwarded by `flow start --ahk`.
    #[arg(long)]
    ahk: bool,

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
/// Exits `0` on a clean stop, `10 + stage index` when startup fails at a
/// [`StartupStage`] (see the module docs for the table), and
/// [`EXIT_CODE_RUNTIME`] (`2`) after a runtime failure / panic.
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

    let reporter = StartupReporter::new(
        config_dir.clone(),
        log_path.clone(),
        startup::fail_at_from_env(),
    );

    // From here on every panic — on any thread — is logged and recorded in
    // the status file before unwinding, so `flow start` and a human reading
    // flowd.status both learn what happened even if the process then dies.
    install_panic_hook(config_dir.clone(), log_path);

    // Console runs: Ctrl-C / console close / logoff / shutdown become a
    // graceful stop. Harmless when detached (no console → never fires).
    if let Err(e) = session::install_console_ctrl_handler() {
        log::warn!("flowd: {e}; console signals will not trigger a graceful stop");
    }

    // Always leave through an explicit exit: the status file has been written
    // by now, and nothing below may depend on destructors or on helper
    // threads (accept, animator, hooks) winding down on their own.
    let code = match run(args, config_dir, &reporter) {
        Ok(RunOutcome::Clean) => 0,
        Ok(RunOutcome::FailedBack) => EXIT_CODE_RUNTIME,
        Err(err) => {
            reporter.fail(&err);
            log::error!("flowd: fatal error: {err}");
            err.exit_code()
        }
    };
    log::info!("flowd: exiting with code {code}");
    std::process::exit(code);
}

/// How the event loop ended, once the teardown has run.
#[derive(Debug, PartialEq, Eq)]
enum RunOutcome {
    /// Clean stop; exit `0`.
    Clean,
    /// Runtime failure (panic, dead thread); fail-back has run; exit `2`.
    FailedBack,
}

/// Install the process-wide panic hook.
///
/// The hook first rewrites `<config_dir>/flowd.status` with
/// [`startup::panic_status`] (`stage: runtime`, `state: error`), then logs
/// `panic at <file>:<line>:<col>: <message>` at `error`, and then defers to
/// the default hook (which prints the panic to stderr). The status file
/// comes first on purpose: it is the one artefact `flow start` and a human
/// can always read, whereas the logger may itself be the thing that is
/// wedged (a full disk, a locked file) — so it must not stand between the
/// panic and the status. It never panics itself: a failed status write is
/// logged and ignored.
fn install_panic_hook(config_dir: PathBuf, log_path: Option<PathBuf>) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let message = panic_payload_message(info.payload());
        let status = startup::panic_status(
            std::process::id(),
            log_path.clone(),
            location.as_deref(),
            &message,
        );
        let status_write = startup::write_status(&config_dir, &status);
        let thread = std::thread::current();
        let reason = startup::panic_reason(location.as_deref(), &message);
        log::error!(
            "flowd: {reason} (thread {:?})",
            thread.name().unwrap_or("<unnamed>")
        );
        if let Err(e) = status_write {
            log::warn!(
                "flowd: could not write {} from the panic hook: {e}",
                startup::status_file_path_in(&config_dir).display()
            );
        }
        default_hook(info);
    }));
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
/// Returns `Ok(Clean)` on a clean stop, `Ok(FailedBack)` after a runtime
/// failure (the fail-back has run; the caller exits `2`), and `Err` naming
/// the failing startup stage. Once a `FlowWM` exists, every error path
/// rescues stranded windows before returning.
fn run(
    args: Args,
    config_dir: PathBuf,
    reporter: &StartupReporter,
) -> Result<RunOutcome, StartupError> {
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
        flow.fail_back(&err.to_string(), false);
        return Err(err);
    }

    // 6. Hotkey script — only once the daemon is listening, so the first
    //    hotkey lands. Tied to this process by a kill-on-close job object.
    if args.ahk {
        match flow.launch_ahk() {
            Some(pid) => log::info!("startup: flow.ahk launched (pid {pid})"),
            None => log::warn!("startup: flow.ahk was not launched; hotkeys are unavailable"),
        }
    }

    // 7. Event loop, under catch_unwind so a main-thread panic still reaches
    //    the fail-back below. `FLOW_FAIL_AT=runtime` panics here on purpose.
    let inject_runtime_panic = reporter.fail_at() == Some(S::Runtime);
    let exit = catch_unwind(AssertUnwindSafe(|| {
        if inject_runtime_panic {
            panic!("injected panic ({}=runtime)", startup::FAIL_AT_ENV);
        }
        flow.run()
    }));

    // 8. Teardown — one path for every exit. Itself guarded: a second panic
    //    must not skip the status file and exit code.
    let (reason, clean) = match &exit {
        Ok(run_exit) => (run_exit.reason(), run_exit.is_clean()),
        Err(payload) => (
            format!(
                "panic in event loop: {}",
                panic_payload_message(payload.as_ref())
            ),
            false,
        ),
    };
    let teardown = catch_unwind(AssertUnwindSafe(|| flow.fail_back(&reason, clean)));
    if let Err(payload) = teardown {
        log::error!(
            "flowd: fail-back itself panicked: {}",
            panic_payload_message(payload.as_ref())
        );
    }

    let outcome = if clean {
        reporter.stopped();
        RunOutcome::Clean
    } else {
        // A panic already wrote an error status from the hook; rewrite it
        // with the final reason either way so the file reflects the exit.
        reporter.runtime_failure(&reason);
        RunOutcome::FailedBack
    };
    // Explicit, so a blocking destructor can never keep the process alive
    // after the teardown has run (the accept thread is already stopped by
    // `fail_back`; this is the belt to that suspender).
    drop(flow);
    Ok(outcome)
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
