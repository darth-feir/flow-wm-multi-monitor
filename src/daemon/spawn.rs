//! `[startup] spawn`: programs the daemon starts once it is ready.
//!
//! Each entry is resolved like a `[binds]` spawn (never through the working
//! directory, see `config::startup`) and started from the user's home
//! folder. A failure is logged and the rest still run; nothing here can
//! stop the daemon. (`docs/src/user-guide/config-reference.md`, *`[startup]`*)

use std::path::PathBuf;
use std::process::Command;

use crate::config::startup::resolve_program;

use super::types::FlowWM;

impl FlowWM {
    /// Start every `[startup] spawn` entry, in order, after `ready`.
    pub fn run_startup_spawns(&self) {
        for argv in &self.config.startup.spawn {
            match spawn_program(argv) {
                Ok((path, pid)) => {
                    log::info!("startup: spawned {} (pid {pid})", path.display())
                }
                Err(e) => log::warn!("startup: could not spawn {}: {e}", argv.join(" ")),
            }
        }
    }
}

/// Resolve and start one program; returns what ran and its pid.
fn spawn_program(argv: &[String]) -> Result<(PathBuf, u32), String> {
    let program = argv.first().ok_or("empty entry")?;
    let path = resolve_program(program)?;
    let mut cmd = Command::new(&path);
    cmd.args(&argv[1..]);
    // Start in the home folder, not wherever the daemon was started from
    // (the keymap does the same for the programs it launches).
    if let Some(home) = std::env::var_os("USERPROFILE") {
        cmd.current_dir(home);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((path, child.id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_program_is_an_error_not_a_panic() {
        let err = spawn_program(&["no-such-program-flowwm".to_owned()]).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        assert!(spawn_program(&[]).is_err());
    }

    #[test]
    fn an_absolute_path_runs_with_its_arguments() {
        let cmd =
            std::env::var("ComSpec").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let (path, pid) = spawn_program(&[cmd.clone(), "/c".into(), "exit".into()]).unwrap();
        assert_eq!(path, PathBuf::from(cmd));
        assert!(pid > 0);
    }
}
