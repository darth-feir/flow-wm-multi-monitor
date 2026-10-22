//! Detached PowerShell shim that finishes the binary swap after `flow
//! update` exits. See (`docs/src/dev-guide/updater.md`) for why this is
//! needed.
//!
//! # The Windows lock problem
//!
//! Windows holds an exclusive lock on any running `.exe`. `flow.exe` can't
//! overwrite itself. But it CAN rename itself — and the renamed file becomes
//! deletable once the process exits. So we spawn a detached PowerShell script
//! that waits for `flow.exe` to exit, then renames the old binary and moves
//! the staged replacement into place.

use std::path::{Path, PathBuf};

use crate::updater::UpdateError;

/// Build the PowerShell swap script as a self-contained string.
///
/// Parameters (`install_dir`, `flow_pid`) are baked into the script body to
/// keep the spawn surface small (no CLI arg quoting concerns). The script:
///
/// 1. Polls `Get-Process -Id <pid>` until the process is gone or 30s pass.
/// 2. For each of `flow.exe` and `flowd.exe`: renames the running file to
///    `.old` (with retries to let Windows release the handle), then moves
///    `.stage\<name>` into place.
/// 3. Cleans up `.old` files and the `.stage/` directory.
/// 4. Appends a timestamped log to `%TEMP%\flow-update-shim.log`.
/// 5. Self-deletes the `.ps1` file.
pub fn build_shim_script(install_dir: &Path, flow_pid: u32) -> String {
    // Single-quote-escape any single quotes in the path for PowerShell safety.
    let install = install_dir.display().to_string().replace('\'', "''");

    // PowerShell single-quoted-string body. Double-brace the literal `{}`/`{name}`
    // so Rust's format!() leaves them alone for PowerShell to interpolate.
    format!(
        r#"
$ErrorActionPreference = 'Stop'
$InstallDir = '{install}'
$FlowPid = {flow_pid}
$Log = Join-Path $env:TEMP 'flow-update-shim.log'

function Write-Log($msg) {{
    $stamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss.fff'
    "$stamp [pid=$FlowPid] $msg" | Out-File -Append -Encoding utf8 $Log
}}

function Move-WithRetry($src, $dst, $tries) {{
    for ($i = 1; $i -le $tries; $i++) {{
        try {{
            if (Test-Path $src) {{
                Move-Item -Force -ErrorAction Stop $src $dst
            }}
            return $true
        }} catch {{
            Start-Sleep -Milliseconds 500
        }}
    }}
    return $false
}}

try {{
    Write-Log "shim start; install_dir=$InstallDir"

    # 1. Wait up to 30s for the parent flow.exe to exit.
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {{
        $proc = $null
        try {{ $proc = Get-Process -Id $FlowPid -ErrorAction Stop }} catch {{ }}
        if (-not $proc) {{ break }}
        Start-Sleep -Milliseconds 250
    }}
    Write-Log "parent flow.exe process gone (or timeout reached)"

    # 2. Swap each binary.
    foreach ($name in @('flow.exe', 'flowd.exe')) {{
        $target = Join-Path $InstallDir $name
        $staged = Join-Path $InstallDir (Join-Path '.stage' $name)
        $old    = Join-Path $InstallDir ($name + '.old')

        if (-not (Test-Path $staged)) {{
            Write-Log "skip $name (no staged file)"
            continue
        }}

        # Drop stale .old from a prior failed run.
        if (Test-Path $old) {{
            Remove-Item -Force -ErrorAction SilentlyContinue $old
        }}

        # Rename the existing binary out of the way (Windows allows renaming
        # a running .exe; deletion is what's blocked).
        if (Test-Path $target) {{
            $ok = Move-WithRetry $target $old 20
            if (-not $ok) {{
                Write-Log "FAILED to rename $target -> $old after 20 tries; leaving $name in place"
                continue
            }}
            Write-Log "renamed $target -> $old"
        }}

        # Move the staged replacement into the vacated path.
        $ok = Move-WithRetry $staged $target 20
        if ($ok) {{
            Write-Log "swapped $name"
        }} else {{
            Write-Log "FAILED to move staged $staged -> $target"
        }}

        # Clean up the .old file.
        if (Test-Path $old) {{
            Remove-Item -Force -ErrorAction SilentlyContinue $old
        }}
    }}

    # 3. Remove the .stage directory.
    $stageDir = Join-Path $InstallDir '.stage'
    if (Test-Path $stageDir) {{
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $stageDir
        Write-Log "removed $stageDir"
    }}

    Write-Log "shim complete"
}} catch {{
    Write-Log "ERROR: $_"
}} finally {{
    # 4. Self-delete the .ps1 file (best-effort).
    $script = $MyInvocation.MyCommand.Path
    if ($script) {{
        Remove-Item -Force -ErrorAction SilentlyContinue $script
    }}
}}
"#
    )
}

