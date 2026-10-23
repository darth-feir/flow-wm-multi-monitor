//! `[startup]` — programs the daemon runs once it is ready, and the program
//! lookup the daemon and the keymap share for a bare name.
//!
//! The lookup never searches the working directory: `CreateProcess` would
//! (before `PATH`), and ADR-0012 closed that door for `flow` itself. A bare
//! name is tried in the per-machine and per-user program folders, the
//! Store's execution aliases and then each `PATH` entry; a path is taken as
//! given only when it is absolute.
//! (`docs/src/user-guide/config-reference.md`, *`[startup]`*)

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `[startup]`: what to run after the daemon reaches `ready`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StartupConfig {
    /// Programs to run once, each as `["program", "arg", ...]`, in order.
    /// A bare name is looked up like a `[binds]` spawn; a failure to start
    /// one is logged and the rest still run.
    pub spawn: Vec<Vec<String>>,
}

impl StartupConfig {
    /// Every entry names a program.
    ///
    /// # Errors
    ///
    /// The index of the first empty entry.
    pub fn validate(&self) -> Result<(), String> {
        for (i, argv) in self.spawn.iter().enumerate() {
            if argv.first().is_none_or(|p| p.trim().is_empty()) {
                return Err(format!(
                    "startup.spawn[{i}] needs at least the program name"
                ));
            }
        }
        Ok(())
    }
}

/// Where a bare program name is looked for, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramDirs {
    /// `%ProgramFiles%` and `%ProgramFiles(x86)%`: `<dir>\<name>\<name>.exe`.
    pub program_files: Vec<PathBuf>,
    /// `%LOCALAPPDATA%\Programs`: `<dir>\<name>\<name>.exe`.
    pub user_programs: Option<PathBuf>,
    /// `%LOCALAPPDATA%\Microsoft\WindowsApps`: the Store's execution aliases, `<dir>\<name>.exe`.
    pub windows_apps: Option<PathBuf>,
    /// `PATH`, split: `<dir>\<name>.exe` then `<dir>\<name>`.
    pub path: Vec<PathBuf>,
}

impl ProgramDirs {
    /// The folders of the running process' environment.
    #[must_use]
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        let local = var("LOCALAPPDATA");
        Self {
            program_files: ["ProgramFiles", "ProgramFiles(x86)"]
                .iter()
                .filter_map(|k| var(k))
                .collect(),
            user_programs: local.as_ref().map(|l| l.join("Programs")),
            windows_apps: local
                .as_ref()
                .map(|l| l.join("Microsoft").join("WindowsApps")),
            path: std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
        }
    }
}

/// Resolve `name` to the file to run. `exists` answers whether a candidate
/// is a file (injected so the rule is testable without a disk).
///
/// # Errors
///
/// A relative path (only absolute paths are taken as given), or a bare name
/// found nowhere, with the folders that were tried.
pub fn resolve_program_with(
    name: &str,
    dirs: &ProgramDirs,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty program name".into());
    }
    let is_path = name.contains(['\\', '/']) || name.contains(':');
    if is_path {
        let p = Path::new(name);
        return if p.is_absolute() {
            Ok(p.to_path_buf())
        } else {
            Err(format!(
                "\"{name}\": a program path must be absolute (a bare name is looked up, a relative path is not)"
            ))
        };
    }
    let stem = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name);
    let exe = format!("{stem}.exe");
    let mut candidates: Vec<PathBuf> = Vec::new();
    for pf in &dirs.program_files {
        candidates.push(pf.join(stem).join(&exe));
    }
    if let Some(up) = &dirs.user_programs {
        candidates.push(up.join(stem).join(&exe));
    }
    if let Some(wa) = &dirs.windows_apps {
        candidates.push(wa.join(&exe));
    }
    for dir in &dirs.path {
        candidates.push(dir.join(&exe));
        candidates.push(dir.join(stem));
    }
    if let Some(found) = candidates.iter().find(|c| exists(c)) {
        return Ok(found.clone());
    }
    Err(format!(
        "\"{name}\" not found in Program Files, %LOCALAPPDATA%\\Programs, the Store's execution aliases or PATH"
    ))
}

