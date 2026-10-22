//! Win32 Job Objects: tie a child process's lifetime to this process.
//!
//! A `JobObject` created with `JobObject::kill_on_close` carries the
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` limit: when the **last handle** to
//! the job is closed, the kernel terminates every process in it. The daemon
//! keeps the handle open for its whole life and never closes it explicitly,
//! so *any* exit — clean stop, panic, abort, `taskkill /f`, a crash in a
//! Win32 callback — closes the handle through normal process teardown and
//! the kernel kills the hotkey script with it. That is what releases the
//! Win key after the daemon dies (see `docs/src/dev-guide/ipc-and-watchdog.md`).
//!
//! The job also carries `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`: a process a
//! job member creates is **not** associated with the job. The member we
//! care about — the hotkey script — is spawned by the daemon itself (see
//! [`JobObject::spawn`]), so it is always in the job; but the terminals and
//! applications the script launches (Mod+T, the launcher) are the user's,
//! not the daemon's, and must survive `flow stop` and a daemon crash. Before
//! this flag every such child inherited the job and died with the daemon.
//! The trade-off: a launcher that re-executes the real interpreter and
//! exits leaves that interpreter outside the job, which is why the daemon
//! resolves the interpreter directly instead of going through the `.ahk`
//! shell association (see `autostart::launch_ahk_in_job`).

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

/// How [`JobObject::spawn`] creates the child.
#[derive(Debug, Default)]
pub(crate) struct SpawnOptions {
    /// Add `CREATE_NO_WINDOW` (console children only).
    pub no_window: bool,
    /// Working directory for the child; `None` inherits the daemon's.
    pub current_dir: Option<PathBuf>,
    /// Environment variables set (or overwritten) in the child's
    /// environment, on top of a copy of the daemon's own. Empty = inherit
    /// the daemon's environment block unchanged.
    pub env: Vec<(OsString, OsString)>,
}

/// An owned job-object handle. Closing it (on drop) kills every member
/// process when the job was created with `JobObject::kill_on_close`.
#[derive(Debug)]
pub(crate) struct JobObject(HANDLE);

// SAFETY: a kernel HANDLE is a process-wide identifier; using it from
// another thread is fine. The daemon stores the job inside `FlowWM` on the
// main thread only, but `Send` is required for that struct to be moved
// into `catch_unwind` closures.
unsafe impl Send for JobObject {}

impl JobObject {
    /// Create an anonymous job whose members are killed when the last handle
    /// to it is closed, and whose members' children are **not** put in the
    /// job (`JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`): the hotkey script dies
    /// with the daemon, the terminals and apps it launches do not.
    ///
    /// # Errors
    ///
    /// Returns a string describing the failing Win32 call
    /// (`CreateJobObjectW` or `SetInformationJobObject`).
    pub fn kill_on_close() -> Result<Self, String> {
        // SAFETY: anonymous job, default security attributes.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|e| format!("CreateJobObjectW failed: {e}"))?;
        let job = Self(handle);

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        // SAFETY: `info` is a fully-initialised extended-limit struct and the
        // length passed matches its size.
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|e| format!("SetInformationJobObject(KILL_ON_JOB_CLOSE) failed: {e}"))?;
        Ok(job)
    }

    /// Put `process` (a handle with `PROCESS_SET_QUOTA | PROCESS_TERMINATE`
    /// access) into this job.
    ///
    /// # Errors
    ///
    /// Returns the `AssignProcessToJobObject` error as a string.
    pub fn assign(&self, process: HANDLE) -> Result<(), String> {
        // SAFETY: both handles are valid for the duration of the call.
        unsafe { AssignProcessToJobObject(self.0, process) }
            .map_err(|e| format!("AssignProcessToJobObject failed: {e}"))
    }

    /// Spawn `exe` with `args`, created **suspended**, assign it to this
    /// job, then resume it — so it is a job member from its first
    /// instruction. Its own children are not job members (silent breakaway,
    /// see the module docs).
    ///
    /// `opts` selects `CREATE_NO_WINDOW`, the working directory and extra
    /// environment variables (see [`SpawnOptions`]). The returned
    /// [`JobChild`] owns the process handle.
    ///
    /// # Errors
    ///
    /// Returns a string on `CreateProcessW` failure. A failure to assign the
    /// (still suspended) process to the job terminates it and is returned as
    /// an error too, so a child can never run outside the job by accident.
    pub fn spawn(
        &self,
        exe: &Path,
        args: &[&OsStr],
        opts: &SpawnOptions,
    ) -> Result<JobChild, String> {
        let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut cmdline_w: Vec<u16> = OsStr::new(&build_command_line(exe, args))
            .encode_wide()
            .chain(Some(0))
            .collect();
        let cwd_w: Option<Vec<u16>> = opts
            .current_dir
            .as_ref()
            .map(|d| d.as_os_str().encode_wide().chain(Some(0)).collect());
        // An explicit environment block only when something is overridden;
        // otherwise the child inherits ours (lpEnvironment = NULL).
        let env_block: Option<Vec<u16>> = (!opts.env.is_empty()).then(|| {
            build_environment_block(std::env::vars_os(), &opts.env)
                .encode_wide()
                .collect()
        });

        let mut flags: PROCESS_CREATION_FLAGS = CREATE_SUSPENDED;
        if opts.no_window {
            flags |= CREATE_NO_WINDOW;
        }
        if env_block.is_some() {
            flags |= CREATE_UNICODE_ENVIRONMENT;
        }
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        // SAFETY: `exe_w`, `cmdline_w` and `cwd_w` are NUL-terminated and
        // outlive the call; `cmdline_w` is mutable as CreateProcessW
        // requires; `env_block` is a double-NUL-terminated UTF-16 block (see
        // `build_environment_block`) flagged with CREATE_UNICODE_ENVIRONMENT;
        // `startup` has its `cb` set; `pi` receives the handles.
        unsafe {
            CreateProcessW(
                PCWSTR::from_raw(exe_w.as_ptr()),
                Some(PWSTR::from_raw(cmdline_w.as_mut_ptr())),
                None,
                None,
                false,
                flags,
                env_block
                    .as_ref()
                    .map(|b| b.as_ptr() as *const core::ffi::c_void),
                cwd_w
                    .as_ref()
                    .map_or(PCWSTR::null(), |c| PCWSTR::from_raw(c.as_ptr())),
                &startup,
                &mut pi,
            )
        }
        .map_err(|e| format!("CreateProcessW({}) failed: {e}", exe.display()))?;

        let child = JobChild {
            process: pi.hProcess,
            pid: pi.dwProcessId,
        };

        if let Err(e) = self.assign(pi.hProcess) {
            // Never let it run outside the job: it is still suspended.
            // SAFETY: valid process handle from CreateProcessW.
            let _ = unsafe { TerminateProcess(pi.hProcess, 1) };
            // SAFETY: thread handle from CreateProcessW; `child` closes the
            // process handle on drop.
            let _ = unsafe { CloseHandle(pi.hThread) };
            return Err(e);
        }

        // SAFETY: valid primary-thread handle; ResumeThread returns the
        // previous suspend count or u32::MAX on failure.
        let prev = unsafe { ResumeThread(pi.hThread) };
        // SAFETY: we are done with the thread handle.
        let _ = unsafe { CloseHandle(pi.hThread) };
        if prev == u32::MAX {
            return Err(format!(
                "ResumeThread failed for pid {}: {}",
                pi.dwProcessId,
                windows::core::Error::from_thread()
            ));
        }
        Ok(child)
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // SAFETY: we own the handle. With KILL_ON_JOB_CLOSE this is what
        // terminates the members when it is the last handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A process spawned by [`JobObject::spawn`]: its pid and an owned process
/// handle (closed on drop — closing the handle does **not** kill the
/// process; the job does that).
#[derive(Debug)]
pub(crate) struct JobChild {
    process: HANDLE,
    pid: u32,
}

// SAFETY: see `JobObject`.
unsafe impl Send for JobChild {}

impl JobChild {
    /// Process id of the child.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// `true` while the process has not exited.
    #[must_use]
    pub fn is_running(&self) -> bool {
        // SAFETY: valid process handle; zero timeout → non-blocking poll.
        unsafe { WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 }
    }

    /// Terminate the process through the handle this child already holds —
    /// never by re-opening its pid, which Windows may have reused by now.
    /// Returns `Ok(false)` when it had already exited.
    ///
    /// # Errors
    ///
    /// Returns the `TerminateProcess` error as a string.
    pub fn terminate(&self) -> Result<bool, String> {
        if !self.is_running() {
            return Ok(false);
        }
        // SAFETY: our own process handle, opened by CreateProcessW with
        // full access.
        unsafe { TerminateProcess(self.process, 1) }
            .map(|()| true)
            .map_err(|e| format!("TerminateProcess failed: {e}"))
    }

    /// Block up to `timeout_ms` for the process to exit; returns its exit
    /// code, or `None` on timeout. (Only the spawn tests need it.)
    #[cfg(test)]
    #[must_use]
    pub fn wait(&self, timeout_ms: u32) -> Option<u32> {
        use windows::Win32::System::Threading::GetExitCodeProcess;
        // SAFETY: valid process handle.
        if unsafe { WaitForSingleObject(self.process, timeout_ms) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0_u32;
        // SAFETY: valid process handle; `code` receives the exit code.
        unsafe { GetExitCodeProcess(self.process, &mut code) }
            .ok()
            .map(|()| code)
    }
}

impl Drop for JobChild {
    fn drop(&mut self) {
        // SAFETY: we own the process handle.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

/// Build a `CreateProcessW` environment block (`CREATE_UNICODE_ENVIRONMENT`
/// form, before UTF-16 encoding): `inherited` with every variable in
/// `overrides` set or replaced (names compared case-insensitively, as
/// Windows does), sorted by upper-cased name as the API recommends, each
/// `NAME=value` NUL-terminated and the whole block terminated by a second
/// NUL. Pure; the caller encodes it with `encode_wide`.
#[must_use]
pub(crate) fn build_environment_block(
    inherited: impl Iterator<Item = (OsString, OsString)>,
    overrides: &[(OsString, OsString)],
) -> OsString {
    let key_of = |k: &OsString| k.to_string_lossy().to_uppercase();
    let mut vars: Vec<(OsString, OsString)> = inherited
        .filter(|(k, _)| !overrides.iter().any(|(ok, _)| key_of(ok) == key_of(k)))
        .chain(overrides.iter().cloned())
        .collect();
    vars.sort_by_key(|(k, _)| key_of(k));
    let mut block = OsString::new();
    for (k, v) in vars {
        block.push(k);
        block.push("=");
        block.push(v);
        block.push("\0");
    }
    if block.is_empty() {
        // An empty block is still two NULs.
        block.push("\0");
    }
    block.push("\0");
    block
}

/// Build a `CreateProcessW` command line: the executable and each argument
/// quoted and escaped per the MSVC CRT rules (`"` → `\"`, backslashes before
/// a quote doubled).
#[must_use]
pub(crate) fn build_command_line(exe: &Path, args: &[&OsStr]) -> String {
    let mut line = quote_arg(&exe.to_string_lossy());
    for a in args {
        line.push(' ');
        line.push_str(&quote_arg(&a.to_string_lossy()));
    }
    line
}

