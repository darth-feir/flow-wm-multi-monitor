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
//! Processes a job member creates inherit the job by default (unless they
//! break away, which needs `JOB_OBJECT_LIMIT_BREAKAWAY_OK` and we do not set
//! it), so a launcher that spawns the real interpreter and exits still leaves
//! the interpreter inside the job.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessW, PROCESS_CREATION_FLAGS,
    PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

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
    /// to it is closed.
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
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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
    /// instruction and every child it spawns inherits the job.
    ///
    /// `no_window` adds `CREATE_NO_WINDOW` (console children only). The
    /// returned [`JobChild`] owns the process handle.
    ///
    /// # Errors
    ///
    /// Returns a string on `CreateProcessW` failure. A failure to assign the
    /// (still suspended) process to the job terminates it and is returned as
    /// an error too, so a child can never run outside the job by accident.
    pub fn spawn(&self, exe: &Path, args: &[&OsStr], no_window: bool) -> Result<JobChild, String> {
        let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut cmdline_w: Vec<u16> = OsStr::new(&build_command_line(exe, args))
            .encode_wide()
            .chain(Some(0))
            .collect();

        let mut flags: PROCESS_CREATION_FLAGS = CREATE_SUSPENDED;
        if no_window {
            flags |= CREATE_NO_WINDOW;
        }
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();

        // SAFETY: `exe_w` and `cmdline_w` are NUL-terminated and outlive the
        // call; `cmdline_w` is mutable as CreateProcessW requires; `startup`
        // has its `cb` set; `pi` receives the handles.
        unsafe {
            CreateProcessW(
                PCWSTR::from_raw(exe_w.as_ptr()),
                Some(PWSTR::from_raw(cmdline_w.as_mut_ptr())),
                None,
                None,
                false,
                flags,
                None,
                PCWSTR::null(),
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
            let _ = unsafe { windows::Win32::System::Threading::TerminateProcess(pi.hProcess, 1) };
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

    #[test]
    fn job_child_runs_inside_job_and_exits() {
        // Smoke test: create a job, spawn `cmd /c exit 7` inside it, and
        // observe the child exit with that code.
        let job = JobObject::kill_on_close().expect("job");
        let child = job
            .spawn(
                &cmd_exe(),
                &[OsStr::new("/c"), OsStr::new("exit"), OsStr::new("7")],
                true,
            )
            .expect("spawn cmd in job");
        assert!(child.pid() != 0);
        assert_eq!(child.wait(10_000), Some(7), "child should exit with 7");
        assert!(!child.is_running());
    }

    #[test]
    fn closing_the_job_kills_its_members() {
        // A child that would run for a minute is terminated as soon as the
        // last job handle is closed — the property the AHK tie-in relies on.
        let job = JobObject::kill_on_close().expect("job");
        let child = job
            .spawn(
                &cmd_exe(),
                &[
                    OsStr::new("/c"),
                    OsStr::new("ping"),
                    OsStr::new("-n"),
                    OsStr::new("60"),
                    OsStr::new("127.0.0.1"),
                    OsStr::new(">nul"),
                ],
                true,
            )
            .expect("spawn long-running cmd in job");
        assert!(child.is_running(), "child should still be running");
        drop(job);
        assert!(
            child.wait(5_000).is_some(),
            "child must be killed when the job handle is closed"
        );
    }
}
