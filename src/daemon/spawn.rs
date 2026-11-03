//! Starting programs: `[startup] spawn` entries and the `spawn` IPC command
//! (`flow spawn`, the keymap's spawn binds).
//!
//! Each program is resolved like a `[binds]` spawn (never through the
//! working directory, see `config::startup`) and started from the user's
//! home folder. An `.exe` is started with the rect its new column will get
//! on the focused screen in its startup info, so that its first window opens
//! there instead of where Windows would put it (ADR-0023, *Spawning on the
//! focused screen* and *Appearing in place*).
//! A failure is logged (and answered, for the IPC command); nothing here can
//! stop the daemon. (`docs/src/user-guide/config-reference.md`,
//! *`[startup]`*)

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CreateProcessW, PROCESS_CREATION_FLAGS,
    PROCESS_INFORMATION, STARTF_USEPOSITION, STARTF_USESIZE, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;
use windows::core::{PCWSTR, PWSTR};

use crate::common::Rect;
use crate::config::startup::resolve_program;
use crate::config::types::NewWindowMonitor;
use crate::ipc::SocketResponse;
use crate::job::build_command_line;

use super::types::FlowWM;

/// How a child is created: a console program gets a visible console of
/// its own (the daemon's console is hidden); the unit tests start theirs
/// without a window, so a test run never flashes one on the desktop.
const CHILD_FLAGS: PROCESS_CREATION_FLAGS = if cfg!(test) {
    CREATE_NO_WINDOW
} else {
    CREATE_NEW_CONSOLE
};

/// A started program: what ran, its pid, and the rect its first window
/// was asked to open at (`None`: Windows decides).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Started {
    pub path: PathBuf,
    pub pid: u32,
    pub at: Option<Rect>,
}

/// The rect a spawned program's first window is asked to open at on a
/// screen with work area `work_area`: the visible rect the new column will
/// get (`slot`, from
/// [`ScrollingSpace::insert_preview_rect`](crate::workspace::ScrollingSpace::insert_preview_rect))
/// inset by the border ring (`ring` = `thickness - overlap`, as the seat
/// commands a tile) when that slot lies on the work area; else the work
/// area's top-left corner plus `gap`, `column_width` wide and the work
/// area's height less two gaps. The window's invisible borders are unknown
/// before it exists, so no frame translation is applied (ADR-0023,
/// *Appearing in place*).
#[must_use]
pub(super) fn spawn_rect(
    work_area: Rect,
    gap: i32,
    slot: Option<Rect>,
    ring: i32,
    column_width: i32,
) -> Rect {
    let on_screen = |r: &Rect| {
        r.width > 0
            && r.height > 0
            && r.x >= work_area.x
            && r.x + r.width <= work_area.x + work_area.width
            && r.y >= work_area.y
            && r.y + r.height <= work_area.y + work_area.height
    };
    if let Some(slot) = slot.filter(on_screen) {
        let inset = slot.inset(ring);
        return if inset.width > 0 && inset.height > 0 {
            inset
        } else {
            slot
        };
    }
    Rect {
        x: work_area.x + gap,
        y: work_area.y + gap,
        width: column_width.clamp(1, (work_area.width - 2 * gap).max(1)),
        height: (work_area.height - 2 * gap).max(1),
    }
}

/// Whether `path` names an `.exe`, which is started with `CreateProcessW`
/// (and so can take a position). Anything else (a `.bat`, a `.cmd`, an
/// extensionless file found on `PATH`) goes through `std::process::Command`
/// as before, at Windows' default position.
#[must_use]
pub(super) fn is_exe(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
}

impl FlowWM {
    /// Start every `[startup] spawn` entry, in order, after `ready`, each on
    /// the active monitor like a `flow spawn`.
    pub fn run_startup_spawns(&self) {
        for argv in &self.config.startup.spawn {
            let at = self.spawn_position();
            match start_program(argv, at.map(|(r, _)| r)) {
                Ok(s) => log::info!(
                    "startup: spawned {} (pid {}){}",
                    s.path.display(),
                    s.pid,
                    describe_position(s.at, at.map(|p| p.1))
                ),
                Err(e) => log::warn!("startup: could not spawn {}: {e}", argv.join(" ")),
            }
        }
    }

