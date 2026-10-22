//! Daemon logging initialization — date-stamped log file plus stderr tee.
//!
//! ## Why this module exists
//!
//! The daemon already uses the `log` facade everywhere (100+ call sites), and
//! `env_logger::init()` is called at startup. **The problem is that
//! `env_logger` writes only to stderr by default**, and the daemon is spawned
//! by `flow start` with `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` — a fully
//! detached process whose stderr goes nowhere. So every log line the code
//! already emits silently vanishes, which is why debugging feels blind.
//!
//! This module replaces bare `env_logger::init()` with a configured logger
//! that writes to **two** targets simultaneously:
//!
//! 1. **A date-stamped file** — `<config_dir>/logs/flowd-YYYY-MM-DD.log`,
//!    opened in append mode. One file per day; multiple daemon restarts on
//!    the same day accumulate into one file. Files older than
//!    `[logging] keep_days` (by the date in their name) are deleted at
//!    daemon start by [`prune_old_logs`]; `keep_days = 0` keeps them all.
//! 2. **stderr** — so that running `flowd` directly from a console (rather
//!    than via `flow start`) still shows logs inline.
//!
//! The date-stamped file (target 1) can be overridden by passing
//! `flowd --log-file PATH`, which redirects all logging to that exact path
//! (truncated on each start) for a clean, isolated single-run debug log.
//! See [`init`].
//!
//! ## Why `chrono` instead of the standard library
//!
//! [`std::time::SystemTime::now`] returns UTC, and the standard library has no
//! way to determine the local timezone offset on Windows (no `TZ` semantics).
//! Log filenames should reflect the user's *local* calendar day so that
//! "today's log file" matches their intuition. [`chrono::Local::now`] handles
//! local time correctly across platforms, so we use it for the single date
//! computation at startup.
//!
//! ## Level policy
//!
//! - **Debug builds**: default level is `debug`.
//! - **Release builds**: default level is `info`.
//! - **Override**: set the `RUST_LOG` environment variable (e.g.
//!   `RUST_LOG=trace`, `RUST_LOG=flow_wm=debug`) before
//!   starting the daemon. This takes precedence over the build-profile
//!   default.
//! - **CLI override**: `flowd --verbose` (debug) or `flowd --log-level <lvl>`
//!   — forwarded by `flow start --verbose` / `--log-level` — takes
//!   precedence over both `RUST_LOG` and the build default.
//!
//! The log directory honours the config directory resolved from `--config`
//! / `FLOW_CONFIG_DIR` (the caller passes it in), and [`init`] returns the
//! resolved log path so the daemon can publish it in its status file.
//!
//! ## Date rollover
//!
//! The date is computed **once at daemon startup** and the file handle is held
//! for the daemon's entire lifetime. If the daemon runs across midnight, it
//! keeps writing to the startup-day's file until restarted. This is a
//! deliberate trade-off: it avoids the complexity of a background rotation
//! thread while keeping the common case (daily daemon restarts) correct.
//!
//! # Errors during initialization
//!
//! If the log file cannot be opened (e.g. the config directory is read-only),
//! initialization falls back to a **stderr-only** logger and emits a warning.
//! The daemon must never fail to start solely because logging is unavailable.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use chrono::{Local, NaiveDate};
use env_logger::{Builder, Target};