/// [`resolve_program_with`] against the real disk and environment.
///
/// # Errors
///
/// As [`resolve_program_with`].
pub fn resolve_program(name: &str) -> Result<PathBuf, String> {
    resolve_program_with(name, &ProgramDirs::from_env(), &|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> ProgramDirs {
        ProgramDirs {
            program_files: vec![PathBuf::from(r"C:\PF"), PathBuf::from(r"C:\PF86")],
            user_programs: Some(PathBuf::from(r"C:\U\Programs")),
            windows_apps: Some(PathBuf::from(r"C:\U\WindowsApps")),
            path: vec![PathBuf::from(r"C:\Windows"), PathBuf::from(r"C:\Tools")],
        }
    }

    #[test]
    fn a_bare_name_is_looked_up_in_order_never_in_the_cwd() {
        let on_disk = |files: &'static [&'static str]| {
            move |p: &Path| files.iter().any(|f| Path::new(f) == p)
        };
        assert_eq!(
            resolve_program_with(
                "alacritty",
                &dirs(),
                &on_disk(&[
                    r"C:\PF\alacritty\alacritty.exe",
                    r"C:\U\Programs\alacritty\alacritty.exe"
                ])
            )
            .unwrap(),
            PathBuf::from(r"C:\PF\alacritty\alacritty.exe")
        );
        assert_eq!(
            resolve_program_with(
                "Alacritty.exe",
                &dirs(),
                &on_disk(&[r"C:\U\Programs\Alacritty\Alacritty.exe"])
            )
            .unwrap(),
            PathBuf::from(r"C:\U\Programs\Alacritty\Alacritty.exe")
        );
        assert_eq!(
            resolve_program_with("wt", &dirs(), &on_disk(&[r"C:\U\WindowsApps\wt.exe"])).unwrap(),
            PathBuf::from(r"C:\U\WindowsApps\wt.exe")
        );
        assert_eq!(
            resolve_program_with("notepad", &dirs(), &on_disk(&[r"C:\Windows\notepad.exe"]))
                .unwrap(),
            PathBuf::from(r"C:\Windows\notepad.exe")
        );
        assert_eq!(
            resolve_program_with("tool", &dirs(), &on_disk(&[r"C:\Tools\tool"])).unwrap(),
            PathBuf::from(r"C:\Tools\tool")
        );
        // Present only in the working directory: not found.
        let err = resolve_program_with("evil", &dirs(), &on_disk(&[r"evil.exe"])).unwrap_err();
        assert!(err.contains("\"evil\" not found"), "{err}");
    }

    #[test]
    fn paths_are_taken_only_when_absolute() {
        let never = |_: &Path| false;
        assert_eq!(
            resolve_program_with(r"C:\Apps\x.exe", &dirs(), &never).unwrap(),
            PathBuf::from(r"C:\Apps\x.exe")
        );
        let err = resolve_program_with(r"bin\x.exe", &dirs(), &never).unwrap_err();
        assert!(err.contains("must be absolute"), "{err}");
        let err = resolve_program_with(r".\x.exe", &dirs(), &never).unwrap_err();
        assert!(err.contains("must be absolute"), "{err}");
        assert!(resolve_program_with("  ", &dirs(), &never).is_err());
    }

    #[test]
    fn startup_entries_need_a_program() {
        let ok: StartupConfig =
            toml::from_str("spawn = [[\"alacritty\"], [\"wt\", \"-p\", \"PowerShell\"]]").unwrap();
        assert!(ok.validate().is_ok());
        assert_eq!(ok.spawn.len(), 2);
        let bad: StartupConfig = toml::from_str("spawn = [[\"alacritty\"], []]").unwrap();
        assert_eq!(
            bad.validate().unwrap_err(),
            "startup.spawn[1] needs at least the program name"
        );
        let blank: StartupConfig = toml::from_str("spawn = [[\" \"]]").unwrap();
        assert!(blank.validate().is_err());
        assert!(StartupConfig::default().validate().is_ok());
        assert!(toml::from_str::<StartupConfig>("run = []").is_err());
    }
}
