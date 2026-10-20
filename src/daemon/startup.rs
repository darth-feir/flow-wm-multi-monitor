//! Staged startup: which step failed, and telling the outside world.
//!
//! `flowd` initialises in a fixed sequence of [`StartupStage`]s. Every
//! fallible step maps its error to a [`StartupError`] naming the stage, so a
//! failed boot is always reported as *"failed at step 'hooks': …"* rather
//! than as an opaque timeout. Progress is surfaced three ways:
//!
//! 1. **Log lines** — `startup: [n/N] <stage> ok` at `info`, and
//!    `startup: FAILED at <stage>: <reason>` at `error`.
//! 2. **Status file** — `<config_dir>/flowd.status`, a small JSON document
//!    ([`StartupStatus`]) rewritten after every stage and on failure. The
//!    `flow start` client polls it (and compares `pid`) so it can print the
//!    failing stage and the log path instead of guessing.
//! 3. **Exit code** — the daemon exits with `10 + stage index` on failure
//!    (see [`StartupStage::exit_code`]), so a supervisor or script can tell
//!    stages apart without parsing output.
//!
//! # After boot: the `runtime` pseudo-stage
//!
//! Once `ready` has been reported the status file keeps describing the
//! daemon's life: a panic or a fail-back (animator or hook thread died,
//! session ending) rewrites it with `stage: "runtime"`, `state: "error"` and
//! the reason, and the process exits with [`EXIT_CODE_RUNTIME`] (`2`). A
//! clean `flow stop` leaves `stage: "runtime"`, `state: "stopped"`.
//! [`StartupStage::Runtime`] is not a boot stage: it is never announced as
//! `[n/N]`, and `FLOW_FAIL_AT=runtime` injects a **panic** in the event
//! loop instead of a startup failure, so the fail-back path can be exercised
//! end to end in the VM.
//!
//! # Fault injection (test-only)
//!
//! Setting the environment variable `FLOW_FAIL_AT` to a stage name (e.g.
//! `FLOW_FAIL_AT=hooks`) makes the daemon fail at that stage with the reason
//! `injected failure (FLOW_FAIL_AT)` instead of running the step. It is
//! honoured in release builds too — it is only an environment variable, and
//! it lets the VM test-bed prove that every stage reports and rescues
//! correctly. An unknown value is ignored with a warning.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Environment variable read by [`fail_at_from_env`] for test-only fault
/// injection. Its value is a [`StartupStage`] snake-case name.
pub const FAIL_AT_ENV: &str = "FLOW_FAIL_AT";

/// Name of the status file written under the config directory.
pub(crate) const STATUS_FILE_NAME: &str = "flowd.status";

/// Exit code of the daemon when startup fails at stage index 0; each later
/// stage adds its index (see [`StartupStage::exit_code`]).
pub(crate) const EXIT_CODE_BASE: i32 = 10;

/// Exit code of the daemon after a runtime failure: a panic (any thread), a
/// dead animator or hook thread, or a wait failure in the event loop. The
/// fail-back (rescue, borders, hooks, hotkey script) has run by then.
pub const EXIT_CODE_RUNTIME: i32 = 2;