/// Initialize the global logger.
///
/// Configures [`env_logger`] to write every record to both a log file and
/// stderr (via [`TeeWriter`]).
///
/// This function must be called **exactly once** per process — it calls
/// [`Builder::init`], which panics if a global logger is already installed.
/// In the daemon this is the first thing `main()` does.
///
/// # Log file selection
///
/// - **`log_file_override = Some(path)`** (from `flowd --log-file PATH`):
///   redirect ALL logging to that exact path, opened fresh
///   (create-or-truncate). This yields a clean, isolated file for a single
///   debugging run, separate from the shared daily log. Stderr is still
///   echoed so a console-launched `flowd --log-file` stays visible.
/// - **`log_file_override = None`** (default): the date-stamped daily log
///   (`<config_dir>/logs/flowd-YYYY-MM-DD.log`, see [`default_log_path`])
///   opened in append mode, so restarts on the same day accumulate into one
///   file. `config_dir` is the directory the daemon resolved from
///   `--config` / `FLOW_CONFIG_DIR`.
///
/// # Level resolution
///
/// 1. If `level_override` is `Some` (from `--verbose` → `"debug"`, or
///    `--log-level <lvl>`), it is parsed as an `env_logger` filter string
///    and wins outright.
/// 2. Else if `RUST_LOG` is set, its directives are used (standard
///    `env_logger` behavior — module-specific filters, etc.).
/// 3. Otherwise, the build-profile default applies: `debug` in debug builds,
///    `info` in release builds (see [`default_level`]).
///
/// # Return value
///
/// The resolved log file path when the file was opened, or `None` when
/// logging fell back to stderr only.
///
/// # File-open failure
///
/// If the log file cannot be created or opened, the logger is initialized
/// with a stderr-only target and a warning is emitted. This ensures the
/// daemon can still run and produce diagnostic output even when the log
/// directory is inaccessible.
pub fn init(
    config_dir: &Path,
    log_file_override: Option<&Path>,
    level_override: Option<&str>,
) -> Option<PathBuf> {
    let env = env_logger::Env::default().default_filter_or(default_level());

    // Resolve the log file path and open mode from the override (if any).
    let (log_path, append) = match log_file_override {
        Some(path) => {
            // Explicit `--log-file`: a clean, isolated file for this run.
            (PathBuf::from(path), false)
        }
        None => {
            // Default: date-stamped daily log, appended to across restarts.
            (default_log_path(config_dir), true)
        }
    };

    let configure = |builder: &mut Builder| {
        if let Some(level) = level_override {
            // `--verbose` / `--log-level` beat RUST_LOG and the build default.
            builder.parse_filters(level);
        }
    };

    match open_log_file(&log_path, append) {
        Ok(file) => {
            // Primary path: tee to both file and stderr.
            let mut builder = Builder::from_env(env);
            configure(&mut builder);
            builder.target(Target::Pipe(Box::new(TeeWriter::new(file))));
            builder.init();
            log::info!(
                "flowd: logging to {} (level: {})",
                log_path.display(),
                level_override.unwrap_or("RUST_LOG or build default")
            );
            Some(log_path)
        }
        Err(e) => {
            // Fallback: stderr-only. Initialize the logger BEFORE emitting the
            // warning so the warning is actually captured by the logger.
            let mut builder = Builder::from_env(env);
            configure(&mut builder);
            builder.init();
            log::warn!(
                "flowd: could not open log file {}: {e}; falling back to stderr-only",
                log_path.display()
            );
            None
        }
    }
}

/// The build-profile default log level: `debug` in debug builds, `info` in
/// release builds. Overridden by `RUST_LOG`, `--verbose` or `--log-level`.
#[must_use]
pub fn default_level() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "info"
    }
}

/// The date-stamped daily log path the daemon uses when no `--log-file`
/// override is given: `<config_dir>/logs/flowd-YYYY-MM-DD.log` for today's
/// local date. The `flow` client uses this to tell the user where to look
/// when the daemon fails to start.
#[must_use]
pub fn default_log_path(config_dir: &Path) -> PathBuf {
    crate::config::dirs::log_file_path_in(config_dir, &today_local_date())
}

/// Delete the daily log files under `<config_dir>/logs` that are older
/// than `keep_days` days — judged by the date in the file name
/// (`flowd-YYYY-MM-DD.log`), which is what the daemon wrote, not the
/// modification time. `keep_days == 0` deletes nothing. Files with any other
/// name (a `--log-file` target, a user's own notes) are never touched.
/// Returns how many files were removed; a missing `logs/` directory or an
/// unreadable entry is not an error.
pub fn prune_old_logs(config_dir: &Path, keep_days: u32) -> usize {
    if keep_days == 0 {
        return 0;
    }
    let today = Local::now().date_naive();
    let dir = crate::config::dirs::logs_dir_in(config_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_stale_log_name(name, today, keep_days) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {
                removed += 1;
                log::info!("pruned log file {} (older than {keep_days} days)", name);
            }
            Err(e) => log::warn!("could not prune log file {}: {e}", entry.path().display()),
        }
    }
    removed
}

/// `true` when `name` is a daily log file (`flowd-YYYY-MM-DD.log`) whose
/// date is more than `keep_days` days before `today`. Pure.
#[must_use]
pub fn is_stale_log_name(name: &str, today: NaiveDate, keep_days: u32) -> bool {
    let Some(date) = name
        .strip_prefix("flowd-")
        .and_then(|rest| rest.strip_suffix(".log"))
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
    else {
        return false;
    };
    let age_days = (today - date).num_days();
    age_days > i64::from(keep_days)
}

/// Return today's local date as a `YYYY-MM-DD` string.
///
/// Uses [`chrono::Local`] to get the wall-clock date in the user's timezone.
/// This is computed once at daemon startup and used to build the log filename.
///
/// # Examples
///
/// The format is always zero-padded:
///
/// ```ignore
/// // Returns e.g. "2026-06-17" on June 17, 2026.
/// let date = today_local_date();
/// assert_eq!(date.len(), 10); // YYYY-MM-DD
/// ```
fn today_local_date() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

