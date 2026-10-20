//! Daemon autostart and AutoHotkey lifecycle.
//!
//! Two related concerns live here:
//!
//! - **AHK lifecycle** — the **daemon** launches the user's `flow.ahk`
//!   keybinding script (`flowd --ahk`, forwarded by `flow start --ahk`) once
//!   it is ready, inside a kill-on-close `JobObject` so the kernel
//!   terminates the script whenever the daemon dies — however it dies. The
//!   AutoHotkey v2 interpreter is resolved directly
//!   ([`resolve_ahk_interpreter`]) and started suspended so it is a job
//!   member before its first instruction; only if no interpreter can be
//!   found does the launch fall back to the `.ahk` shell association (with
//!   a best-effort job assignment after the fact). A pidfile records the
//!   launched PID so `flow stop` (and the daemon's own fail-back) can kill
//!   exactly that process.
//! - **Login autostart** — `flow enable-autostart [--ahk]` writes a single
//!   `flow.lnk` into the user's `shell:startup` folder whose target is
//!   `flow.exe start [--ahk]`. Login therefore reuses the interactive startup
//!   path's pre-flight config check and double-start guard. `flow
//!   disable-autostart` removes the shortcut. Both are idempotent.
//!
//! See (`docs/src/dev-guide/ipc-and-watchdog.md`) for the command reference.

use std::ffi::OsStr;
use std::fs;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IPersistFile,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Threading::{
    GetProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    IShellLinkW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{Interface, PCWSTR, PWSTR};

use crate::config::dirs::config_dir;
use crate::job::{JobChild, JobObject};

/// Filename of the autostart shortcut inside the `shell:startup` folder.
const FLOW_LNK: &str = "flow.lnk";

/// Filename of the pidfile recording the launched AutoHotkey PID.
pub const AHK_PIDFILE: &str = "flow.ahk.pid";

/// Resolve the user's `shell:startup` folder.
///
/// This is `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup`. Windows
/// launches every `.lnk` (or executable) in this folder at logon.
///
/// # Errors
///
/// Returns an error string if `%APPDATA%` is not set (broken environment).
pub fn startup_dir() -> Result<PathBuf, String> {
    let appdata = std::env::var("APPDATA")
        .map_err(|_| "%APPDATA% is not set; cannot resolve shell:startup".to_string())?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup"))
}

/// Report returned by [`enable_autostart`] / [`disable_autostart`].
#[derive(Debug, Clone)]
pub struct AutostartReport {
    /// Path to the single `flow.lnk` shortcut that was created or removed.
    pub shortcut: PathBuf,
}

/// Create (or overwrite) the login autostart shortcut.
///
/// Writes a single `flow.lnk` into [`startup_dir`] whose target is `flow.exe`
/// with arguments `start` (or `start --ahk` when `ahk` is true), working
/// directory set to the executable's folder, and show-cmd `SW_HIDE` so the
/// short-lived `flow.exe` console never flashes at login. Because the shortcut
/// runs the real CLI, login reuses the interactive path's pre-flight config
/// check and double-start guard — there is no parallel startup code path.
///
/// When `ahk` is true, the user's `flow.ahk` must already exist (write it with
/// `flow config init --ahk`); this is verified up-front so a missing script is
/// reported clearly rather than only failing at login.
///
/// # Errors
///
/// Returns an error string if:
/// - `ahk` is true and `flow.ahk` does not exist in the config directory.
/// - `flow.exe`'s path cannot be determined.
/// - The shortcut cannot be created (COM failure).
pub fn enable_autostart(ahk: bool) -> Result<AutostartReport, String> {
    if ahk {
        let script = flow_ahk_path();
        if !script.exists() {
            return Err(format!(
                "flow.ahk not found at {}; run `flow config init --ahk` to create it first",
                script.display()
            ));
        }
    }

    let exe = flow_exe_path()?;
    let working_dir = exe.parent().map(Path::to_path_buf);
    let args = if ahk { "start --ahk" } else { "start" };

    let link = shortcut_path()?;
    create_shortcut(&link, &exe, Some(args), working_dir.as_deref())?;
    Ok(AutostartReport { shortcut: link })
}

/// Remove the login autostart shortcut.
///
/// Idempotent: returns `Ok` if the shortcut does not exist (treats missing as
/// success), so calling this twice is never an error.
///
/// # Errors
///
/// Returns an error string only on filesystem errors other than `NotFound`.
pub fn disable_autostart() -> Result<AutostartReport, String> {
    let link = shortcut_path()?;
    match fs::remove_file(&link) {
        Ok(()) => Ok(AutostartReport { shortcut: link }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(AutostartReport { shortcut: link })
        }
        Err(e) => Err(format!("failed to remove {}: {e}", link.display())),
    }
}

// ── AHK: daemon-owned launch inside a job object ──────────────────────────

/// The running hotkey script, as launched by [`launch_ahk_in_job`].
///
/// Owns the `JobObject` whose closing kills the script. The daemon keeps
/// this inside `FlowWM` for its whole life and never closes it explicitly;
/// process teardown (clean or not) closes the handle and the kernel does the
/// rest. [`AhkProcess::stop`] is the explicit, orderly kill used by
/// fail-back and `flow stop`.
#[derive(Debug)]
pub struct AhkProcess {
    /// Kill-on-close job the script lives in.
    _job: JobObject,
    /// The script process (or the launcher that started it, on the
    /// fallback path).
    child: AhkChild,
    /// Process id recorded in the pidfile.
    pid: u32,
    /// Where the pidfile was written.
    pidfile: PathBuf,
    /// `true` when the process was created directly inside the job
    /// (`CreateProcess` suspended → assign → resume); `false` on the
    /// shell-association fallback, where only a best-effort late assignment
    /// was possible.
    job_tied: bool,
}

/// Process handle holder for the two launch paths.
#[derive(Debug)]
enum AhkChild {
    /// Spawned by `JobObject::spawn`.
    Job(JobChild),
    /// Launched via `ShellExecuteExW`; we hold the `hProcess` it returned.
    Shell(HANDLE),
}

impl AhkChild {
    fn is_running(&self) -> bool {
        match self {
            AhkChild::Job(c) => c.is_running(),
            // SAFETY: valid process handle from ShellExecuteExW.
            AhkChild::Shell(h) => unsafe { WaitForSingleObject(*h, 0) != WAIT_OBJECT_0 },
        }
    }
}

impl Drop for AhkChild {
    fn drop(&mut self) {
        if let AhkChild::Shell(h) = self {
            // SAFETY: we own the handle lent by ShellExecuteExW.
            let _ = unsafe { CloseHandle(*h) };
        }
    }
}

impl AhkProcess {
    /// Process id of the launched script.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// `true` while the launched process is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.child.is_running()
    }

    /// `true` when the script was created directly inside the job (the
    /// kernel will kill it with the daemon); `false` on the fallback path.
    #[must_use]
    pub fn is_job_tied(&self) -> bool {
        self.job_tied
    }

    /// Terminate the script now and remove the pidfile.
    ///
    /// Kills by pid (the same path `flow stop` uses) and then drops the job,
    /// which also terminates anything the script spawned. Returns `true` if
    /// a process was terminated.
    pub fn stop(self) -> bool {
        let killed = match kill_process(self.pid) {
            Ok(k) => k,
            Err(e) => {
                log::warn!("stopping flow.ahk (pid {}): {e}", self.pid);
                false
            }
        };
        if let Err(e) = fs::remove_file(&self.pidfile)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "failed to remove AHK pidfile {}: {e}",
                self.pidfile.display()
            );
        }
        // `self` (and the job handle) drops here.
        killed
    }
}