/// A step of the daemon's startup sequence, in execution order.
///
/// # Exit-code mapping
///
/// `flowd` exits with `10 + index` when a stage fails:
///
/// | Stage | Index | Exit code |
/// |-------|-------|-----------|
/// | `logging` | 0 | 10 |
/// | `config` | 1 | 11 |
/// | `monitors` | 2 | 12 |
/// | `scan` | 3 | 13 |
/// | `pipe` | 4 | 14 |
/// | `hooks` | 5 | 15 |
/// | `animator` | 6 | 16 |
/// | `initial_layout` | 7 | 17 |
/// | `borders` | 8 | 18 |
/// | `loadout_restore` | 9 | 19 |
/// | `ready` | 10 | 20 |
///
/// Logging comes first so that every later stage (including config parsing)
/// is captured in the log file; borders are attached after the initial
/// layout because overlays seat at the laid-out rects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupStage {
    /// Logger initialised (log file resolved under the config directory).
    Logging,
    /// Config directory initialised and `flow.toml` / rules parsed.
    Config,
    /// Monitor geometry enumerated and per-monitor scrolling spaces built.
    Monitors,
    /// Existing top-level windows scanned into the registry.
    Scan,
    /// IPC named pipe created (fails if another daemon owns it).
    Pipe,
    /// WinEvent hook thread started and the mandatory hooks registered.
    Hooks,
    /// Animator worker started.
    Animator,
    /// Initial layout submitted — windows start moving from here on.
    InitialLayout,
    /// Pre-existing floats adopted and border overlays attached.
    Borders,
    /// Last loadout restored (skipped with `--no-restore`, still reported).
    LoadoutRestore,
    /// Everything done; the event loop is about to start.
    Ready,
    /// Not a boot stage: the daemon was past `ready` when the status was
    /// written (runtime failure, fail-back or clean stop). Reads as index
    /// `ALL.len()` and exit code [`EXIT_CODE_RUNTIME`].
    Runtime,
}

impl StartupStage {
    /// Every stage in execution order.
    pub const ALL: [StartupStage; 11] = [
        StartupStage::Logging,
        StartupStage::Config,
        StartupStage::Monitors,
        StartupStage::Scan,
        StartupStage::Pipe,
        StartupStage::Hooks,
        StartupStage::Animator,
        StartupStage::InitialLayout,
        StartupStage::Borders,
        StartupStage::LoadoutRestore,
        StartupStage::Ready,
    ];

    /// Zero-based position of this stage in [`Self::ALL`]; `ALL.len()` for
    /// the [`Runtime`](Self::Runtime) pseudo-stage (one past the last boot
    /// stage).
    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|s| *s == self)
            .unwrap_or(Self::ALL.len())
    }

    /// Process exit code the daemon uses when this stage fails:
    /// `EXIT_CODE_BASE` plus [`index`](Self::index), or
    /// [`EXIT_CODE_RUNTIME`] for [`Runtime`](Self::Runtime).
    #[must_use]
    pub fn exit_code(self) -> i32 {
        if self == Self::Runtime {
            EXIT_CODE_RUNTIME
        } else {
            EXIT_CODE_BASE + self.index() as i32
        }
    }

    /// Snake-case name as used in logs, the status file and `FLOW_FAIL_AT`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            StartupStage::Logging => "logging",
            StartupStage::Config => "config",
            StartupStage::Monitors => "monitors",
            StartupStage::Scan => "scan",
            StartupStage::Pipe => "pipe",
            StartupStage::Hooks => "hooks",
            StartupStage::Animator => "animator",
            StartupStage::InitialLayout => "initial_layout",
            StartupStage::Borders => "borders",
            StartupStage::LoadoutRestore => "loadout_restore",
            StartupStage::Ready => "ready",
            StartupStage::Runtime => "runtime",
        }
    }

    /// Parse a snake-case stage name (the inverse of [`name`](Self::name)).
    /// Matching is case-insensitive and tolerates surrounding whitespace.
    /// `"runtime"` parses to the pseudo-stage (for `FLOW_FAIL_AT=runtime`).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let wanted = name.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .chain(std::iter::once(Self::Runtime))
            .find(|s| s.name() == wanted)
    }
}

impl fmt::Display for StartupStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A startup failure: the stage that failed and why.
///
/// Displays as `failed at step '<stage>': <reason>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupError {
    /// The stage that failed.
    pub stage: StartupStage,
    /// Human-readable cause.
    pub reason: String,
}

impl StartupError {
    /// Build an error for `stage` from any displayable reason.
    pub fn new(stage: StartupStage, reason: impl fmt::Display) -> Self {
        Self {
            stage,
            reason: reason.to_string(),
        }
    }

    /// Process exit code for this failure (see [`StartupStage::exit_code`]).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.stage.exit_code()
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed at step '{}': {}", self.stage, self.reason)
    }
}

impl std::error::Error for StartupError {}