/// Open (or create) the log file, creating parent dirs.
///
/// The `append` flag selects the open mode:
///
/// - `append = true` — opened with `.append(true).create(true)`. If the file
///   exists, new records are appended to the end; if not, it is created.
///   Existing contents are never truncated. This is used for the default
///   date-stamped daily log, where multiple daemon restarts on the same day
///   accumulate into one file.
/// - `append = false` — opened with `.write(true).create(true).truncate(true)`.
///   If the file exists it is **truncated to zero length**; if not, it is
///   created. This yields a fresh, isolated file for a single debugging run
///   via `flowd --log-file PATH`.
///
/// The parent directory tree is created if missing via
/// [`std::fs::create_dir_all`].
///
/// # Errors
///
/// Returns [`io::Error`] if directory creation or file opening fails.
fn open_log_file(path: &Path, append: bool) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).write(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    opts.open(path)
}

/// A writer that duplicates every write to both a file and stderr.
///
/// This is the bridge between [`env_logger`] (which writes via the [`Write`]
/// trait) and our two-target output strategy. Each [`write`] call sends the
/// bytes to the log file first (the primary, durable target), then to stderr
/// (best-effort — errors are silently ignored because a failing stderr must
/// never prevent the file write from succeeding).
///
/// The file is always written first so that even if stderr is broken, the
/// log record is preserved on disk. Stderr failures are swallowed because
/// stderr is a debugging convenience, not a correctness requirement.
///
/// `TeeWriter` is `Send` (because [`File`] is `Send`), satisfying
/// [`env_logger::Target::Pipe`]'s `Box<dyn Write + Send + 'static>` bound.
struct TeeWriter {
    /// The append-mode log file. Writes go here first.
    file: File,
}

impl TeeWriter {
    /// Wrap a [`File`] so that writes are also echoed to stderr.
    fn new(file: File) -> Self {
        Self { file }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Primary: write to the durable log file.
        self.file.write_all(buf)?;
        // Secondary: echo to stderr. Errors here are non-fatal — a broken
        // stderr pipe must not prevent the file write (already done above)
        // from being reported as successful to env_logger.
        let _ = io::stderr().write_all(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let _ = io::stderr().flush();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Positive: `today_local_date` returns a `YYYY-MM-DD` formatted string.
    ///
    /// We validate the format structurally (not the specific date, which
    /// depends on when the test runs) by checking the length and that each
    /// component parses as digits.
    #[test]
    fn today_local_date_is_iso_format() {
        let date = today_local_date();
        let parts: Vec<&str> = date.split('-').collect();
        assert_eq!(parts.len(), 3, "expected YYYY-MM-DD, got: {date}");
        assert_eq!(parts[0].len(), 4, "year should be 4 digits: {date}");
        assert_eq!(parts[1].len(), 2, "month should be 2 digits: {date}");
        assert_eq!(parts[2].len(), 2, "day should be 2 digits: {date}");

        // Year should be a plausible recent year (guards against garbage).
        let year: u32 = parts[0]
            .parse()
            .unwrap_or_else(|_| panic!("year should be numeric: {date}"));
        assert!(
            (2024..=2100).contains(&year),
            "year should be plausible: {date}"
        );
    }

    #[test]
    fn stale_log_name_is_judged_by_the_date_in_the_name() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 22).unwrap();
        assert!(is_stale_log_name("flowd-2026-09-22.log", today, 14)); // 30 days
        assert!(is_stale_log_name("flowd-2026-10-07.log", today, 14)); // 15 days
        assert!(!is_stale_log_name("flowd-2026-10-08.log", today, 14)); // 14 days: kept
        assert!(!is_stale_log_name("flowd-2026-10-22.log", today, 14)); // today
        assert!(!is_stale_log_name("flowd-2026-11-01.log", today, 14)); // future
        // keep_days = 0 would mean "older than 0 days" — the caller short-circuits
        // it, but even here today's file is never stale.
        assert!(!is_stale_log_name("flowd-2026-10-22.log", today, 0));
        // Anything that is not a daily log is left alone.
        assert!(!is_stale_log_name("flowd-2026-09-22.log.bak", today, 14));
        assert!(!is_stale_log_name("debug-2026-09-22.log", today, 14));
        assert!(!is_stale_log_name("flowd-yesterday.log", today, 14));
        assert!(!is_stale_log_name("stage16.log", today, 14));
    }

