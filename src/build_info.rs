//! What this binary was built from: crate version plus the git state the
//! build script embedded (`build.rs`, `FLOW_BUILD_*`).
//!
//! `flow --version` / `flowd --version` print [`version_line`], and the
//! update checker (`crate::updater`) compares [`COMMIT`] against the remote
//! branch and derives the default repository from [`ORIGIN_URL`]. Every git
//! field is the empty string when the source was not a git checkout (a
//! crates.io tarball) or `git` was not installed at build time; callers must
//! treat empty as "unknown".

use std::sync::LazyLock;

/// Crate version (`CARGO_PKG_VERSION`), e.g. `0.2.0`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Full commit hash of `HEAD` at build time, or empty.
pub const COMMIT: &str = env!("FLOW_BUILD_COMMIT");

/// Abbreviated commit hash (`git rev-parse --short`), or empty.
pub const COMMIT_SHORT: &str = env!("FLOW_BUILD_COMMIT_SHORT");

/// Branch checked out at build time; empty for a detached `HEAD` or no
/// checkout.
pub const BRANCH: &str = env!("FLOW_BUILD_BRANCH");

/// Whether tracked files had uncommitted changes at build time.
pub const DIRTY: bool = {
    let flag = env!("FLOW_BUILD_DIRTY").as_bytes();
    !flag.is_empty() && flag[0] == b'1'
};

/// Committer date of `HEAD` as `YYYY-MM-DD`, or empty.
pub const COMMIT_DATE: &str = env!("FLOW_BUILD_COMMIT_DATE");

/// `remote.origin.url` of the checkout, or empty.
pub const ORIGIN_URL: &str = env!("FLOW_BUILD_ORIGIN_URL");

/// Whether the build carries a commit hash at all.
pub fn has_commit() -> bool {
    !COMMIT.is_empty()
}

/// The commit for display: the short hash, `-dirty` appended when the tree
/// had uncommitted changes; empty when unknown.
pub fn commit_label() -> String {
    commit_label_from(COMMIT_SHORT, DIRTY)
}

/// The `--version` payload after the binary name: `0.2.0 (feat/multi-monitor
/// @ cf54b88, 2026-10-22)`, or just `0.2.0` for a build without git state.
pub fn version_line() -> String {
    version_line_from(VERSION, BRANCH, &commit_label(), COMMIT_DATE)
}

/// [`version_line`] with a static lifetime, for clap's `version` attribute
/// (computed once, on first use).
pub fn version_line_static() -> &'static str {
    static LINE: LazyLock<String> = LazyLock::new(version_line);
    LINE.as_str()
}