/// Map a plain `Result<T, E>` onto a [`StartupError`] for `stage`.
///
/// Extension trait so call sites read `step.at(StartupStage::Pipe)?`.
pub trait AtStage<T> {
    /// Attribute the error (if any) to `stage`.
    fn at(self, stage: StartupStage) -> Result<T, StartupError>;
}

impl<T, E: fmt::Display> AtStage<T> for Result<T, E> {
    fn at(self, stage: StartupStage) -> Result<T, StartupError> {
        self.map_err(|e| StartupError::new(stage, e))
    }
}

/// Lifecycle state recorded in the status file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupState {
    /// Still booting; `stage` is the last stage that completed.
    Starting,
    /// Boot finished; the daemon is serving the pipe.
    Ready,
    /// Boot failed at `stage`; `reason` says why — or, with `stage ==
    /// runtime`, the daemon failed after boot (panic, dead thread) and has
    /// failed back. The process is exiting.
    Error,
    /// The daemon stopped cleanly (`flow stop`, session end) after running
    /// its rescue; `stage` is `runtime`.
    Stopped,
}

/// Contents of `<config_dir>/flowd.status`.
///
/// Rewritten after every stage. Readers must compare `pid` with the process
/// they spawned: a stale file from an earlier daemon carries that daemon's
/// pid, never the new one's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupStatus {
    /// Process id of the daemon that wrote the file.
    pub pid: u32,
    /// Last stage completed (`starting`/`ready`) or the failing stage (`error`).
    pub stage: StartupStage,
    /// Where the daemon is in its lifecycle.
    pub state: StartupState,
    /// Failure reason; empty unless `state == Error`.
    #[serde(default)]
    pub reason: String,
    /// The log file this daemon writes to (`None` when logging fell back to
    /// stderr only).
    #[serde(default)]
    pub log_path: Option<PathBuf>,
}

/// Path of the status file inside `config_dir`.
#[must_use]
pub fn status_file_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join(STATUS_FILE_NAME)
}

/// Serialise `status` as the pretty JSON document the status file holds
/// and write it to `w`. Pure over the writer, so the panic hook's output
/// can be tested without touching the filesystem.
///
/// # Errors
///
/// Returns the serialisation or write error.
pub(crate) fn write_status_to<W: std::io::Write>(
    w: &mut W,
    status: &StartupStatus,
) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(status).map_err(std::io::Error::other)?;
    w.write_all(json.as_bytes())
}

/// Write `status` atomically to `<config_dir>/flowd.status`.
///
/// The JSON is written to a sibling temp file and renamed over the target so
/// a concurrent reader never sees a half-written document.
///
/// # Errors
///
/// Returns the I/O error if the file cannot be written or renamed.
pub fn write_status(config_dir: &Path, status: &StartupStatus) -> std::io::Result<()> {
    let path = status_file_path_in(config_dir);
    let tmp = config_dir.join(format!("{STATUS_FILE_NAME}.{}.tmp", status.pid));
    let mut buf = Vec::new();
    write_status_to(&mut buf, status)?;
    std::fs::write(&tmp, buf)?;
    std::fs::rename(&tmp, &path)
}

/// The status document a panic hook writes: `stage: runtime`, `state:
/// error`, and a reason of the form `panic at <location>: <message>`.
///
/// `location` is `file:line:col` (or `None` when the panic carries no
/// location); `message` is the panic payload text.
#[must_use]
pub fn panic_status(
    pid: u32,
    log_path: Option<PathBuf>,
    location: Option<&str>,
    message: &str,
) -> StartupStatus {
    StartupStatus {
        pid,
        stage: StartupStage::Runtime,
        state: StartupState::Error,
        reason: panic_reason(location, message),
        log_path,
    }
}

/// `panic at <location>: <message>` (or `panic: <message>` without a
/// location) — the reason string shared by the log line and the status file.
#[must_use]
pub fn panic_reason(location: Option<&str>, message: &str) -> String {
    match location {
        Some(loc) => format!("panic at {loc}: {message}"),
        None => format!("panic: {message}"),
    }
}