    #[test]
    fn prune_old_logs_removes_only_stale_daily_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let logs = temp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let today = Local::now().date_naive();
        let name = |days_ago: i64| {
            format!(
                "flowd-{}.log",
                (today - chrono::Duration::days(days_ago)).format("%Y-%m-%d")
            )
        };
        for f in [
            name(30),
            name(15),
            name(14),
            name(0),
            "notes.log".to_owned(),
        ] {
            std::fs::write(logs.join(f), "x").unwrap();
        }
        assert_eq!(prune_old_logs(temp.path(), 14), 2);
        assert!(!logs.join(name(30)).exists());
        assert!(!logs.join(name(15)).exists());
        assert!(logs.join(name(14)).exists());
        assert!(logs.join(name(0)).exists());
        assert!(logs.join("notes.log").exists());
        // keep_days = 0: nothing is ever removed.
        std::fs::write(logs.join(name(400)), "x").unwrap();
        assert_eq!(prune_old_logs(temp.path(), 0), 0);
        assert!(logs.join(name(400)).exists());
        // No logs dir at all is fine.
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(prune_old_logs(empty.path(), 14), 0);
    }

    /// Positive: `TeeWriter` writes the full payload to the file.
    ///
    /// We cannot easily verify the stderr side of the tee (it would require
    /// capturing process stderr), but we can confirm the file receives every
    /// byte, which is the primary durable target.
    #[test]
    fn tee_writer_writes_payload_to_file() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let path = temp.path().join("tee-test.log");
        let file = File::create(&path).expect("file creation failed");

        let mut tee = TeeWriter::new(file);
        let payload = b"hello log line\nsecond line\n";
        tee.write_all(payload).expect("write_all failed");
        tee.flush().expect("flush failed");
        drop(tee); // close the file handle

        let contents = std::fs::read_to_string(&path).expect("read back failed");
        assert_eq!(contents.as_bytes(), payload);
    }

    /// Positive: `TeeWriter` reports the correct byte count.
    ///
    /// The [`Write::write`] contract requires returning the number of bytes
    /// written. Our impl writes all bytes via `write_all` and reports the full
    /// length, so callers (env_logger) see an accurate count.
    #[test]
    fn tee_writer_reports_byte_count() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let path = temp.path().join("tee-count.log");
        let file = File::create(&path).expect("file creation failed");

        let mut tee = TeeWriter::new(file);
        let payload = b"exactly 13 bytes"; // 15 bytes
        let written = tee.write(payload).expect("write failed");
        assert_eq!(written, payload.len(), "should report all bytes written");
    }

    /// Positive: `open_log_file` creates the parent directory tree.
    ///
    /// Passing a path with non-existent parent directories should succeed:
    /// the function calls `create_dir_all` before opening the file.
    #[test]
    fn open_log_file_creates_parent_dirs() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let nested = temp.path().join("a").join("b").join("logs");
        let file_path = nested.join("flowd-test.log");

        // The returned handle is intentionally unused: the test only verifies
        // that `open_log_file` creates the file and its parent directories.
        let _file = open_log_file(&file_path, true).expect("open_log_file failed");
        assert!(
            file_path.exists(),
            "log file should exist after open_log_file"
        );
        assert!(
            nested.exists(),
            "parent directory tree should have been created"
        );
    }

    /// Positive: `open_log_file` appends to an existing file rather than
    /// truncating it.
    ///
    /// This is the core invariant of the date-based append strategy: a second
    /// open of the same path preserves prior contents and adds to them.
    #[test]
    fn open_log_file_appends_without_truncating() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let path = temp.path().join("append-test.log");

        // First open + write.
        {
            let mut f = open_log_file(&path, true).expect("first open failed");
            f.write_all(b"first line\n").expect("first write failed");
            f.flush().expect("first flush failed");
        }

        // Second open + write — must NOT destroy "first line".
        {
            let mut f = open_log_file(&path, true).expect("second open failed");
            f.write_all(b"second line\n").expect("second write failed");
            f.flush().expect("second flush failed");
        }

        let contents = std::fs::read_to_string(&path).expect("read back failed");
        assert_eq!(contents, "first line\nsecond line\n");
    }

    /// Positive: `open_log_file` with `append = false` truncates an existing
    /// file to a clean slate.
    ///
    /// This is the `--log-file` override behaviour: each launch of
    /// `flowd --log-file PATH` must start from an empty file so the captured
    /// log contains only that single run's output.
    #[test]
    fn open_log_file_truncates_when_override() {
        let temp = tempfile::tempdir().expect("tempdir creation failed");
        let path = temp.path().join("override-test.log");

        // Pre-create the file with stale content from a previous run.
        std::fs::write(&path, "leftover from a previous debug run\n").expect("pre-write failed");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back failed"),
            "leftover from a previous debug run\n"
        );

        // Open in override (truncate) mode — existing content must be wiped.
        let _file = open_log_file(&path, false).expect("open in override mode failed");

        let contents = std::fs::read_to_string(&path).expect("read back failed");
        assert!(
            contents.is_empty(),
            "override mode should truncate the file to empty, got: {contents}"
        );
    }
}
