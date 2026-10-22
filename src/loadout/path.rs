//! Where a loadout file may live.
//!
//! Two pure rules shared by the CLI (a bad path is refused before anything
//! is sent) and the daemon (which trusts nothing that arrives over the
//! pipe): [`validate_default_path`] for the `[loadout] default_path` config
//! key (a plain file name) and [`validate_loadout_path`] for an explicit
//! `flow loadout save|load <path>` (`.json`, no `..`). See
//! `docs/src/dev-guide/loadout.md`, *Where the file may be*.

use std::path::{Component, Path};

/// Check a `[loadout] default_path` value: a bare file name, nothing else.
///
/// # Errors
///
/// Returns a message naming the offending property: empty, a `.`/`..`
/// component, a path separator, or a `:` (drive letter or NTFS stream).
pub fn validate_default_path(name: &str) -> Result<(), String> {
    const KEY: &str = "[loadout] default_path";
    if name.trim().is_empty() {
        return Err(format!("{KEY} must not be empty"));
    }
    if name == "." || name == ".." {
        return Err(format!("{KEY} must be a file name, got {name:?}"));
    }
    if name.contains(['/', '\\']) {
        return Err(format!(
            "{KEY} must be a plain file name inside the config directory (no path separators), got {name:?}"
        ));
    }
    if name.contains(':') {
        return Err(format!(
            "{KEY} must be a plain file name (no drive letter or stream), got {name:?}"
        ));
    }
    Ok(())
}

/// Check an explicit loadout path from the CLI or the pipe: `.json`
/// extension (any case) and no `..` component. Absolute paths pass.
///
/// # Errors
///
/// Returns a message naming the rule that failed.
pub fn validate_loadout_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("loadout path must not be empty".to_owned());
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "loadout path must not contain '..' components, got {}",
            path.display()
        ));
    }
    let is_json = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
    if !is_json {
        return Err(format!(
            "loadout path must name a .json file, got {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn default_path_accepts_a_plain_file_name() {
        assert_eq!(validate_default_path("loadout.json"), Ok(()));
        assert_eq!(validate_default_path("my-loadout.json"), Ok(()));
        assert_eq!(validate_default_path("state.v2"), Ok(()));
    }

    #[test]
    fn default_path_rejects_directories_and_traversal() {
        for bad in [
            "",
            "  ",
            ".",
            "..",
            "../loadout.json",
            "..\\loadout.json",
            "sub/loadout.json",
            "sub\\loadout.json",
            "C:\\Users\\me\\loadout.json",
            "\\\\server\\share\\x.json",
            "/etc/x.json",
            "loadout.json:stream",
        ] {
            let err = validate_default_path(bad).expect_err(bad);
            assert!(err.contains("[loadout] default_path"), "{err}");
        }
    }

    #[test]
    fn loadout_path_requires_json_and_no_parent_components() {
        assert_eq!(validate_loadout_path(Path::new("work.json")), Ok(()));
        assert_eq!(validate_loadout_path(Path::new("Work.JSON")), Ok(()));
        assert_eq!(
            validate_loadout_path(Path::new("C:\\Users\\me\\layouts\\work.json")),
            Ok(())
        );
        assert_eq!(validate_loadout_path(Path::new("./sub/work.json")), Ok(()));

        let err = validate_loadout_path(Path::new("..\\evil.json")).unwrap_err();
        assert!(err.contains("'..'"), "{err}");
        let err = validate_loadout_path(Path::new("C:\\a\\..\\b.json")).unwrap_err();
        assert!(err.contains("'..'"), "{err}");
        let err = validate_loadout_path(Path::new("sub/../x.json")).unwrap_err();
        assert!(err.contains("'..'"), "{err}");

        let err = validate_loadout_path(Path::new("flow.toml")).unwrap_err();
        assert!(err.contains(".json"), "{err}");
        let err = validate_loadout_path(Path::new("C:\\Windows\\System32\\drivers\\etc\\hosts"))
            .unwrap_err();
        assert!(err.contains(".json"), "{err}");
        let err = validate_loadout_path(Path::new("noext")).unwrap_err();
        assert!(err.contains(".json"), "{err}");
        assert!(validate_loadout_path(&PathBuf::new()).is_err());
        // A `..` hidden in the file name itself is just a name.
        assert_eq!(validate_loadout_path(Path::new("a..b.json")), Ok(()));
    }
}