/// Read and parse `<config_dir>/flowd.status`.
///
/// Returns `None` when the file is missing, unreadable, or not valid JSON
/// (a reader mid-rename or a file from an incompatible version).
#[must_use]
pub fn read_status(config_dir: &Path) -> Option<StartupStatus> {
    let text = std::fs::read_to_string(status_file_path_in(config_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Remove the status file, ignoring a missing file.
pub fn remove_status(config_dir: &Path) {
    let _ = std::fs::remove_file(status_file_path_in(config_dir));
}

/// Interpret a `FLOW_FAIL_AT` value: `Some(stage)` for a known stage name,
/// `None` (with a warning) for anything else, `None` silently when unset.
#[must_use]
pub(crate) fn fail_at_from_value(value: Option<&str>) -> Option<StartupStage> {
    let raw = value?;
    if raw.trim().is_empty() {
        return None;
    }
    match StartupStage::parse(raw) {
        Some(stage) => {
            log::warn!("startup: {FAIL_AT_ENV}={raw}: will inject a failure at stage '{stage}'");
            Some(stage)
        }
        None => {
            log::warn!("startup: ignoring unknown {FAIL_AT_ENV} value {raw:?} (not a stage name)");
            None
        }
    }
}

/// Read [`FAIL_AT_ENV`] from the process environment (see
/// `fail_at_from_value`).
#[must_use]
pub fn fail_at_from_env() -> Option<StartupStage> {
    fail_at_from_value(std::env::var(FAIL_AT_ENV).ok().as_deref())
}

/// Progress reporter threaded through `main` and `FlowWM::new`.
///
/// Call [`begin`](Self::begin) before running a stage (it is where fault
/// injection takes effect), [`ok`](Self::ok) after it succeeds, and
/// [`fail`](Self::fail) once on the way out when a stage has failed.
pub struct StartupReporter {
    config_dir: PathBuf,
    log_path: Option<PathBuf>,
    pid: u32,
    fail_at: Option<StartupStage>,
}

impl StartupReporter {
    /// Create a reporter writing status under `config_dir`.
    ///
    /// `log_path` is echoed into every status document; `fail_at` is the
    /// stage to fail deliberately (see [`fail_at_from_env`]).
    #[must_use]
    pub fn new(
        config_dir: PathBuf,
        log_path: Option<PathBuf>,
        fail_at: Option<StartupStage>,
    ) -> Self {
        Self {
            config_dir,
            log_path,
            pid: std::process::id(),
            fail_at,
        }
    }

    /// Resolved log file path, if logging goes to a file.
    #[must_use]
    pub fn log_path(&self) -> Option<&Path> {
        self.log_path.as_deref()
    }

    /// The stage configured for fault injection, if any.
    #[must_use]
    pub fn fail_at(&self) -> Option<StartupStage> {
        self.fail_at
    }

    /// Record a post-boot failure: log `runtime: FAILED: <reason>` and
    /// rewrite the status file with `stage: runtime`, `state: error`.
    pub fn runtime_failure(&self, reason: &str) {
        log::error!("runtime: FAILED: {reason}");
        self.write(
            StartupStage::Runtime,
            StartupState::Error,
            reason.to_owned(),
        );
    }

    /// Record a clean stop: rewrite the status file with `stage: runtime`,
    /// `state: stopped` so a later reader does not mistake a stopped daemon
    /// for a ready one.
    pub fn stopped(&self) {
        self.write(StartupStage::Runtime, StartupState::Stopped, String::new());
    }

    /// Check for an injected failure before running `stage`.
    ///
    /// # Errors
    ///
    /// Returns a [`StartupError`] with reason `injected failure (FLOW_FAIL_AT)`
    /// when `stage` is the configured fault-injection stage.
    pub fn begin(&self, stage: StartupStage) -> Result<(), StartupError> {
        if self.fail_at == Some(stage) {
            return Err(StartupError::new(
                stage,
                format!("injected failure ({FAIL_AT_ENV})"),
            ));
        }
        log::debug!("startup: entering stage '{stage}'");
        Ok(())
    }

    /// Record that `stage` completed: log `startup: [n/N] <stage> ok` and
    /// rewrite the status file (`ready` once the final stage completes).
    pub fn ok(&self, stage: StartupStage) {
        log::info!(
            "startup: [{}/{}] {stage} ok",
            stage.index() + 1,
            StartupStage::ALL.len()
        );
        let state = if stage == StartupStage::Ready {
            StartupState::Ready
        } else {
            StartupState::Starting
        };
        self.write(stage, state, String::new());
    }

    /// Record a failure: log `startup: FAILED at <stage>: <reason>` and
    /// rewrite the status file with `state: error`.
    pub fn fail(&self, err: &StartupError) {
        log::error!("startup: FAILED at {}: {}", err.stage, err.reason);
        self.write(err.stage, StartupState::Error, err.reason.clone());
    }

    /// Write the status document, retrying up to [`STATUS_WRITE_ATTEMPTS`]
    /// times [`STATUS_WRITE_RETRY_DELAY`] apart.
    ///
    /// `write_status` renames a temp file over `flowd.status`; while `flow
    /// start` (or an editor) has the file open the rename can fail with a
    /// sharing violation for a few milliseconds. A missed write would leave
    /// a stale `starting`/`ready` behind, which `flow stop` would then wait
    /// on, so it is worth a couple of retries.
    fn write(&self, stage: StartupStage, state: StartupState, reason: String) {
        let status = StartupStatus {
            pid: self.pid,
            stage,
            state,
            reason,
            log_path: self.log_path.clone(),
        };
        let mut last_err = None;
        for attempt in 1..=STATUS_WRITE_ATTEMPTS {
            match write_status(&self.config_dir, &status) {
                Ok(()) => return,
                Err(e) => {
                    last_err = Some(e);
                    if attempt < STATUS_WRITE_ATTEMPTS {
                        std::thread::sleep(STATUS_WRITE_RETRY_DELAY);
                    }
                }
            }
        }
        log::warn!(
            "startup: could not write {} after {STATUS_WRITE_ATTEMPTS} attempts: {}",
            status_file_path_in(&self.config_dir).display(),
            last_err.map_or_else(String::new, |e| e.to_string())
        );
    }
}

/// How many times [`StartupReporter`] tries to write the status file.
const STATUS_WRITE_ATTEMPTS: u32 = 3;

/// Pause between two attempts to write the status file.
const STATUS_WRITE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_display_is_snake_case() {
        assert_eq!(StartupStage::InitialLayout.to_string(), "initial_layout");
        assert_eq!(StartupStage::LoadoutRestore.to_string(), "loadout_restore");
        assert_eq!(StartupStage::Hooks.to_string(), "hooks");
        for stage in StartupStage::ALL {
            let name = stage.name();
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is not snake_case"
            );
            assert_eq!(StartupStage::parse(name), Some(stage));
        }
    }

    #[test]
    fn runtime_pseudo_stage_is_outside_the_boot_sequence() {
        assert!(!StartupStage::ALL.contains(&StartupStage::Runtime));
        assert_eq!(StartupStage::Runtime.index(), StartupStage::ALL.len());
        assert_eq!(StartupStage::Runtime.exit_code(), EXIT_CODE_RUNTIME);
        assert_eq!(StartupStage::Runtime.name(), "runtime");
        assert_eq!(StartupStage::parse("runtime"), Some(StartupStage::Runtime));
        assert_eq!(
            fail_at_from_value(Some("runtime")),
            Some(StartupStage::Runtime)
        );
        // Runtime's exit code never collides with a boot stage's.
        assert!(
            StartupStage::ALL
                .iter()
                .all(|s| s.exit_code() != EXIT_CODE_RUNTIME)
        );
    }

    #[test]
    fn panic_status_writes_runtime_error_document() {
        let status = panic_status(
            777,
            Some(PathBuf::from("C:\\logs\\flowd.log")),
            Some("src\\daemon\\run.rs:123:9"),
            "index out of bounds",
        );
        assert_eq!(status.stage, StartupStage::Runtime);
        assert_eq!(status.state, StartupState::Error);
        assert_eq!(
            status.reason,
            "panic at src\\daemon\\run.rs:123:9: index out of bounds"
        );

        let mut buf = Vec::new();
        write_status_to(&mut buf, &status).expect("write");
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("\"pid\": 777"), "{text}");
        assert!(text.contains("\"stage\": \"runtime\""), "{text}");
        assert!(text.contains("\"state\": \"error\""), "{text}");
        assert!(text.contains("index out of bounds"), "{text}");
        // The document round-trips through the reader's parser.
        let parsed: StartupStatus = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, status);

        assert_eq!(panic_reason(None, "boom"), "panic: boom");
    }

    #[test]
    fn reporter_records_runtime_failure_and_clean_stop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reporter = StartupReporter::new(dir.path().to_path_buf(), None, None);
        reporter.ok(StartupStage::Ready);
        assert_eq!(read_status(dir.path()).unwrap().state, StartupState::Ready);

        reporter.runtime_failure("animator thread died");
        let s = read_status(dir.path()).unwrap();
        assert_eq!(s.stage, StartupStage::Runtime);
        assert_eq!(s.state, StartupState::Error);
        assert_eq!(s.reason, "animator thread died");

        reporter.stopped();
        let s = read_status(dir.path()).unwrap();
        assert_eq!(s.stage, StartupStage::Runtime);
        assert_eq!(s.state, StartupState::Stopped);
        assert!(s.reason.is_empty());
        let text = std::fs::read_to_string(status_file_path_in(dir.path())).unwrap();
        assert!(text.contains("\"state\": \"stopped\""), "{text}");
    }

    #[test]
    fn stage_exit_codes_are_base_plus_index() {
        assert_eq!(StartupStage::Logging.exit_code(), 10);
        assert_eq!(StartupStage::Config.exit_code(), 11);
        assert_eq!(StartupStage::Pipe.exit_code(), 14);
        assert_eq!(StartupStage::Hooks.exit_code(), 15);
        assert_eq!(StartupStage::Ready.exit_code(), 20);
        for (i, stage) in StartupStage::ALL.iter().enumerate() {
            assert_eq!(stage.index(), i);
            assert_eq!(stage.exit_code(), EXIT_CODE_BASE + i as i32);
        }
        // Exit codes are unique and never collide with the generic 1.
        let mut codes: Vec<i32> = StartupStage::ALL.iter().map(|s| s.exit_code()).collect();
        codes.dedup();
        assert_eq!(codes.len(), StartupStage::ALL.len());
        assert!(codes.iter().all(|c| *c > 1));
    }

    #[test]
    fn stage_order_puts_pipe_and_hooks_before_initial_layout() {
        assert!(StartupStage::Pipe.index() < StartupStage::InitialLayout.index());
        assert!(StartupStage::Hooks.index() < StartupStage::InitialLayout.index());
        assert!(StartupStage::Scan.index() < StartupStage::Pipe.index());
        assert_eq!(StartupStage::ALL.last(), Some(&StartupStage::Ready));
    }

    #[test]
    fn startup_error_display_names_step_and_reason() {
        let err = StartupError::new(StartupStage::Hooks, "missing hooks: FOREGROUND");
        assert_eq!(
            err.to_string(),
            "failed at step 'hooks': missing hooks: FOREGROUND"
        );
        assert_eq!(err.exit_code(), 15);
        let mapped: Result<(), StartupError> = Err::<(), _>("boom").at(StartupStage::Pipe);
        assert_eq!(
            mapped.unwrap_err(),
            StartupError::new(StartupStage::Pipe, "boom")
        );
    }

    #[test]
    fn status_file_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_status(dir.path()).is_none(), "no file yet");

        let status = StartupStatus {
            pid: 4242,
            stage: StartupStage::Hooks,
            state: StartupState::Error,
            reason: "injected failure (FLOW_FAIL_AT)".to_owned(),
            log_path: Some(dir.path().join("logs").join("flowd-2026-10-19.log")),
        };
        write_status(dir.path(), &status).expect("write");
        assert!(status_file_path_in(dir.path()).exists());
        assert_eq!(read_status(dir.path()), Some(status.clone()));

        // JSON uses snake_case tags that the CLI and docs rely on.
        let text = std::fs::read_to_string(status_file_path_in(dir.path())).unwrap();
        assert!(text.contains("\"stage\": \"hooks\""), "{text}");
        assert!(text.contains("\"state\": \"error\""), "{text}");

        // Overwrite with a ready status and confirm the replacement is seen.
        let ready = StartupStatus {
            pid: 4242,
            stage: StartupStage::Ready,
            state: StartupState::Ready,
            reason: String::new(),
            log_path: None,
        };
        write_status(dir.path(), &ready).expect("rewrite");
        assert_eq!(read_status(dir.path()), Some(ready));

        remove_status(dir.path());
        assert!(read_status(dir.path()).is_none());
        // Removing again is a silent no-op.
        remove_status(dir.path());
    }

    #[test]
    fn read_status_rejects_garbage() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(status_file_path_in(dir.path()), "{not json").unwrap();
        assert!(read_status(dir.path()).is_none());
    }

    #[test]
    fn reporter_writes_status_after_each_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("flowd.log");
        let reporter = StartupReporter::new(dir.path().to_path_buf(), Some(log.clone()), None);

        reporter.begin(StartupStage::Pipe).expect("no injection");
        reporter.ok(StartupStage::Pipe);
        let s = read_status(dir.path()).expect("status written");
        assert_eq!(s.pid, std::process::id());
        assert_eq!(s.stage, StartupStage::Pipe);
        assert_eq!(s.state, StartupState::Starting);
        assert_eq!(s.log_path.as_deref(), Some(log.as_path()));

        reporter.ok(StartupStage::Ready);
        assert_eq!(read_status(dir.path()).unwrap().state, StartupState::Ready);

        reporter.fail(&StartupError::new(StartupStage::Hooks, "nope"));
        let s = read_status(dir.path()).unwrap();
        assert_eq!(s.state, StartupState::Error);
        assert_eq!(s.stage, StartupStage::Hooks);
        assert_eq!(s.reason, "nope");
    }

    #[test]
    fn fail_at_value_maps_to_stage_and_unknown_is_ignored() {
        assert_eq!(fail_at_from_value(None), None);
        assert_eq!(fail_at_from_value(Some("")), None);
        assert_eq!(fail_at_from_value(Some("   ")), None);
        assert_eq!(
            fail_at_from_value(Some("borders")),
            Some(StartupStage::Borders)
        );
        assert_eq!(fail_at_from_value(Some("hooks")), Some(StartupStage::Hooks));
        assert_eq!(
            fail_at_from_value(Some("loadout_restore")),
            Some(StartupStage::LoadoutRestore)
        );
        assert_eq!(
            fail_at_from_value(Some(" Initial_Layout ")),
            Some(StartupStage::InitialLayout)
        );
        // Unknown names are ignored (a warning is logged) rather than
        // failing the boot or guessing a stage.
        assert_eq!(fail_at_from_value(Some("bogus")), None);
        assert_eq!(fail_at_from_value(Some("initial-layout")), None);
    }

    #[test]
    fn reporter_injects_failure_only_at_configured_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let reporter =
            StartupReporter::new(dir.path().to_path_buf(), None, Some(StartupStage::Borders));
        assert!(reporter.begin(StartupStage::Hooks).is_ok());
        assert!(reporter.begin(StartupStage::InitialLayout).is_ok());
        let err = reporter.begin(StartupStage::Borders).unwrap_err();
        assert_eq!(err.stage, StartupStage::Borders);
        assert_eq!(err.reason, "injected failure (FLOW_FAIL_AT)");
        assert_eq!(
            err.to_string(),
            "failed at step 'borders': injected failure (FLOW_FAIL_AT)"
        );
    }
}