/// Quote one argument per the MSVC CRT parsing rules.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '"')) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0_usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cmd_exe() -> PathBuf {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        PathBuf::from(system_root).join("System32").join("cmd.exe")
    }

    #[test]
    fn command_line_quoting_follows_crt_rules() {
        let exe = Path::new("C:\\Program Files\\AutoHotkey\\v2\\AutoHotkey64.exe");
        let script = OsStr::new("C:\\Users\\me\\.config\\flow\\flow.ahk");
        assert_eq!(
            build_command_line(exe, &[script]),
            "\"C:\\Program Files\\AutoHotkey\\v2\\AutoHotkey64.exe\" C:\\Users\\me\\.config\\flow\\flow.ahk"
        );
        // Spaces force quotes; embedded quotes are escaped; trailing
        // backslashes before the closing quote are doubled.
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(
            quote_arg("C:\\dir with space\\"),
            "\"C:\\dir with space\\\\\""
        );
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("plain"), "plain");
    }

    fn ping_exe() -> PathBuf {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        PathBuf::from(system_root).join("System32").join("ping.exe")
    }

    fn hidden() -> SpawnOptions {
        SpawnOptions {
            no_window: true,
            ..Default::default()
        }
    }

    /// Pids currently in `job` (`JobObjectBasicProcessIdList`).
    fn job_pids(job: &JobObject) -> Vec<usize> {
        use windows::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicProcessIdList, QueryInformationJobObject,
        };
        // Room for 64 pids after the fixed header.
        let mut buf = vec![0u8; std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() + 64 * 8];
        // SAFETY: the buffer is at least the size of the list header and the
        // kernel writes at most its length.
        unsafe {
            QueryInformationJobObject(
                Some(job.0),
                JobObjectBasicProcessIdList,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                None,
            )
        }
        .expect("QueryInformationJobObject");
        // SAFETY: the header was written by the call above.
        let list = unsafe { &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST) };
        let n = list.NumberOfProcessIdsInList as usize;
        // SAFETY: `n` pids follow the header, inside `buf`.
        let pids = unsafe { std::slice::from_raw_parts(list.ProcessIdList.as_ptr(), n.min(64)) };
        pids.to_vec()
    }

    #[test]
    fn job_child_runs_inside_job_and_exits() {
        // Smoke test: create a job, spawn `cmd /c exit 7` inside it, and
        // observe the child exit with that code.
        let job = JobObject::kill_on_close().expect("job");
        let child = job
            .spawn(
                &cmd_exe(),
                &[OsStr::new("/c"), OsStr::new("exit"), OsStr::new("7")],
                &hidden(),
            )
            .expect("spawn cmd in job");
        assert!(child.pid() != 0);
        assert_eq!(child.wait(10_000), Some(7), "child should exit with 7");
        assert!(!child.is_running());
    }

    #[test]
    fn closing_the_job_kills_its_members() {
        // A member that would run for a minute is terminated as soon as the
        // last job handle is closed — the property the AHK tie-in relies on.
        // `ping` is the member itself (no cmd in between, whose child would
        // now break away and outlive the job).
        let job = JobObject::kill_on_close().expect("job");
        let child = job
            .spawn(
                &ping_exe(),
                &[OsStr::new("-n"), OsStr::new("60"), OsStr::new("127.0.0.1")],
                &hidden(),
            )
            .expect("spawn long-running ping in job");
        assert!(child.is_running(), "child should still be running");
        drop(job);
        assert!(
            child.wait(5_000).is_some(),
            "child must be killed when the job handle is closed"
        );
    }

    #[test]
    fn children_of_a_member_break_away_from_the_job() {
        // Bug A: apps the hotkey script launched died with the daemon. With
        // SILENT_BREAKAWAY_OK the member's own child (here: cmd's `ping`) is
        // not in the job — the job's pid list only ever holds cmd itself.
        let job = JobObject::kill_on_close().expect("job");
        let child = job
            .spawn(
                &cmd_exe(),
                &[OsStr::new("/c"), OsStr::new("ping -n 3 127.0.0.1 >nul")],
                &hidden(),
            )
            .expect("spawn cmd in job");
        // Give cmd time to start ping, then look a few times.
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let pids = job_pids(&job);
            assert!(
                pids.iter().all(|&p| p == child.pid() as usize),
                "only the spawned member may be in the job, got {pids:?} (member {})",
                child.pid()
            );
        }
        assert!(child.terminate().expect("terminate"), "cmd was running");
        assert!(child.wait(5_000).is_some());
        assert_eq!(child.terminate(), Ok(false), "already exited");
    }

    #[test]
    fn spawn_applies_env_overrides_and_working_directory() {
        // `cmd /c echo %FLOW_BIN% & cd` into a file, read back.
        let tmp = tempfile::tempdir().expect("tempdir");
        let out = tmp.path().join("env.txt");
        let job = JobObject::kill_on_close().expect("job");
        let opts = SpawnOptions {
            no_window: true,
            current_dir: Some(tmp.path().to_path_buf()),
            env: vec![(OsString::from("FLOW_BIN"), OsString::from("C:\\x y"))],
        };
        // Relative output paths: the working directory is the tempdir, and
        // a quoted path inside the `/c` string would confuse cmd's parser.
        let script = "echo %FLOW_BIN%> env.txt & cd>> env.txt";
        let child = job
            .spawn(&cmd_exe(), &[OsStr::new("/c"), OsStr::new(script)], &opts)
            .expect("spawn");
        assert_eq!(child.wait(10_000), Some(0));
        let text = std::fs::read_to_string(&out).expect("output file");
        let mut lines = text.lines().map(str::trim);
        assert_eq!(lines.next(), Some("C:\\x y"), "{text}");
        let cwd = lines.next().expect("cd line");
        assert_eq!(
            Path::new(cwd).canonicalize().ok().as_deref(),
            tmp.path().canonicalize().ok().as_deref(),
            "{text}"
        );
    }

    #[test]
    fn environment_block_overrides_case_insensitively_and_is_sorted() {
        let inherited = vec![
            (OsString::from("Path"), OsString::from("C:\\old")),
            (OsString::from("ZZZ"), OsString::from("1")),
            (OsString::from("flow_bin"), OsString::from("stale")),
        ];
        let overrides = vec![(OsString::from("FLOW_BIN"), OsString::from("C:\\flow"))];
        let block = build_environment_block(inherited.into_iter(), &overrides);
        let s = block.to_string_lossy();
        // Sorted by upper-cased name, stale case variant replaced, double NUL.
        assert_eq!(s, "FLOW_BIN=C:\\flow\0Path=C:\\old\0ZZZ=1\0\0");
        // No overrides: inherited only, still terminated.
        let only = build_environment_block(
            vec![(OsString::from("A"), OsString::from("b"))].into_iter(),
            &[],
        );
        assert_eq!(only.to_string_lossy(), "A=b\0\0");
        assert_eq!(
            build_environment_block(std::iter::empty(), &[]).to_string_lossy(),
            "\0\0"
        );
    }
}