/// Launch `<config_dir>/flow.ahk` inside a kill-on-close job object.
///
/// Resolution order:
///
/// 1. [`resolve_ahk_interpreter`] finds an AutoHotkey v2 executable → it is
///    started with `JobObject::spawn` (suspended, assigned, resumed), so
///    the script is a job member from its first instruction and every child
///    it creates inherits the job.
/// 2. No interpreter found → the `.ahk` shell association is used (see
///    [`spawn_ahk_script_in`]) and the returned process is assigned to the
///    job **after** creation. A warning is logged, because if the
///    association points at a launcher that re-executes the real interpreter
///    and exits, that child may not be inside the job.
///
/// The pid is written to `<config_dir>/flow.ahk.pid` on both paths.
///
/// # Errors
///
/// Returns a string if `flow.ahk` is missing, the job cannot be created, or
/// both launch paths fail.
pub fn launch_ahk_in_job(config_dir: &Path) -> Result<AhkProcess, String> {
    let script = flow_ahk_path_in(config_dir);
    if !script.exists() {
        return Err(format!(
            "flow.ahk not found at {}; run `flow config init --ahk` to create it",
            script.display()
        ));
    }
    let job = JobObject::kill_on_close()?;
    let pidfile = ahk_pidfile_path_in(config_dir);
    // A pidfile that survived a previous run is stale by definition: its
    // process was either killed by the job or is an orphan we never reach
    // for (see `kill_process`). Drop it so nothing can mistake the old pid
    // for this launch while the new one is being written.
    match fs::remove_file(&pidfile) {
        Ok(()) => log::info!("removed stale AHK pidfile {}", pidfile.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!(
            "could not remove stale AHK pidfile {}: {e}",
            pidfile.display()
        ),
    }

    let process = match resolve_ahk_interpreter() {
        Some(exe) => {
            let child = job.spawn(&exe, &[script.as_os_str()], false)?;
            let pid = child.pid();
            log::info!(
                "launched flow.ahk with {} (pid {pid}) inside a kill-on-close job",
                exe.display()
            );
            AhkProcess {
                _job: job,
                child: AhkChild::Job(child),
                pid,
                pidfile: pidfile.clone(),
                job_tied: true,
            }
        }
        None => {
            log::warn!(
                "no AutoHotkey v2 interpreter found (registry InstallDir, %LOCALAPPDATA%\\Programs\\AutoHotkey, %ProgramFiles%\\AutoHotkey, PATH); \
                 falling back to the .ahk shell association — the job tie-in may not be in effect"
            );
            let (pid, handle) = shell_execute_ahk(&script)?;
            let job_tied = match job.assign(handle) {
                Ok(()) => {
                    log::info!(
                        "launched flow.ahk via shell association (pid {pid}); assigned to the job after creation"
                    );
                    true
                }
                Err(e) => {
                    log::warn!(
                        "launched flow.ahk via shell association (pid {pid}) but could not assign it to the job: {e}"
                    );
                    false
                }
            };
            AhkProcess {
                _job: job,
                child: AhkChild::Shell(handle),
                pid,
                pidfile: pidfile.clone(),
                job_tied,
            }
        }
    };

    if let Err(e) = fs::write(&pidfile, process.pid.to_string()) {
        log::warn!("failed to write AHK pidfile {}: {e}", pidfile.display());
    }
    Ok(process)
}

/// Candidate AutoHotkey v2 executables, in preference order, built from
/// the places an install can live. Pure: nothing is checked for existence.
///
/// * `install_dirs` — `InstallDir` values from `HKCU`/`HKLM`
///   `Software\AutoHotkey` (either order the caller prefers).
/// * `local_appdata` — `%LOCALAPPDATA%` (per-user installs live under
///   `Programs\AutoHotkey`).
/// * `program_files` — `%ProgramFiles%` (machine-wide installs).
///
/// Within each root the 64-bit v2 interpreter is preferred, then the 32-bit
/// one, then the root-level `AutoHotkey.exe` (the v2 launcher, which still
/// ends up in the job because children inherit it).
#[must_use]
pub fn ahk_interpreter_candidates(
    install_dirs: &[PathBuf],
    local_appdata: Option<&Path>,
    program_files: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = install_dirs.to_vec();
    if let Some(l) = local_appdata {
        roots.push(l.join("Programs").join("AutoHotkey"));
    }
    if let Some(p) = program_files {
        roots.push(p.join("AutoHotkey"));
    }
    let mut out = Vec::with_capacity(roots.len() * 3);
    for root in roots {
        let v2 = root.join("v2");
        for candidate in [
            v2.join("AutoHotkey64.exe"),
            v2.join("AutoHotkey32.exe"),
            root.join("AutoHotkey64.exe"),
            root.join("AutoHotkey.exe"),
        ] {
            if !out.contains(&candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

/// Find an AutoHotkey v2 interpreter on this machine.
///
/// Checks, in order, the registry `InstallDir` (`HKCU` then `HKLM`
/// `Software\AutoHotkey`), `%LOCALAPPDATA%\Programs\AutoHotkey`,
/// `%ProgramFiles%\AutoHotkey` (see [`ahk_interpreter_candidates`]), and
/// finally `AutoHotkey64.exe` / `AutoHotkey.exe` on `PATH`. Returns the
/// first candidate that exists.
#[must_use]
pub fn resolve_ahk_interpreter() -> Option<PathBuf> {
    let install_dirs: Vec<PathBuf> = [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
        .into_iter()
        .filter_map(|root| registry_string(root, "Software\\AutoHotkey", "InstallDir"))
        .map(PathBuf::from)
        .collect();
    let local_appdata = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
    if let Some(found) = ahk_interpreter_candidates(
        &install_dirs,
        local_appdata.as_deref(),
        program_files.as_deref(),
    )
    .into_iter()
    .find(|p| p.is_file())
    {
        return Some(found);
    }
    // PATH search.
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| [dir.join("AutoHotkey64.exe"), dir.join("AutoHotkey.exe")])
        .find(|p| p.is_file())
}

/// Read a `REG_SZ` value, or `None` if the key/value is absent.
fn registry_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
    let subkey_w = Wide::from_str(subkey);
    let value_w = Wide::from_str(value);
    let mut size: u32 = 0;
    // SAFETY: querying the size with a null buffer is the documented
    // two-step protocol.
    let rc = unsafe {
        RegGetValueW(
            root,
            subkey_w.pcwstr(),
            value_w.pcwstr(),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if rc.is_err() || size == 0 {
        return None;
    }
    let mut buf: Vec<u16> = vec![0; (size as usize).div_ceil(2)];
    // SAFETY: buffer sized from the first call; `size` is in bytes.
    let rc = unsafe {
        RegGetValueW(
            root,
            subkey_w.pcwstr(),
            value_w.pcwstr(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if rc.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..len]);
    (!s.trim().is_empty()).then(|| s.trim().to_owned())
}

/// Launch `script` via the `.ahk` shell association; returns the pid and
/// the process handle (`SEE_MASK_NOCLOSEPROCESS`), which the caller owns.
fn shell_execute_ahk(script: &Path) -> Result<(u32, HANDLE), String> {
    let script_w = Wide::from_os(script.as_os_str());
    let verb_w = Wide::from_str("open");

    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb_w.pcwstr(),
        lpFile: script_w.pcwstr(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };

    // SAFETY: ShellExecuteExW fills info.hProcess on success.
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| format!("ShellExecuteExW failed: {e}"))?;
    }
    if info.hProcess.is_invalid() {
        return Err(
            "ShellExecuteExW returned no process handle (is AutoHotkey installed?)".to_string(),
        );
    }
    // SAFETY: hProcess is valid (checked above) and owned by us until closed.
    let pid = unsafe { GetProcessId(info.hProcess) };
    if pid == 0 {
        // SAFETY: release the handle; we are returning an error.
        let _ = unsafe { CloseHandle(info.hProcess) };
        return Err("GetProcessId returned 0 after launching flow.ahk".to_string());
    }
    Ok((pid, info.hProcess))
}

/// Launch the user's `flow.ahk` via the shell's `.ahk` file association
/// (default config directory). See [`spawn_ahk_script_in`].
///
/// # Errors
///
/// See [`spawn_ahk_script_in`].
pub fn spawn_ahk_script() -> Result<u32, String> {
    spawn_ahk_script_in(&config_dir())
}

/// Launch `<config_dir>/flow.ahk` via the shell's `.ahk` file association.
///
/// Uses `ShellExecuteExW` with verb `"open"` so Windows resolves the installed
/// AutoHotkey interpreter — no install-path probing. The launched process
/// handle is read (via `SEE_MASK_NOCLOSEPROCESS`) and converted to a PID with
/// [`GetProcessId`], then written to a pidfile so [`stop_ahk_script`] can
/// terminate exactly this process later. This is the legacy, untied launch;
/// the daemon uses [`launch_ahk_in_job`].
///
/// # Returns
///
/// The launched AutoHotkey process ID on success.
///
/// # Errors
///
/// Returns an error string if:
/// - `flow.ahk` does not exist in the config directory.
/// - The shell association launch fails (e.g. AutoHotkey is not installed).
pub fn spawn_ahk_script_in(config_dir: &Path) -> Result<u32, String> {
    let script = flow_ahk_path_in(config_dir);
    if !script.exists() {
        return Err(format!(
            "flow.ahk not found at {}; run `flow config init --ahk` to create it",
            script.display()
        ));
    }

    let script_w = Wide::from_os(script.as_os_str());
    let verb_w = Wide::from_str("open");

    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: verb_w.pcwstr(),
        lpFile: script_w.pcwstr(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };

    // SAFETY: ShellExecuteExW fills info.hProcess on success.
    unsafe {
        ShellExecuteExW(&mut info).map_err(|e| format!("ShellExecuteExW failed: {e}"))?;
    }

    if info.hProcess.is_invalid() {
        return Err(
            "ShellExecuteExW returned no process handle (is AutoHotkey installed?)".to_string(),
        );
    }

    // SAFETY: hProcess is valid (checked above) and owned by us until closed.
    let pid = unsafe { GetProcessId(info.hProcess) };
    // SAFETY: release the handle Windows lent us; the outcome is irrelevant
    // because we already have the PID.
    let _ = unsafe { CloseHandle(info.hProcess) };

    if pid == 0 {
        return Err("GetProcessId returned 0 after launching flow.ahk".to_string());
    }

    let pidfile = ahk_pidfile_path_in(config_dir);
    if let Err(e) = fs::write(&pidfile, pid.to_string()) {
        log::warn!("failed to write AHK pidfile {}: {e}", pidfile.display());
    }
    Ok(pid)
}

/// Terminate the AutoHotkey process recorded in the default config
/// directory's pidfile. See [`stop_ahk_script_in`].
///
/// # Errors
///
/// See [`stop_ahk_script_in`].
pub fn stop_ahk_script() -> Result<bool, String> {
    stop_ahk_script_in(&config_dir())
}

/// Terminate the AutoHotkey process recorded in `<config_dir>/flow.ahk.pid`.
///
/// Reads the PID from the pidfile, kills that process via
/// [`TerminateProcess`], and removes the pidfile regardless of outcome.
/// Used by `flow stop` (always, no flag needed) and by the daemon's own
/// fail-back.
///
/// # Returns
///
/// `Ok(true)` if a process was killed, `Ok(false)` if there was no pidfile
/// (nothing to stop) or the pidfile was unreadable/stale.
///
/// # Errors
///
/// Returns an error string on filesystem errors reading the pidfile other
/// than `NotFound`, or on `TerminateProcess` failure.
pub fn stop_ahk_script_in(config_dir: &Path) -> Result<bool, String> {
    let pidfile = ahk_pidfile_path_in(config_dir);
    let pid_str = match fs::read_to_string(&pidfile) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("failed to read {}: {e}", pidfile.display())),
    };

    let result = match pid_str.trim().parse::<u32>() {
        Ok(pid) => kill_process(pid),
        Err(_) => Ok(false), // Corrupt pidfile — nothing to stop.
    };

    // Always remove the pidfile; either the process is gone, or it was stale.
    if let Err(e) = fs::remove_file(&pidfile)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("failed to remove AHK pidfile {}: {e}", pidfile.display());
    }
    result
}

// ── Path helpers ───────────────────────────────────────────────────────────

/// Path to the single autostart shortcut inside [`startup_dir`].
fn shortcut_path() -> Result<PathBuf, String> {
    Ok(startup_dir()?.join(FLOW_LNK))
}

/// Path to the running `flow.exe` (this binary).
///
/// Returns `current_exe()` directly — no existence check, since the OS already
/// proved the path is valid by starting this process. The only failure mode is
/// `current_exe()` itself failing (extremely rare).
///
/// # Errors
///
/// Returns an error string only if `current_exe()` fails.
fn flow_exe_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot determine current executable: {e}"))
}

/// Path to the user's `flow.ahk` inside the default config directory.
fn flow_ahk_path() -> PathBuf {
    flow_ahk_path_in(&config_dir())
}

/// Path to `flow.ahk` inside `config_dir`.
#[must_use]
pub fn flow_ahk_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("flow.ahk")
}