/// `"owner/name"` when `url` is a github.com remote in any of git's usual
/// spellings — `https://github.com/owner/name(.git)`,
/// `git@github.com:owner/name(.git)`, `ssh://git@github.com/owner/name.git`,
/// `git://github.com/owner/name` — else `None`.
pub fn github_repo_from_url(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = if let Some(r) = url.strip_prefix("git@github.com:") {
        r
    } else {
        // Scheme form: strip `scheme://`, an optional `user@`, then the host.
        let (_, after_scheme) = url.split_once("://")?;
        let (authority, path) = after_scheme.split_once('/')?;
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        // A port (`github.com:22`) is still github.com.
        let host = host.split_once(':').map_or(host, |(h, _)| h);
        if !host.eq_ignore_ascii_case("github.com") {
            return None;
        }
        path
    };
    let rest = rest.trim_start_matches('/').trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

fn commit_label_from(short: &str, dirty: bool) -> String {
    match (short, dirty) {
        ("", _) => String::new(),
        (s, true) => format!("{s}-dirty"),
        (s, false) => s.to_owned(),
    }
}

fn version_line_from(version: &str, branch: &str, commit: &str, date: &str) -> String {
    if commit.is_empty() {
        return version.to_owned();
    }
    let mut inner = String::new();
    if !branch.is_empty() {
        inner.push_str(branch);
        inner.push_str(" @ ");
    }
    inner.push_str(commit);
    if !date.is_empty() {
        inner.push_str(", ");
        inner.push_str(date);
    }
    format!("{version} ({inner})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_repo_parses_https_forms() {
        assert_eq!(
            github_repo_from_url("https://github.com/CCpcalvin/flow-wm"),
            Some("CCpcalvin/flow-wm".into())
        );
        assert_eq!(
            github_repo_from_url("https://github.com/CCpcalvin/flow-wm.git"),
            Some("CCpcalvin/flow-wm".into())
        );
        assert_eq!(
            github_repo_from_url("https://github.com/owner/name/"),
            Some("owner/name".into())
        );
        assert_eq!(
            github_repo_from_url("http://GitHub.com/owner/name.git\n"),
            Some("owner/name".into())
        );
        assert_eq!(
            github_repo_from_url("https://user@github.com/owner/name.git"),
            Some("owner/name".into())
        );
    }

    #[test]
    fn github_repo_parses_ssh_forms() {
        assert_eq!(
            github_repo_from_url("git@github.com:owner/name.git"),
            Some("owner/name".into())
        );
        assert_eq!(
            github_repo_from_url("git@github.com:owner/name"),
            Some("owner/name".into())
        );
        assert_eq!(
            github_repo_from_url("ssh://git@github.com/owner/name.git"),
            Some("owner/name".into())
        );
        assert_eq!(
            github_repo_from_url("ssh://git@github.com:22/owner/name.git"),
            Some("owner/name".into())
        );
        assert_eq!(
            github_repo_from_url("git://github.com/owner/name.git"),
            Some("owner/name".into())
        );
    }

    #[test]
    fn github_repo_rejects_other_hosts_and_shapes() {
        assert_eq!(github_repo_from_url(""), None);
        assert_eq!(
            github_repo_from_url("https://gitlab.com/owner/name.git"),
            None
        );
        assert_eq!(github_repo_from_url("git@gitlab.com:owner/name.git"), None);
        assert_eq!(github_repo_from_url("https://github.com/owner"), None);
        assert_eq!(github_repo_from_url("https://github.com/owner/"), None);
        assert_eq!(github_repo_from_url("https://github.com//name"), None);
        assert_eq!(github_repo_from_url("https://github.com/a/b/c"), None);
        assert_eq!(github_repo_from_url("C:\\src\\flow-wm"), None);
        assert_eq!(github_repo_from_url("owner/name"), None);
    }

    #[test]
    fn version_line_shapes() {
        assert_eq!(
            version_line_from("0.2.0", "feat/multi-monitor", "cf54b88", "2026-10-22"),
            "0.2.0 (feat/multi-monitor @ cf54b88, 2026-10-22)"
        );
        // Detached HEAD: no branch.
        assert_eq!(
            version_line_from("0.2.0", "", "cf54b88", "2026-10-22"),
            "0.2.0 (cf54b88, 2026-10-22)"
        );
        // No date (odd, but must not print a dangling comma).
        assert_eq!(
            version_line_from("0.2.0", "main", "cf54b88", ""),
            "0.2.0 (main @ cf54b88)"
        );
        // Not a git checkout: bare version, whatever else is set.
        assert_eq!(version_line_from("0.2.0", "", "", ""), "0.2.0");
        assert_eq!(
            version_line_from("0.2.0", "main", "", "2026-10-22"),
            "0.2.0"
        );
    }

    #[test]
    fn commit_label_marks_dirty_trees() {
        assert_eq!(commit_label_from("cf54b88", false), "cf54b88");
        assert_eq!(commit_label_from("cf54b88", true), "cf54b88-dirty");
        assert_eq!(commit_label_from("", true), "");
    }

    #[test]
    fn embedded_constants_are_consistent() {
        // The build script sets every variable; a commit implies a short
        // form, which is a prefix of the full hash.
        if has_commit() {
            assert!(!COMMIT_SHORT.is_empty());
            assert!(COMMIT.starts_with(COMMIT_SHORT));
            assert!(COMMIT.chars().all(|c| c.is_ascii_hexdigit()));
        } else {
            assert!(COMMIT_SHORT.is_empty());
        }
        assert!(version_line().starts_with(VERSION));
    }
}