    /// The `spawn` IPC command: start `argv` with its first window on the
    /// focused screen. See [`SocketMessage::Spawn`](crate::ipc::SocketMessage::Spawn).
    pub(super) fn dispatch_spawn(&self, argv: &[String]) -> SocketResponse {
        let at = self.spawn_position();
        match start_program(argv, at.map(|(r, _)| r)) {
            Ok(s) => {
                let monitor = s.at.and(at.map(|p| p.1));
                log::info!(
                    "spawn: started {} (pid {}){}",
                    s.path.display(),
                    s.pid,
                    describe_position(s.at, monitor)
                );
                SocketResponse::Data {
                    payload: serde_json::json!({
                        "pid": s.pid,
                        "path": s.path.display().to_string(),
                        "x": s.at.map(|r| r.x),
                        "y": s.at.map(|r| r.y),
                        "width": s.at.map(|r| r.width),
                        "height": s.at.map(|r| r.height),
                        "monitor": monitor,
                    }),
                }
            }
            Err(e) => {
                log::warn!("spawn: could not start {}: {e}", argv.join(" "));
                SocketResponse::Error {
                    message: format!("could not start {}: {e}", argv.join(" ")),
                }
            }
        }
    }

    /// The rect (and monitor index) a spawned program's first window is
    /// asked to open at: [`spawn_rect`] for the slot a window inserted now
    /// would get in the active monitor's active strip, from a dry run of the
    /// seat ([`insert_preview_rect`](crate::workspace::ScrollingSpace::insert_preview_rect)).
    /// `None` while tiling is off or with `[focus] new_window_monitor =
    /// "placed"`: Windows decides then.
    fn spawn_position(&self) -> Option<(Rect, usize)> {
        if self.tiling_paused || self.config.focus.new_window_monitor != NewWindowMonitor::Focused {
            return None;
        }
        let idx = self.active_monitor;
        let monitor = self.monitors.get(idx)?;
        let space = &monitor.active_workspace().scrolling;
        let column_width = i32::try_from(space.config().column_width).unwrap_or(i32::MAX);
        let ring = i32::try_from(self.config.borders.thickness).unwrap_or(0)
            - i32::try_from(self.config.borders.overlap).unwrap_or(0);
        let rect = spawn_rect(
            monitor.work_area(),
            space.padding().window_gap,
            space.insert_preview_rect(),
            ring,
            column_width,
        );
        Some((rect, idx))
    }
}

/// `" at (x,y) on monitor N, size WxH"`, or `" at the default position"`.
fn describe_position(at: Option<Rect>, monitor: Option<usize>) -> String {
    match (at, monitor) {
        (Some(r), Some(m)) => format!(
            " at ({},{}) on monitor {m}, size {}x{}",
            r.x, r.y, r.width, r.height
        ),
        (Some(r), None) => format!(" at ({},{}), size {}x{}", r.x, r.y, r.width, r.height),
        _ => " at the default position".to_owned(),
    }
}

/// Resolve and start one program, its first window asked to open at the
/// rect `at` when it is an `.exe` ([`is_exe`]).
///
/// # Errors
///
/// An empty `argv`, a program that cannot be resolved, or a failed start.
pub(super) fn start_program(argv: &[String], at: Option<Rect>) -> Result<Started, String> {
    let program = argv.first().ok_or("empty entry")?;
    let path = resolve_program(program)?;
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    if is_exe(&path) {
        let pid = create_process(&path, &argv[1..], home.as_deref(), at)?;
        return Ok(Started { path, pid, at });
    }
    let mut cmd = Command::new(&path);
    cmd.args(&argv[1..]);
    // Start in the home folder, not wherever the daemon was started from
    // (the keymap does the same for the programs it launches).
    if let Some(home) = home {
        cmd.current_dir(home);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Started {
        path,
        pid: child.id(),
        at: None,
    })
}