/// Path to the AHK pidfile inside `config_dir`.
#[must_use]
pub fn ahk_pidfile_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join(AHK_PIDFILE)
}

// ── Win32: process kill ────────────────────────────────────────────────────

/// `true` when a process image path names an AutoHotkey interpreter
/// (`AutoHotkey64.exe`, `AutoHotkey32.exe`, `AutoHotkeyUX.exe`, a
/// `…\AutoHotkey\v2\…` install, …): the file name or any path component
/// contains `autohotkey`, case-insensitively.
///
/// The pidfile only records a number, and Windows reuses process ids. A
/// pid written by a daemon that died an hour ago may now belong to anything
/// at all, so `kill_process` refuses to terminate a process whose image
/// does not pass this predicate.
#[must_use]
pub fn is_autohotkey_image(image_path: &str) -> bool {
    image_path.to_ascii_lowercase().contains("autohotkey")
}

/// Full image path of an open process (`QueryFullProcessImageNameW`), or
/// `None` if it cannot be read (the handle needs
/// `PROCESS_QUERY_LIMITED_INFORMATION`).
fn process_image_path(handle: HANDLE) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` UTF-16 units and both outlive the call; the
    // API writes at most `len` units and updates `len` to the count written.
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// Terminate the AutoHotkey process with this PID.
///
/// # Returns
///
/// `Ok(true)` if the process was terminated, `Ok(false)` if the PID does not
/// refer to a running process (stale pidfile) **or** the process has already
/// exited — e.g. the daemon's fail-back and `flow stop` both reaching for the
/// same hotkey script, or the job object having killed it first. That is
/// the normal case, not an error: `TerminateProcess` on an exited process
/// fails with `Access is denied`, so liveness is checked first.
///
/// Also `Ok(false)` — with an `info` log — when the pid is alive but its
/// image is not an AutoHotkey interpreter ([`is_autohotkey_image`]): the
/// pidfile outlived its process and Windows handed the number to someone
/// else. Killing that process would be the one unforgivable outcome of a
/// stale pidfile, so the pid is treated as stale instead.
///
/// # Errors
///
/// Returns an error string if `TerminateProcess` fails for a live process.
fn kill_process(pid: u32) -> Result<bool, String> {
    // SAFETY: OpenProcess with these rights only queries/terminates the
    // named PID; a stale PID either fails (Ok(false)) or targets a process
    // the user already has permission to terminate. SYNCHRONIZE lets us
    // poll whether it has already exited, QUERY_LIMITED_INFORMATION lets us
    // read its image name.
    let handle = match unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
    } {
        Ok(h) => h,
        Err(_) => return Ok(false), // Process not running (stale pidfile).
    };

    // SAFETY: valid process handle; zero timeout → non-blocking poll.
    if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
        log::debug!("kill_process: pid {pid} has already exited");
        // SAFETY: release the handle.
        let _ = unsafe { CloseHandle(handle) };
        return Ok(false);
    }

    // Pid reuse guard: only ever terminate an AutoHotkey interpreter.
    let image = process_image_path(handle);
    if !image.as_deref().is_some_and(is_autohotkey_image) {
        log::info!(
            "kill_process: pid {pid} is {} — the AHK pidfile is stale (pid reused); not terminating it",
            image.as_deref().unwrap_or("<unreadable image name>")
        );
        // SAFETY: release the handle.
        let _ = unsafe { CloseHandle(handle) };
        return Ok(false);
    }

    // SAFETY: valid process handle with PROCESS_TERMINATE.
    let result = unsafe { TerminateProcess(handle, 1) }
        .map(|()| true)
        .map_err(|e| format!("TerminateProcess failed: {e}"));

    // Always release the handle regardless of the kill outcome.
    // SAFETY: we own the handle.
    let _ = unsafe { CloseHandle(handle) };
    result
}

/// Block until the process with `pid` has exited, or `timeout` elapses.
///
/// Returns `true` when the process is gone (it exited, or no such process
/// could be opened — already reaped), `false` on timeout. `flow stop` uses
/// it after the daemon's status flips to `stopped` so that the process —
/// and with it the named pipe — is really gone before `flow start` may
/// run again.
#[must_use]
pub fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    // SAFETY: SYNCHRONIZE grants only the right to wait on the process.
    let handle = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(h) => h,
        // No such process (or no access, which for our own daemon means it
        // is already gone).
        Err(_) => return true,
    };
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: valid process handle; returns WAIT_OBJECT_0 once it exits.
    let exited = unsafe { WaitForSingleObject(handle, ms) } == WAIT_OBJECT_0;
    // SAFETY: we own the handle.
    let _ = unsafe { CloseHandle(handle) };
    exited
}

// ── Win32: COM shortcut creation ───────────────────────────────────────────

/// Owned wide-string backing for `PCWSTR` borrows.
///
/// Keeps the `Vec<u16>` (null-terminated) alive so the borrowed `PCWSTR`
/// remains valid as long as `self` is held. The borrow through `&self`
/// enforces the lifetime at the call site.
struct Wide {
    buf: Vec<u16>,
}

impl Wide {
    /// Encode an `OsStr` to a null-terminated UTF-16 buffer.
    fn from_os(s: &OsStr) -> Self {
        Self {
            buf: s.encode_wide().chain(std::iter::once(0)).collect(),
        }
    }

    /// Encode a `&str` to a null-terminated UTF-16 buffer.
    fn from_str(s: &str) -> Self {
        Self::from_os(OsStr::new(s))
    }

    /// Borrow the buffer as a `PCWSTR`.
    fn pcwstr(&self) -> PCWSTR {
        PCWSTR::from_raw(self.buf.as_ptr())
    }
}

/// Create or overwrite a Windows `.lnk` shortcut via COM.
///
/// Initializes COM for the current thread, builds an `IShellLinkW` with the
/// supplied target/arguments/working-directory, persists it via
/// `IPersistFile::Save`, and uninitializes COM.
///
/// # Arguments
///
/// * `link_path` — Where to write the `.lnk`.
/// * `target` — What the shortcut runs (an executable).
/// * `args` — Optional command-line arguments.
/// * `working_dir` — Optional working directory.
///
/// # Errors
///
/// Returns an error string on any COM/HRESULT failure.
///
/// # Safety
///
/// `CoInitializeEx` / `CoUninitialize` are paired. Each COM method below is
/// `pub unsafe fn` in `windows-0.62.2`, so each call has its own minimal
/// `unsafe` scope. `Wide` buffers outlive every `PCWSTR` borrow. `S_FALSE`
/// (already-initialized) is a success HRESULT, so `is_err()` treats it as Ok.
fn create_shortcut(
    link_path: &Path,
    target: &Path,
    args: Option<&str>,
    working_dir: Option<&Path>,
) -> Result<(), String> {
    let target_w = Wide::from_os(target.as_os_str());
    let args_w = args.map(Wide::from_str);
    let work_w = working_dir.map(|p| Wide::from_os(p.as_os_str()));
    let link_w = Wide::from_os(link_path.as_os_str());

    // SAFETY: CoInitializeEx on this thread; S_FALSE (already-initialized) is
    // a success code so is_err() treats it as Ok.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(format!("CoInitializeEx failed: {hr}"));
    }

    let result = (|| -> windows::core::Result<()> {
        // SAFETY: CoCreateInstance on ShellLink CLSID is the documented
        // constructor for IShellLinkW.
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };

        // SAFETY: target_w outlives this call.
        unsafe { link.SetPath(target_w.pcwstr())? };
        if let Some(a) = &args_w {
            // SAFETY: a outlives this call.
            unsafe { link.SetArguments(a.pcwstr())? };
        }
        if let Some(w) = &work_w {
            // SAFETY: w outlives this call.
            unsafe { link.SetWorkingDirectory(w.pcwstr())? };
        }
        // SAFETY: SW_HIDE suppresses the short-lived console at login.
        unsafe { link.SetShowCmd(SW_HIDE)? };

        let persist: IPersistFile = link.cast()?;
        // SAFETY: link_w outlives this call; `true` overwrites an existing .lnk.
        unsafe { persist.Save(link_w.pcwstr(), true)? };
        Ok(())
    })();

    // SAFETY: pairs with CoInitializeEx above; always runs so COM is torn down
    // even on the error path.
    unsafe { CoUninitialize() };

    result.map_err(|e| format!("failed to create shortcut: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate `APPDATA` / `FLOW_CONFIG_DIR`. See the
    /// identical pattern in `src/config/dirs.rs`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Capture the original values of `APPDATA` and `FLOW_CONFIG_DIR` so a test
    /// can restore them on drop.
    struct EnvGuard {
        appdata: Option<String>,
        config_dir: Option<String>,
    }

    impl EnvGuard {
        fn capture() -> Self {
            Self {
                appdata: std::env::var("APPDATA").ok(),
                config_dir: std::env::var("FLOW_CONFIG_DIR").ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: serialized by ENV_LOCK; tests are short-lived and run
            // single-threaded within the lock.
            match self.appdata.take() {
                Some(v) => unsafe { std::env::set_var("APPDATA", v) },
                None => unsafe { std::env::remove_var("APPDATA") },
            }
            match self.config_dir.take() {
                Some(v) => unsafe { std::env::set_var("FLOW_CONFIG_DIR", v) },
                None => unsafe { std::env::remove_var("FLOW_CONFIG_DIR") },
            }
        }
    }

    /// Build a tempdir tree and point both `%APPDATA%` and `FLOW_CONFIG_DIR`
    /// at it, returning `(startup_dir, config_dir)`.
    ///
    /// The caller must hold `ENV_LOCK` for the duration.
    fn make_startup_tree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let appdata = tmp.path().join("AppData");
        let startup = appdata
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup");
        fs::create_dir_all(&startup).unwrap();
        let cfg = tmp.path().join("config").join("flow");
        fs::create_dir_all(&cfg).unwrap();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::set_var("APPDATA", &appdata) };
        unsafe { std::env::set_var("FLOW_CONFIG_DIR", &cfg) };
        (tmp, startup, cfg)
    }

    // ── startup_dir ────────────────────────────────────────────────────────

    #[test]
    fn startup_dir_ends_with_startup_when_appdata_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::set_var("APPDATA", "C:\\fake_roaming") };

        let dir = startup_dir().expect("APPDATA is set");
        assert!(dir.ends_with("Startup"), "dir was: {dir:?}");
        assert!(
            dir.to_string_lossy().contains("Start Menu"),
            "dir was: {dir:?}"
        );
    }

    #[test]
    fn startup_dir_errors_without_appdata() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("APPDATA") };

        let result = startup_dir();
        assert!(result.is_err(), "should error when APPDATA is unset");
    }

    // ── path helpers ───────────────────────────────────────────────────────

    #[test]
    fn ahk_pidfile_path_is_stable_name_in_config_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, cfg) = make_startup_tree();

        let path = ahk_pidfile_path_in(&config_dir());
        assert_eq!(path, cfg.join("flow.ahk.pid"));
    }

    #[test]
    fn flow_ahk_path_points_at_config_dir_script() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, cfg) = make_startup_tree();

        let path = flow_ahk_path();
        assert_eq!(path, cfg.join("flow.ahk"));
    }

    #[test]
    fn shortcut_filename_is_stable() {
        assert_eq!(FLOW_LNK, "flow.lnk");
        assert_eq!(AHK_PIDFILE, "flow.ahk.pid");
    }

    #[test]
    fn interpreter_candidates_prefer_v2_64bit_then_32bit_then_launcher() {
        let reg = PathBuf::from("C:\\Program Files\\AutoHotkey");
        let local = PathBuf::from("C:\\Users\\me\\AppData\\Local");
        let pf = PathBuf::from("C:\\Program Files");
        let c = ahk_interpreter_candidates(std::slice::from_ref(&reg), Some(&local), Some(&pf));
        assert_eq!(c[0], reg.join("v2").join("AutoHotkey64.exe"));
        assert_eq!(c[1], reg.join("v2").join("AutoHotkey32.exe"));
        assert_eq!(c[2], reg.join("AutoHotkey64.exe"));
        assert_eq!(c[3], reg.join("AutoHotkey.exe"));
        // The per-user root comes after the registry root.
        let user_root = local.join("Programs").join("AutoHotkey");
        assert_eq!(c[4], user_root.join("v2").join("AutoHotkey64.exe"));
        // The ProgramFiles root is the same as the registry root here, so
        // its entries are deduplicated rather than repeated.
        assert_eq!(c.len(), 8, "{c:?}");
        // No roots at all → no candidates.
        assert!(ahk_interpreter_candidates(&[], None, None).is_empty());
    }

    #[test]
    fn stop_ahk_script_in_uses_the_given_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!stop_ahk_script_in(tmp.path()).expect("no pidfile is a no-op"));
        fs::write(ahk_pidfile_path_in(tmp.path()), "garbage").unwrap();
        assert!(!stop_ahk_script_in(tmp.path()).expect("corrupt pidfile is a no-op"));
        assert!(!ahk_pidfile_path_in(tmp.path()).exists());
    }

    #[test]
    fn kill_process_treats_exited_process_as_nothing_to_do() {
        // Spawn a child that exits immediately, wait for it, then "kill" it:
        // the pid is gone (or recycled only in theory), so this must be
        // Ok(false), never an Access-denied error.
        let child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .output()
            .expect("spawn cmd");
        assert!(child.status.success());
        // Our own pid is alive; opening it works, but we must not kill it —
        // so only exercise the exited path with a definitely-dead pid: 0 is
        // the idle process and cannot be opened → Ok(false) as well.
        assert_eq!(kill_process(0), Ok(false));
    }

    #[test]
    fn autohotkey_image_predicate_is_case_insensitive_and_path_tolerant() {
        assert!(is_autohotkey_image(
            r"C:\Program Files\AutoHotkey\v2\AutoHotkey64.exe"
        ));
        assert!(is_autohotkey_image(
            r"c:\users\me\appdata\local\programs\autohotkey\v2\autohotkey32.exe"
        ));
        assert!(is_autohotkey_image("AUTOHOTKEYUX.EXE"));
        // A reused pid now belonging to something else must never pass.
        assert!(!is_autohotkey_image(r"C:\Windows\System32\svchost.exe"));
        assert!(!is_autohotkey_image(
            r"C:\Program Files\Mozilla Firefox\firefox.exe"
        ));
        assert!(!is_autohotkey_image(""));
    }

    #[test]
    fn wait_for_process_exit_is_true_for_a_nonexistent_pid_and_false_for_self() {
        // Pid 0 is the System Idle Process: OpenProcess fails → "gone".
        assert!(wait_for_process_exit(0, Duration::from_millis(10)));
        // Our own process is alive: the bounded wait must time out.
        let t0 = std::time::Instant::now();
        assert!(!wait_for_process_exit(
            std::process::id(),
            Duration::from_millis(50)
        ));
        assert!(t0.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn launch_ahk_in_job_errors_when_script_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = launch_ahk_in_job(tmp.path()).expect_err("no flow.ahk");
        assert!(err.contains("flow.ahk not found"), "{err}");
        assert!(!ahk_pidfile_path_in(tmp.path()).exists());
    }

    // ── disable_autostart ──────────────────────────────────────────────────

    #[test]
    fn disable_autostart_no_op_when_shortcut_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, startup, _cfg) = make_startup_tree();

        // No flow.lnk exists yet.
        let report = disable_autostart().expect("missing shortcut is a no-op");
        assert_eq!(report.shortcut, startup.join(FLOW_LNK));
        assert!(!report.shortcut.exists());
    }

    #[test]
    fn disable_autostart_removes_existing_shortcut() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, startup, _cfg) = make_startup_tree();

        let link = startup.join(FLOW_LNK);
        fs::write(&link, b"placeholder").unwrap();
        assert!(link.exists());

        let report = disable_autostart().expect("removing existing shortcut");
        assert!(!link.exists(), "shortcut should be removed");
        assert_eq!(report.shortcut, link);
    }

    // ── enable_autostart ───────────────────────────────────────────────────

    #[test]
    fn enable_autostart_with_ahk_errors_when_script_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, startup, _cfg) = make_startup_tree();

        // flow.ahk does NOT exist in the config dir.
        let result = enable_autostart(true);
        let err = result.expect_err("should error when flow.ahk missing");
        assert!(
            err.contains("flow.ahk not found"),
            "error should mention flow.ahk: {err}"
        );
        assert!(
            err.contains("flow config init --ahk"),
            "error should suggest the fix: {err}"
        );
        // The pre-flight check must run BEFORE COM, so no shortcut is created.
        assert!(
            !startup.join(FLOW_LNK).exists(),
            "no shortcut should be left behind"
        );
    }

    // ── stop_ahk_script ────────────────────────────────────────────────────

    #[test]
    fn stop_ahk_script_returns_false_when_pidfile_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, _cfg) = make_startup_tree();

        let result = stop_ahk_script().expect("missing pidfile is a no-op");
        assert!(!result, "should report nothing was stopped");
    }

    #[test]
    fn stop_ahk_script_handles_corrupt_pidfile() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, cfg) = make_startup_tree();

        let pidfile = cfg.join("flow.ahk.pid");
        fs::write(&pidfile, "not-a-number").unwrap();

        let result = stop_ahk_script().expect("corrupt pidfile is a no-op");
        assert!(!result, "should report nothing was stopped");
        // The corrupt pidfile must be cleaned up so the next start is clean.
        assert!(!pidfile.exists(), "corrupt pidfile should be removed");
    }

    #[test]
    fn stop_ahk_script_errors_when_pidfile_unreadable() {
        // Negative: a pidfile path that exists but cannot be read as a file
        // (here: a directory) is neither `NotFound` nor a corrupt-content
        // case, so it must fall through to the generic I/O error branch and
        // surface an `Err`. On Windows, `read_to_string` on a directory
        // yields `PermissionDenied`, which is `!= NotFound` → Err path.
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        let (_tmp, _startup, cfg) = make_startup_tree();

        let pidfile = cfg.join("flow.ahk.pid");
        fs::create_dir(&pidfile).expect("create dir as pidfile");

        let result = stop_ahk_script();
        assert!(
            result.is_err(),
            "unreadable pidfile should surface an error, got: {result:?}"
        );
        // The error must mention the pidfile path so the user can diagnose.
        let err = result.unwrap_err();
        assert!(
            err.contains("flow.ahk.pid"),
            "error should reference the pidfile path: {err}"
        );
    }

    // ── error propagation when APPDATA is unset ───────────────────────────
    //
    // `startup_dir()` is the root of the path-resolution chain; if APPDATA
    // is missing, every public command that needs `shortcut_path()` must
    // surface that error verbatim (via `?`) rather than panicking or
    // silently succeeding.

    #[test]
    fn disable_autostart_errors_when_appdata_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("APPDATA") };

        let result = disable_autostart();
        let err = result.expect_err("should error when APPDATA is unset");
        assert!(
            err.contains("%APPDATA%"),
            "error should mention %APPDATA%: {err}"
        );
    }

    #[test]
    fn enable_autostart_without_ahk_errors_when_appdata_unset() {
        // With `ahk = false`, the flow.ahk pre-flight check is skipped, so
        // the next call is `flow_exe_path()` (which succeeds — the test
        // binary exists) followed by `shortcut_path()`, which propagates
        // the APPDATA-unset error BEFORE any COM call. This pins that the
        // APPDATA check runs ahead of COM initialization.
        let _guard = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::capture();
        // SAFETY: serialized by ENV_LOCK.
        unsafe { std::env::remove_var("APPDATA") };

        let result = enable_autostart(false);
        let err = result.expect_err("should error when APPDATA is unset");
        assert!(
            err.contains("%APPDATA%"),
            "error should mention %APPDATA%: {err}"
        );
    }

    // ── Wide: UTF-16 encoding helpers ──────────────────────────────────────
    //
    // `Wide` is the bridge between Rust `&str`/`OsStr` and the borrowed
    // `PCWSTR` Win32 ABI. Its correctness contract is:
    //   1. The buffer is null-terminated.
    //   2. `from_str` and `from_os` agree for ASCII input.
    //   3. `pcwstr()` returns a pointer into the live buffer.
    // These tests pin all three.

    #[test]
    fn wide_from_str_ascii_encodes_with_null_terminator() {
        // Nominal: each ASCII byte maps to one UTF-16 code unit, plus the
        // trailing NUL.
        let w = Wide::from_str("ABC");
        assert_eq!(w.buf, vec![0x41, 0x42, 0x43, 0x00]);
    }

    #[test]
    fn wide_from_str_empty_yields_only_null_terminator() {
        // Edge: empty input must still produce a valid (zero-length) wide
        // string — the buffer holds only the NUL terminator. Win32 APIs
        // that accept empty strings rely on this.
        let w = Wide::from_str("");
        assert_eq!(w.buf, vec![0x00]);
    }

    #[test]
    fn wide_from_str_unicode_bmp_encodes_as_single_code_unit() {
        // Non-ASCII within the Basic Multilingual Plane stays a single
        // UTF-16 code unit: U+00E9 (é) → 0xE9.
        let w = Wide::from_str("café");
        assert_eq!(w.buf, vec![0x63, 0x61, 0x66, 0xE9, 0x00]);
    }

    #[test]
    fn wide_from_str_supplemental_plane_encodes_as_surrogate_pair() {
        // Astral plane (U+1F600 GRINNING FACE) requires a surrogate pair.
        // Computed: (0x1F600 - 0x10000) >> 10 = 0x3D  → high = 0xD800 + 0x3D = 0xD83D
        //           (0x1F600 - 0x10000) & 0x3FF = 0x200 → low  = 0xDC00 + 0x200 = 0xDE00
        // This pins that `encode_wide` (Rust's WTF-8 → UTF-16) is used unchanged.
        let w = Wide::from_str("\u{1F600}");
        assert_eq!(w.buf, vec![0xD83D, 0xDE00, 0x00]);
    }

    #[test]
    fn wide_from_os_matches_from_str_for_ascii() {
        // `from_os` and `from_str` must agree on ASCII input, since
        // `from_str` delegates to `from_os(OsStr::new(s))`. This pins the
        // delegation and guards against a future divergence.
        let s = "hello";
        let from_os = Wide::from_os(OsStr::new(s));
        let from_str = Wide::from_str(s);
        assert_eq!(from_os.buf, from_str.buf);
    }

    #[test]
    fn wide_pcwstr_points_into_buffer_with_null_terminator() {
        // The PCWSTR returned by `pcwstr()` must point at the buffer's
        // first code unit and the buffer must be NUL-terminated so Win32
        // APIs can find the end. We read it back through PCWSTR's own
        // unsafe accessor to round-trip the data.
        let w = Wide::from_str("hi");
        let ptr = w.pcwstr();
        assert!(!ptr.is_null(), "PCWSTR must not be null");

        // SAFETY: `ptr` borrows `w.buf`, which is alive for the duration of
        // this read. `as_wide` calls `wcslen` and slices up to the NUL,
        // which we know is present at index 2 from the encoder's contract.
        let round_trip = unsafe { ptr.to_string() }.expect("valid UTF-16");
        assert_eq!(round_trip, "hi", "PCWSTR should round-trip the input");
    }
}