/// The Windows PowerShell host the shim runs in, by absolute path: a
/// `powershell.exe` planted in the current directory or earlier on `PATH`
/// must never be what finishes the update.
#[must_use]
pub fn powershell_path() -> PathBuf {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

/// Write the shim script to disk and spawn it fully detached.
///
/// The script lives at `<install_dir>\.flow-update-shim.ps1` — same volume as
/// the binaries, so `Move-Item` is atomic on NTFS. It is created with
/// `create_new`, so a file already at that path (a leftover shim, or
/// something planted there) fails the update instead of being overwritten
/// or executed in its place. Spawned via [`powershell_path`] with
/// `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` to match the existing
/// `spawn_daemon` pattern in `src/bin/flow.rs` and ensure the script survives
/// the parent `flow.exe` exiting.
///
/// # Errors
///
/// - [`UpdateError::Io`] if the script file can't be created — including
///   `AlreadyExists` when a `.flow-update-shim.ps1` is already there.
/// - [`UpdateError::ShimSpawn`] if spawning `powershell.exe` fails.
pub fn spawn_swap_shim(install_dir: &Path, flow_pid: u32) -> Result<(), UpdateError> {
    use std::io::Write;
    use std::os::windows::process::CommandExt;

    // CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW — matches the existing
    // detached-spawn pattern in src/bin/flow.rs (line ~877).
    const DETACHED: u32 = 0x0000_0200 | 0x0800_0000;

    let script_path = install_dir.join(".flow-update-shim.ps1");
    let script_body = build_shim_script(install_dir, flow_pid);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&script_path)?
        .write_all(script_body.as_bytes())?;

    let mut cmd = std::process::Command::new(powershell_path());
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-WindowStyle",
        "Hidden",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]);
    cmd.arg(&script_path);
    cmd.creation_flags(DETACHED);

    cmd.spawn()
        .map_err(|e| UpdateError::ShimSpawn(e.to_string()))?;

    // Intentionally do NOT wait on the child — it must outlive us so it can
    // swap our binary once we exit.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    // `build_shim_script` is pure string formatting (no process spawn, no
    // disk) so it is the only part of this module that is unit-testable.
    // `spawn_swap_shim` writes to disk and spawns a real `powershell.exe`,
    // so it is out of scope here.

    #[test]
    fn build_shim_script_bakes_in_install_dir_and_pid() {
        // Positive/nominal: the install dir path and the pid must both be
        // interpolated into the generated PowerShell so the detached shim
        // actually watches the right process and swaps the right directory.
        let dir = PathBuf::from(r"C:\Program Files\flow-wm");
        let script = build_shim_script(&dir, 4242u32);

        // Pid is emitted as a bare integer on the $FlowPid assignment line.
        assert!(
            script.contains("$FlowPid = 4242"),
            "pid should be interpolated; got:\n{script}"
        );
        // Install dir is emitted single-quoted.
        assert!(
            script.contains(r"$InstallDir = 'C:\Program Files\flow-wm'"),
            "install dir should be interpolated; got:\n{script}"
        );
        // Functional landmarks that must survive any future reformatting.
        assert!(script.contains("Get-Process"), "missing process poll");
        assert!(script.contains("Move-WithRetry"), "missing retry helper");
        assert!(script.contains("flow-update-shim.log"), "missing log path");
        assert!(
            script.contains("$ErrorActionPreference = 'Stop'"),
            "missing strict-error preamble"
        );
    }

    #[test]
    fn build_shim_script_escapes_single_quotes_in_install_dir() {
        // Positive edge: a single quote in the install path must be doubled
        // (`'` -> `''`) so PowerShell parses the single-quoted string literally.
        // An unescaped quote would terminate the string early and break the
        // swap paths silently.
        let dir = PathBuf::from(r"C:\flow's dir");
        let script = build_shim_script(&dir, 1u32);

        assert!(
            script.contains(r"$InstallDir = 'C:\flow''s dir'"),
            "single quote should be doubled for PowerShell; got:\n{script}"
        );
        // And the raw unescaped quote must NOT appear in that assignment.
        let line = script
            .lines()
            .find(|l| l.contains("$InstallDir ="))
            .expect("$InstallDir assignment must exist");
        assert!(
            !line.contains(r"'C:\flow's dir'"),
            "unescaped single quote leaked into: {line}"
        );
    }

    #[test]
    fn build_shim_script_targets_both_binaries_and_stage_dir() {
        // Positive: the swap loop must touch both co-located binaries and
        // read each replacement from `.stage\`. If either name or the stage
        // path were dropped, the update would silently no-op for one binary.
        let dir = Path::new(r"C:\flow");
        let script = build_shim_script(dir, 100u32);

        assert!(
            script.contains("@('flow.exe', 'flowd.exe')"),
            "swap loop must enumerate both binaries; got:\n{script}"
        );
        assert!(
            script.contains("'.stage'"),
            "staged-file lookup must reference .stage; got:\n{script}"
        );
        assert!(
            script.contains(".flow-update-shim.ps1") || script.contains("$MyInvocation"),
            "shim should self-delete on completion"
        );
    }

    #[test]
    fn powershell_path_is_absolute_under_system_root() {
        // The shim host is never resolved through PATH or the cwd.
        let p = powershell_path();
        assert!(p.is_absolute(), "{p:?}");
        assert!(
            p.ends_with(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
            "{p:?}"
        );
        assert!(p.is_file(), "Windows PowerShell must exist at {p:?}");
    }

    #[test]
    fn spawn_swap_shim_refuses_an_existing_shim_file() {
        // A `.flow-update-shim.ps1` already in the install dir must fail the
        // update (AlreadyExists) rather than be overwritten or run as-is.
        let dir = tempfile::tempdir().expect("tempdir");
        let planted = dir.path().join(".flow-update-shim.ps1");
        std::fs::write(&planted, "Write-Host planted").unwrap();
        let err = spawn_swap_shim(dir.path(), 1).expect_err("must refuse");
        match err {
            UpdateError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists),
            other => panic!("expected an AlreadyExists I/O error, got {other}"),
        }
        assert_eq!(
            std::fs::read_to_string(&planted).unwrap(),
            "Write-Host planted",
            "the planted file must be left untouched"
        );
    }

    #[test]
    fn build_shim_script_accepts_pid_zero() {
        // Edge: pid = 0 is nonsensical at runtime but must not panic the
        // formatter (formatting never divides or indexes by pid). The
        // generated script should still be well-formed.
        let script = build_shim_script(Path::new(r"C:\flow"), 0u32);
        assert!(
            script.contains("$FlowPid = 0"),
            "pid zero should be interpolated verbatim; got:\n{script}"
        );
        // The 30s deadline + retry count are independent of pid and should
        // still be present.
        assert!(
            script.contains("AddSeconds(30)"),
            "deadline constant missing; got:\n{script}"
        );
    }
}
