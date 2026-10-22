//! Build script for `flow-wm`.
//!
//! This project targets Windows exclusively. The build script panics if the
//! target OS is not Windows, preventing accidental compilation on other
//! platforms. Because `compile_error!` is a macro for the main compilation
//! phase (not build-time), `panic!` in `build.rs` is the correct approach to
//! halt the build early.
//!
//! It also embeds the git state of the checkout being built as `FLOW_BUILD_*`
//! environment variables read by `src/build_info.rs`. Every variable is
//! always set (empty when `git` or a checkout is unavailable), so the crate
//! reads them with `env!` and never fails to build. See
//! `docs/src/dev-guide/updater.md`, *Build metadata*.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        panic!(
            "flow-wm only supports Windows targets. \
             Build with --target x86_64-pc-windows-msvc"
        );
    }
    println!("cargo:rerun-if-changed=build.rs");

    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let info = GitInfo::read(&manifest_dir).unwrap_or_default();
    for path in &info.rerun_paths {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rustc-env=FLOW_BUILD_COMMIT={}", info.commit);
    println!(
        "cargo:rustc-env=FLOW_BUILD_COMMIT_SHORT={}",
        info.commit_short
    );
    println!("cargo:rustc-env=FLOW_BUILD_BRANCH={}", info.branch);
    println!(
        "cargo:rustc-env=FLOW_BUILD_DIRTY={}",
        if info.dirty { "1" } else { "0" }
    );
    println!(
        "cargo:rustc-env=FLOW_BUILD_COMMIT_DATE={}",
        info.commit_date
    );
    println!("cargo:rustc-env=FLOW_BUILD_ORIGIN_URL={}", info.origin_url);
}

/// What `git` reports about the checkout, all empty when unavailable.
#[derive(Default)]
struct GitInfo {
    commit: String,
    commit_short: String,
    branch: String,
    dirty: bool,
    commit_date: String,
    origin_url: String,
    /// Files whose change must re-run this script (HEAD, the branch ref, the
    /// index), so a new commit or checkout is picked up by the next build.
    rerun_paths: Vec<PathBuf>,
}

impl GitInfo {
    /// `None` when `git` is missing or `dir` is not inside a work tree.
    fn read(dir: &Path) -> Option<Self> {
        // The first call doubles as the "is this a checkout" probe.
        let commit = git(dir, &["rev-parse", "HEAD"])?;
        if commit.is_empty() {
            return None;
        }
        let commit_short = git(dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        // A detached HEAD answers "HEAD": report no branch rather than that.
        let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
            .filter(|b| b != "HEAD")
            .unwrap_or_default();
        // Tracked files only: an untracked scratch file is not a dirty build.
        let dirty = git(dir, &["status", "--porcelain", "--untracked-files=no"])
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        // Committer date, short ISO (YYYY-MM-DD).
        let commit_date = git(dir, &["log", "-1", "--format=%cs"]).unwrap_or_default();
        let origin_url = git(dir, &["config", "--get", "remote.origin.url"]).unwrap_or_default();

        let mut rerun_paths = Vec::new();
        for name in ["HEAD", "index", "packed-refs"] {
            if let Some(p) = git_path(dir, name) {
                rerun_paths.push(p);
            }
        }
        if let Some(full) = git(dir, &["rev-parse", "--symbolic-full-name", "HEAD"])
            && full.starts_with("refs/")
            && let Some(p) = git_path(dir, &full)
        {
            rerun_paths.push(p);
        }

        Some(Self {
            commit,
            commit_short,
            branch,
            dirty,
            commit_date,
            origin_url,
            rerun_paths,
        })
    }
}

/// Run `git <args>` in `dir`; `None` on any failure, else trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Absolute path of a file inside the git dir (`git rev-parse --git-path`
/// handles work trees, where `.git` is a file), if that file exists.
fn git_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let rel = git(dir, &["rev-parse", "--git-path", name])?;
    let p = PathBuf::from(rel);
    let abs = if p.is_absolute() { p } else { dir.join(p) };
    abs.exists().then_some(abs)
}