/// `CreateProcessW` for `exe` with `args` (quoted per the CRT rules), in
/// `cwd`, with `STARTF_USEPOSITION | STARTF_USESIZE` for the rect `at` and
/// [`CHILD_FLAGS`]. Windows applies the position to the first overlapped
/// window created with `CW_USEDEFAULT` as its position, the size to one
/// created with `CW_USEDEFAULT` as its size; an app that asks for its own
/// size keeps it. The child may take the foreground if the daemon may.
/// Both handles are closed.
fn create_process(
    exe: &Path,
    args: &[String],
    cwd: Option<&Path>,
    at: Option<Rect>,
) -> Result<u32, String> {
    let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let arg_refs: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
    let mut cmdline_w: Vec<u16> = OsStr::new(&build_command_line(exe, &arg_refs))
        .encode_wide()
        .chain(Some(0))
        .collect();
    let cwd_w: Option<Vec<u16>> = cwd.map(|d| d.as_os_str().encode_wide().chain(Some(0)).collect());
    let mut startup = STARTUPINFOW {
        cb: u32::try_from(std::mem::size_of::<STARTUPINFOW>()).unwrap_or(0),
        ..Default::default()
    };
    if let Some(r) = at {
        startup.dwFlags |= STARTF_USEPOSITION | STARTF_USESIZE;
        // Screen coordinates left of or above the primary screen are
        // negative; the field is a DWORD that Windows reads back as signed.
        startup.dwX = r.x.cast_unsigned();
        startup.dwY = r.y.cast_unsigned();
        startup.dwXSize = r.width.max(1).cast_unsigned();
        startup.dwYSize = r.height.max(1).cast_unsigned();
    }
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: `exe_w`, `cmdline_w` and `cwd_w` are NUL-terminated and
    // outlive the call; `cmdline_w` is mutable as CreateProcessW requires;
    // `startup` has its `cb` set; `pi` receives the handles.
    unsafe {
        CreateProcessW(
            PCWSTR::from_raw(exe_w.as_ptr()),
            Some(PWSTR::from_raw(cmdline_w.as_mut_ptr())),
            None,
            None,
            false,
            CHILD_FLAGS,
            None,
            cwd_w
                .as_ref()
                .map_or(PCWSTR::null(), |c| PCWSTR::from_raw(c.as_ptr())),
            &startup,
            &mut pi,
        )
    }
    .map_err(|e| format!("{}: {e}", exe.display()))?;
    // SAFETY: a pid from CreateProcessW; failure (the daemon may not set
    // the foreground itself right now) only means Windows decides.
    let _ = unsafe { AllowSetForegroundWindow(pi.dwProcessId) };
    // SAFETY: both handles come from CreateProcessW and are not used again.
    unsafe {
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
    }
    Ok(pi.dwProcessId)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn an_unknown_program_is_an_error_not_a_panic() {
        let err = start_program(&["no-such-program-flowwm".to_owned()], None).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        assert!(start_program(&[], None).is_err());
    }

    #[test]
    fn an_absolute_path_runs_with_its_arguments() {
        let cmd =
            std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let at = r(10, 20, 300, 200);
        let s = start_program(&[cmd.clone(), "/c".into(), "exit".into()], Some(at)).unwrap();
        assert_eq!(s.path, PathBuf::from(cmd));
        assert!(s.pid > 0);
        assert_eq!(s.at, Some(at));
    }

    /// Positive: the slot on the work area, inset by the ring.
    #[test]
    fn the_slot_inset_by_the_ring_when_it_is_on_the_work_area() {
        let wa = r(-1920, 0, 1920, 1032);
        let slot = r(-1904 + 600 + 16, 16, 940, 1000);
        assert_eq!(
            spawn_rect(wa, 16, Some(slot), 2, 600),
            r(-1904 + 600 + 16 + 2, 18, 936, 996)
        );
        // No ring (thickness == overlap): the slot itself.
        assert_eq!(spawn_rect(wa, 16, Some(slot), 0, 600), slot);
    }

    /// Boundary: a slot that sticks out of the work area (a parked or
    /// half-scrolled column) gives the corner, at the base width; a slot
    /// flush with the edge still counts.
    #[test]
    fn a_slot_off_the_work_area_gives_the_corner() {
        let wa = r(0, 0, 1920, 1032);
        let corner = r(16, 16, 960, 1000);
        assert_eq!(
            spawn_rect(wa, 16, Some(r(1500, 16, 960, 1000)), 2, 960),
            corner
        );
        assert_eq!(
            spawn_rect(wa, 16, Some(r(-4000, 16, 960, 1000)), 2, 960),
            corner
        );
        assert_eq!(
            spawn_rect(wa, 16, Some(r(944, 16, 976, 1000)), 0, 960),
            r(944, 16, 976, 1000)
        );
    }

    /// Negative: no slot or an empty one gives the corner; a column wider
    /// than the screen is clamped to it.
    #[test]
    fn no_usable_slot_gives_the_corner() {
        let wa = r(-1920, 0, 1920, 1032);
        assert_eq!(spawn_rect(wa, 8, None, 2, 600), r(-1912, 8, 600, 1016));
        assert_eq!(
            spawn_rect(wa, 8, Some(r(-1900, 8, 0, 0)), 2, 600),
            r(-1912, 8, 600, 1016)
        );
        assert_eq!(spawn_rect(wa, 8, None, 2, 5000), r(-1912, 8, 1904, 1016));
    }

    /// Only an `.exe` (any case) takes the positioned path.
    #[test]
    fn only_an_exe_is_started_with_a_position() {
        assert!(is_exe(Path::new(r"C:\a\alacritty.exe")));
        assert!(is_exe(Path::new(r"C:\a\Tool.EXE")));
        assert!(!is_exe(Path::new(r"C:\a\script.cmd")));
        assert!(!is_exe(Path::new(r"C:\a\tool")));
    }
}
