//! Update **checker** behind `flow update` and the optional `flow start`
//! notice. It asks the GitHub API how this build relates to a branch (by the
//! commit the build script embedded, see [`crate::build_info`]) or to the
//! latest release (by crate version), prints the answer and how to update,
//! and exits with a code that says whether something newer exists. It never
//! downloads or replaces a binary; a source build is updated with `git pull`
//! + `cargo build --release`. See (`docs/src/dev-guide/updater.md`).
//!
//! The network is touched only in [`check`] / [`check_target`]; everything
//! that turns GitHub's JSON into a [`Report`] is a pure function with
//! canned-JSON unit tests.

use std::cmp::Ordering;
use std::time::Duration;

use thiserror::Error;

use crate::build_info;
use crate::config::UpdateConfig;

/// The repository compared against when neither `[update] repo` nor a
/// github.com `origin` URL names one.
pub const UPSTREAM_REPO: &str = "CCpcalvin/flow-wm";

const API: &str = "https://api.github.com";
const USER_AGENT: &str = concat!("flow-wm/", env!("CARGO_PKG_VERSION"));

/// Upper bound on every request. `check` also fires from `flow start` (when
/// `check_for_updates` is on), so a dead network must resolve in seconds.
const TIMEOUT: Duration = Duration::from_secs(5);

/// `flow update` exit code: nothing newer (identical, ahead, same or newer
/// than the latest release, or not newer by date in the fallback).
pub const EXIT_UP_TO_DATE: i32 = 0;
/// `flow update` exit code: something newer exists.
pub const EXIT_NEWER: i32 = 10;
/// `flow update` exit code: api.github.com could not be reached (offline,
/// DNS, TLS, timeout).
pub const EXIT_NETWORK: i32 = 2;
/// `flow update` exit code: the repository, branch or release does not exist
/// (GitHub answered 404), or `[update] repo` is not `owner/name`.
pub const EXIT_NOT_FOUND: i32 = 3;
/// `flow update` exit code: GitHub answered with another error (403 rate
/// limit, 5xx) or something that could not be parsed.
pub const EXIT_GITHUB: i32 = 4;
/// `flow update` exit code: branch mode was asked for but this build has no
/// commit hash embedded (not built from a git checkout).
pub const EXIT_NO_BUILD_INFO: i32 = 5;

/// Why a check could not produce a [`Report`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum UpdateError {
    /// The request never got an HTTP answer.
    #[error("could not reach api.github.com: {0}")]
    Network(String),
    /// GitHub answered 404, or the config names an impossible target.
    #[error("{0}")]
    NotFound(String),
    /// GitHub answered with a non-404 error status.
    #[error("GitHub answered HTTP {status} for {url}{}", rate_limit_hint(*status))]
    Http {
        /// The HTTP status.
        status: u16,
        /// The URL that was asked.
        url: String,
    },
    /// The answer was not the JSON expected.
    #[error("unexpected reply from GitHub: {0}")]
    Malformed(String),
    /// Branch mode without an embedded commit.
    #[error(
        "this build carries no commit hash (not built from a git checkout); \
         set [update] branch = \"\" in flow.toml to compare against releases instead"
    )]
    NoBuildInfo,
}

fn rate_limit_hint(status: u16) -> &'static str {
    if status == 403 || status == 429 {
        " (rate limited? unauthenticated clients get 60 requests per hour)"
    } else {
        ""
    }
}

impl UpdateError {
    /// The `flow update` exit code for this failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            UpdateError::Network(_) => EXIT_NETWORK,
            UpdateError::NotFound(_) => EXIT_NOT_FOUND,
            UpdateError::Http { .. } | UpdateError::Malformed(_) => EXIT_GITHUB,
            UpdateError::NoBuildInfo => EXIT_NO_BUILD_INFO,
        }
    }
}

/// What the check compares this build against, resolved from `[update]`
/// and the build-time fallbacks by [`target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Compare the embedded commit with `branch` of `repo`.
    Branch {
        /// `owner/name`.
        repo: String,
        /// Remote branch name.
        branch: String,
    },
    /// Compare the crate version with the latest release of `repo`.
    Release {
        /// `owner/name`.
        repo: String,
    },
}

impl Target {
    /// The `owner/name` either variant names.
    pub fn repo(&self) -> &str {
        match self {
            Target::Branch { repo, .. } | Target::Release { repo } => repo,
        }
    }
}

/// The outcome of a check, which `flow update` turns into its exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing newer on the remote.
    UpToDate,
    /// Something newer exists.
    Newer,
}

impl Verdict {
    /// [`EXIT_UP_TO_DATE`] or [`EXIT_NEWER`].
    pub fn exit_code(self) -> i32 {
        match self {
            Verdict::UpToDate => EXIT_UP_TO_DATE,
            Verdict::Newer => EXIT_NEWER,
        }
    }
}

/// What a check found, in lines ready for `flow: ` prefixes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Newer or not.
    pub verdict: Verdict,
    /// One line naming what was compared and the result; the `flow start`
    /// notice prints only this.
    pub summary: String,
    /// Further lines (the newest remote commit, the release date, ...).
    pub details: Vec<String>,
}

/// The build being checked, so the JSON-to-report functions stay pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBuild {
    /// Crate version, e.g. `0.2.0`.
    pub version: String,
    /// Full commit hash, empty when unknown.
    pub commit: String,
    /// Short commit hash (with `-dirty` when applicable), empty when unknown.
    pub commit_label: String,
    /// `YYYY-MM-DD` of the commit, empty when unknown.
    pub commit_date: String,
}

impl LocalBuild {
    /// This binary, from [`crate::build_info`].
    pub fn this() -> Self {
        Self {
            version: build_info::VERSION.to_owned(),
            commit: build_info::COMMIT.to_owned(),
            commit_label: build_info::commit_label(),
            commit_date: build_info::COMMIT_DATE.to_owned(),
        }
    }
}

/// Resolve `[update]` against this build's embedded origin and branch.
///
/// # Errors
///
/// `NotFound` when `repo` is given but is not `owner/name`.
pub fn target(cfg: &UpdateConfig) -> Result<Target, UpdateError> {
    resolve_target(cfg, build_info::ORIGIN_URL, build_info::BRANCH)
}

/// [`target`] with the build-time inputs passed in: `repo` from the config,
/// else from a github.com `origin_url`, else [`UPSTREAM_REPO`]; branch mode
/// with the config's `branch`, else `build_branch`; release mode when the
/// config says `branch = ""` or there is no branch at all.
pub fn resolve_target(
    cfg: &UpdateConfig,
    origin_url: &str,
    build_branch: &str,
) -> Result<Target, UpdateError> {
    let repo = match cfg.repo.as_deref().map(str::trim) {
        Some(r) if !r.is_empty() => {
            if !is_repo_slug(r) {
                return Err(UpdateError::NotFound(format!(
                    "[update] repo must be \"owner/name\", got {r:?}"
                )));
            }
            r.to_owned()
        }
        _ => {
            build_info::github_repo_from_url(origin_url).unwrap_or_else(|| UPSTREAM_REPO.to_owned())
        }
    };
    let branch = match cfg.branch.as_deref().map(str::trim) {
        Some(b) => b.to_owned(),
        None => build_branch.trim().to_owned(),
    };
    Ok(if branch.is_empty() {
        Target::Release { repo }
    } else {
        Target::Branch { repo, branch }
    })
}

/// `owner/name`: exactly one `/`, both parts non-empty and made of the
/// characters GitHub allows (letters, digits, `-`, `_`, `.`).
fn is_repo_slug(s: &str) -> bool {
    let ok_part = |p: &str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    matches!(s.split_once('/'), Some((o, n)) if ok_part(o) && ok_part(n))
}

/// Ask GitHub and report. Every request goes through one HTTPS-only agent
/// bounded by [`TIMEOUT`], identified as `flow-wm/<version>`, without a
/// token; the `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` environment is
/// honoured by `ureq` itself.
///
/// Branch mode compares the embedded commit with the branch head. When the
/// commit is unknown to the remote (not pushed yet, or rewritten) GitHub
/// answers 404 and the check falls back to the branch head alone, judged by
/// commit date. Release mode compares the latest release tag with the crate
/// version.
///
/// # Errors
///
/// See [`UpdateError`]; `exit_code` maps each to a `flow update` exit code.
pub fn check(cfg: &UpdateConfig) -> Result<Report, UpdateError> {
    check_target(&target(cfg)?, &LocalBuild::this())
}

/// [`check`] for an already resolved target and build.
pub fn check_target(target: &Target, local: &LocalBuild) -> Result<Report, UpdateError> {
    let agent = https_agent();
    match target {
        Target::Branch { repo, branch } => {
            if local.commit.is_empty() {
                return Err(UpdateError::NoBuildInfo);
            }
            let compare = format!("{API}/repos/{repo}/compare/{}...{branch}", local.commit);
            match get(&agent, &compare) {
                Ok(body) => compare_report(&body, local, repo, branch),
                Err(e) if should_fall_back(&e) => {
                    let head = format!("{API}/repos/{repo}/commits/{branch}");
                    match get(&agent, &head) {
                        Ok(body) => head_report(&body, local, repo, branch),
                        Err(e) => Err(head_not_found(&e, repo, branch).unwrap_or(e)),
                    }
                }
                Err(e) => Err(e),
            }
        }
        Target::Release { repo } => {
            let url = format!("{API}/repos/{repo}/releases/latest");
            match get(&agent, &url) {
                Ok(body) => release_report(&body, local, repo),
                Err(e) if should_fall_back(&e) => Err(UpdateError::NotFound(format!(
                    "{repo} has no releases (or the repository does not exist)"
                ))),
                Err(e) => Err(e),
            }
        }
    }
}

/// Whether a failed compare should be retried against the branch head: only
/// a 404, which is what GitHub answers when the local commit is unknown to
/// it (it answers the same for a missing repo or branch, which the fallback
/// then reports as such).
fn should_fall_back(err: &UpdateError) -> bool {
    matches!(err, UpdateError::Http { status: 404, .. })
}

/// What a failed branch-head request means, when it means "not found":
/// GitHub answers 422 (`No commit found for SHA`) for a branch the
/// repository does not have, and 404 for a repository that does not exist
/// (or is private). Any other failure is `None` and reported as is.
fn head_not_found(err: &UpdateError, repo: &str, branch: &str) -> Option<UpdateError> {
    match err {
        UpdateError::Http { status: 422, .. } => Some(UpdateError::NotFound(format!(
            "{repo} has no branch {branch:?}"
        ))),
        UpdateError::Http { status: 404, .. } => Some(UpdateError::NotFound(format!(
            "repository {repo} not found on GitHub (or private)"
        ))),
        _ => None,
    }
}

/// A ureq agent that only ever speaks HTTPS, with every request bounded by
/// [`TIMEOUT`] and sent as `flow-wm/<version>`.
fn https_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// `GET url` as a string body; a non-2xx status is `Http`, anything that
/// never got a status is `Network`.
fn get(agent: &ureq::Agent, url: &str) -> Result<String, UpdateError> {
    let mut resp = agent
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(status) => UpdateError::Http {
                status,
                url: url.to_owned(),
            },
            other => UpdateError::Network(other.to_string()),
        })?;
    resp.body_mut()
        .read_to_string()
        .map_err(|e| UpdateError::Network(e.to_string()))
}

// --- GitHub JSON shapes (unknown fields ignored) -------------------------

#[derive(Debug, serde::Deserialize)]
struct CompareJson {
    status: String,
    ahead_by: u64,
    behind_by: u64,
    #[serde(default)]
    commits: Vec<CommitJson>,
}

#[derive(Debug, serde::Deserialize)]
struct CommitJson {
    sha: String,
    commit: CommitBodyJson,
}

#[derive(Debug, serde::Deserialize)]
struct CommitBodyJson {
    committer: Option<SignatureJson>,
    author: Option<SignatureJson>,
    #[serde(default)]
    message: String,
}

#[derive(Debug, serde::Deserialize)]
struct SignatureJson {
    #[serde(default)]
    date: String,
}

#[derive(Debug, serde::Deserialize)]
struct ReleaseJson {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    published_at: String,
}

impl CommitJson {
    fn short(&self) -> &str {
        &self.sha[..self.sha.len().min(7)]
    }

    /// `YYYY-MM-DD` of the committer (else author) date.
    fn date(&self) -> &str {
        let sig = self
            .commit
            .committer
            .as_ref()
            .or(self.commit.author.as_ref());
        sig.map_or("", |s| date_only(&s.date))
    }

    fn title(&self) -> &str {
        self.commit.message.lines().next().unwrap_or("").trim()
    }

    /// `a1b2c3d (2026-10-23) fix: something`.
    fn describe(&self) -> String {
        format!("{} ({}) {}", self.short(), self.date(), self.title())
    }
}

/// The `YYYY-MM-DD` part of an ISO-8601 timestamp (or the whole of a shorter
/// string).
fn date_only(ts: &str) -> &str {
    &ts[..ts.len().min(10)]
}

fn parse<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, UpdateError> {
    serde_json::from_str(body).map_err(|e| UpdateError::Malformed(e.to_string()))
}

/// Report from `GET /repos/{repo}/compare/{local}...{branch}`.
///
/// GitHub's `status` is from the head's (the branch's) point of view:
/// `ahead` means the branch has commits this build lacks.
pub fn compare_report(
    body: &str,
    local: &LocalBuild,
    repo: &str,
    branch: &str,
) -> Result<Report, UpdateError> {
    let cmp: CompareJson = parse(body)?;
    let me = &local.commit_label;
    let plural = |n: u64| if n == 1 { "commit" } else { "commits" };
    // For `ahead` and `diverged` the list holds the branch's commits this
    // build lacks, oldest first.
    let newest = cmp
        .commits
        .last()
        .map(|c| format!("newest remote commit: {}", c.describe()));
    let (verdict, summary) = match cmp.status.as_str() {
        "identical" => (
            Verdict::UpToDate,
            format!("up to date: your build {me} is the head of {repo} {branch}"),
        ),
        "ahead" => (
            Verdict::Newer,
            format!(
                "update available: your build {me} is {} {} behind {repo} {branch}",
                cmp.ahead_by,
                plural(cmp.ahead_by)
            ),
        ),
        "behind" => (
            Verdict::UpToDate,
            format!(
                "up to date: your build {me} is {} {} ahead of {repo} {branch} (nothing newer on the remote)",
                cmp.behind_by,
                plural(cmp.behind_by)
            ),
        ),
        "diverged" => (
            Verdict::Newer,
            format!(
                "update available: {repo} {branch} has {} {} your build {me} lacks, and your build has {} {} the branch lacks (diverged)",
                cmp.ahead_by,
                plural(cmp.ahead_by),
                cmp.behind_by,
                plural(cmp.behind_by)
            ),
        ),
        other => {
            return Err(UpdateError::Malformed(format!(
                "unknown compare status {other:?}"
            )));
        }
    };
    let details = match verdict {
        Verdict::Newer => newest.into_iter().collect(),
        Verdict::UpToDate => Vec::new(),
    };
    Ok(Report {
        verdict,
        summary,
        details,
    })
}

/// Report from `GET /repos/{repo}/commits/{branch}` — the fallback when the
/// local commit is unknown to the remote. Newer when the branch head was
/// committed on a later day than this build's commit.
pub fn head_report(
    body: &str,
    local: &LocalBuild,
    repo: &str,
    branch: &str,
) -> Result<Report, UpdateError> {
    let head: CommitJson = parse(body)?;
    let newer = head.date() > local.commit_date.as_str();
    let verdict = if newer {
        Verdict::Newer
    } else {
        Verdict::UpToDate
    };
    let lead = if newer {
        "update available"
    } else {
        "nothing newer by date"
    };
    let local_date = if local.commit_date.is_empty() {
        "an unknown date"
    } else {
        &local.commit_date
    };
    Ok(Report {
        verdict,
        summary: format!(
            "{lead}: {repo} {branch} head is {}, committed {}; your build is {} from {local_date}, not found on the remote (not pushed yet, or rewritten)",
            head.short(),
            head.date(),
            local.commit_label,
        ),
        details: vec![format!("remote head: {}", head.describe())],
    })
}

/// Report from `GET /repos/{repo}/releases/latest`: the tag's semver against
/// the crate version.
pub fn release_report(body: &str, local: &LocalBuild, repo: &str) -> Result<Report, UpdateError> {
    let rel: ReleaseJson = parse(body)?;
    let tag = rel.tag_name.as_str();
    let order = compare_versions(tag, &local.version).ok_or_else(|| {
        UpdateError::Malformed(format!(
            "release tag {tag:?} or version {:?} is not MAJOR.MINOR.PATCH",
            local.version
        ))
    })?;
    let me = &local.version;
    let (verdict, summary) = match order {
        Ordering::Greater => (
            Verdict::Newer,
            format!("update available: release {tag} of {repo} is newer than your {me}"),
        ),
        Ordering::Equal => (
            Verdict::UpToDate,
            format!("up to date: release {tag} is the latest of {repo} and matches your {me}"),
        ),
        Ordering::Less => (
            Verdict::UpToDate,
            format!("up to date: your {me} is newer than the latest release {tag} of {repo}"),
        ),
    };
    let mut details = Vec::new();
    if !rel.published_at.is_empty() {
        details.push(format!("published {}", date_only(&rel.published_at)));
    }
    if !rel.html_url.is_empty() {
        details.push(rel.html_url.clone());
    }
    Ok(Report {
        verdict,
        summary,
        details,
    })
}

/// Parse `"vX.Y.Z"` or `"X.Y.Z"` into a `(major, minor, patch)` triple.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut p = s.split('.');
    let triple = (
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
    );
    p.next().is_none().then_some(triple)
}

/// How `tag` relates to `current` (`Greater` = the tag is newer); `None` if
/// either is not `MAJOR.MINOR.PATCH` (an optional leading `v` is fine).
pub fn compare_versions(tag: &str, current: &str) -> Option<Ordering> {
    Some(parse_version(tag)?.cmp(&parse_version(current)?))
}

/// The lines telling the user how to update, for a `Target`: a source
/// build's pull-and-build recipe, or the release page. `install_dir` is
/// where the running `flow.exe` lives, if known.
pub fn how_to_update(target: &Target, install_dir: Option<&str>) -> Vec<String> {
    let dest = install_dir.map_or_else(
        || "over the installed flow.exe and flowd.exe".to_owned(),
        |d| format!("over flow.exe and flowd.exe in {d}"),
    );
    let recipe = match target {
        Target::Branch { branch, .. } => format!(
            "to update a source build: git pull (branch {branch}), cargo build --release, flow stop, then copy target\\release\\flow.exe and flowd.exe {dest}"
        ),
        Target::Release { repo } => format!(
            "to update: download the release from https://github.com/{repo}/releases/latest, flow stop, then copy its flow.exe and flowd.exe {dest}"
        ),
    };
    vec![
        recipe,
        "flow update never downloads or replaces anything itself".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> LocalBuild {
        LocalBuild {
            version: "0.2.0".into(),
            commit: "cf54b88aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            commit_label: "cf54b88".into(),
            commit_date: "2026-10-22".into(),
        }
    }

    fn cfg(repo: Option<&str>, branch: Option<&str>) -> UpdateConfig {
        UpdateConfig {
            repo: repo.map(str::to_owned),
            branch: branch.map(str::to_owned),
        }
    }

    const ORIGIN: &str = "https://github.com/someone/flow-wm.git";

    fn http(status: u16) -> UpdateError {
        UpdateError::Http {
            status,
            url: "u".into(),
        }
    }

    // --- target resolution ---

    #[test]
    fn target_defaults_to_origin_repo_and_build_branch() {
        assert_eq!(
            resolve_target(&cfg(None, None), ORIGIN, "feat/multi-monitor"),
            Ok(Target::Branch {
                repo: "someone/flow-wm".into(),
                branch: "feat/multi-monitor".into()
            })
        );
    }

    #[test]
    fn target_falls_back_to_upstream_without_github_origin() {
        assert_eq!(
            resolve_target(&cfg(None, None), "", "main").unwrap().repo(),
            UPSTREAM_REPO
        );
        assert_eq!(
            resolve_target(&cfg(None, None), "https://gitlab.com/x/y.git", "main")
                .unwrap()
                .repo(),
            UPSTREAM_REPO
        );
        // ssh origin parses too.
        assert_eq!(
            resolve_target(&cfg(None, None), "git@github.com:me/fork.git", "main")
                .unwrap()
                .repo(),
            "me/fork"
        );
    }

    #[test]
    fn target_config_repo_wins_and_is_validated() {
        assert_eq!(
            resolve_target(&cfg(Some("me/fork"), None), ORIGIN, "main")
                .unwrap()
                .repo(),
            "me/fork"
        );
        // Blank is "not set".
        assert_eq!(
            resolve_target(&cfg(Some("  "), None), ORIGIN, "main")
                .unwrap()
                .repo(),
            "someone/flow-wm"
        );
        for bad in [
            "fork",
            "me/fork/extra",
            "/fork",
            "me/",
            "me fork/x",
            "me/fo rk",
        ] {
            let err = resolve_target(&cfg(Some(bad), None), ORIGIN, "main").unwrap_err();
            assert!(matches!(err, UpdateError::NotFound(_)), "{bad}: {err}");
            assert_eq!(err.exit_code(), EXIT_NOT_FOUND);
        }
    }

    #[test]
    fn target_release_mode_on_empty_branch_or_no_build_branch() {
        let release = Target::Release {
            repo: "someone/flow-wm".into(),
        };
        // Explicit empty string: releases even though the build has a branch.
        assert_eq!(
            resolve_target(&cfg(None, Some("")), ORIGIN, "feat/x"),
            Ok(release.clone())
        );
        assert_eq!(
            resolve_target(&cfg(None, Some("  ")), ORIGIN, "feat/x"),
            Ok(release.clone())
        );
        // Absent and no build branch (detached HEAD / tarball): releases.
        assert_eq!(resolve_target(&cfg(None, None), ORIGIN, ""), Ok(release));
        // Explicit branch beats the build branch.
        assert_eq!(
            resolve_target(&cfg(None, Some("main")), ORIGIN, "feat/x"),
            Ok(Target::Branch {
                repo: "someone/flow-wm".into(),
                branch: "main".into()
            })
        );
    }

    // --- exit codes ---

    #[test]
    fn exit_codes_are_distinct_and_documented() {
        assert_eq!(Verdict::UpToDate.exit_code(), 0);
        assert_eq!(Verdict::Newer.exit_code(), 10);
        assert_eq!(UpdateError::Network("x".into()).exit_code(), 2);
        assert_eq!(UpdateError::NotFound("x".into()).exit_code(), 3);
        assert_eq!(http(403).exit_code(), 4);
        assert_eq!(UpdateError::Malformed("x".into()).exit_code(), 4);
        assert_eq!(UpdateError::NoBuildInfo.exit_code(), 5);
        // Error messages stay plain and name the cause.
        assert_eq!(
            UpdateError::Network("dns".into()).to_string(),
            "could not reach api.github.com: dns"
        );
        assert!(http(403).to_string().contains("rate limited"));
        assert!(!http(500).to_string().contains("rate limited"));
    }

    // --- fallback decision ---

    #[test]
    fn only_a_404_falls_back_to_the_branch_head() {
        assert!(should_fall_back(&http(404)));
        assert!(!should_fall_back(&http(403)));
        assert!(!should_fall_back(&http(500)));
        assert!(!should_fall_back(&UpdateError::Network("x".into())));
        assert!(!should_fall_back(&UpdateError::Malformed("x".into())));
    }

    #[test]
    fn fallback_failure_names_the_missing_branch_or_repository() {
        // 422 is GitHub's answer for a ref the repository does not have.
        assert_eq!(
            head_not_found(&http(422), "me/f", "feat/x"),
            Some(UpdateError::NotFound(
                "me/f has no branch \"feat/x\"".into()
            ))
        );
        // 404 is a repository that does not exist (or is private).
        assert_eq!(
            head_not_found(&http(404), "owner/nope", "main"),
            Some(UpdateError::NotFound(
                "repository owner/nope not found on GitHub (or private)".into()
            ))
        );
        // Anything else is reported as it is.
        assert_eq!(head_not_found(&http(403), "me/f", "main"), None);
        assert_eq!(head_not_found(&http(500), "me/f", "main"), None);
        assert_eq!(
            head_not_found(&UpdateError::Network("x".into()), "me/f", "main"),
            None
        );
        for status in [404, 422] {
            assert_eq!(
                head_not_found(&http(status), "me/f", "main")
                    .unwrap()
                    .exit_code(),
                EXIT_NOT_FOUND
            );
        }
    }

    // --- compare status -> report ---

    fn compare_json(status: &str, ahead: u64, behind: u64, with_commits: bool) -> String {
        let commits = if with_commits {
            r#"[
              {"sha":"1111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","commit":{"committer":{"date":"2026-10-23T08:00:00Z"},"author":{"date":"2026-10-23T07:00:00Z"},"message":"older: first\n\nbody"}},
              {"sha":"a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0","commit":{"committer":{"date":"2026-10-24T09:30:00Z"},"author":{"date":"2026-10-24T09:00:00Z"},"message":"fix: newest thing\n\nmore"}}
            ]"#
        } else {
            "[]"
        };
        format!(
            r#"{{"url":"x","status":"{status}","ahead_by":{ahead},"behind_by":{behind},"total_commits":{ahead},"commits":{commits},"files":[]}}"#
        )
    }

    #[test]
    fn compare_identical_is_up_to_date() {
        let r = compare_report(
            &compare_json("identical", 0, 0, false),
            &local(),
            "me/f",
            "main",
        )
        .unwrap();
        assert_eq!(r.verdict, Verdict::UpToDate);
        assert_eq!(
            r.summary,
            "up to date: your build cf54b88 is the head of me/f main"
        );
        assert!(r.details.is_empty());
    }

    #[test]
    fn compare_ahead_means_the_build_is_behind() {
        let r =
            compare_report(&compare_json("ahead", 2, 0, true), &local(), "me/f", "main").unwrap();
        assert_eq!(r.verdict, Verdict::Newer);
        assert_eq!(
            r.summary,
            "update available: your build cf54b88 is 2 commits behind me/f main"
        );
        // The newest remote commit is the last of the list: short sha, date, first line.
        assert_eq!(
            r.details,
            vec!["newest remote commit: a1b2c3d (2026-10-24) fix: newest thing"]
        );
    }

    #[test]
    fn compare_ahead_by_one_is_singular() {
        let r =
            compare_report(&compare_json("ahead", 1, 0, true), &local(), "me/f", "main").unwrap();
        assert!(r.summary.contains("is 1 commit behind"), "{}", r.summary);
    }

    #[test]
    fn compare_behind_means_the_build_is_ahead() {
        let r = compare_report(
            &compare_json("behind", 0, 3, false),
            &local(),
            "me/f",
            "main",
        )
        .unwrap();
        assert_eq!(r.verdict, Verdict::UpToDate);
        assert_eq!(
            r.summary,
            "up to date: your build cf54b88 is 3 commits ahead of me/f main (nothing newer on the remote)"
        );
    }

    #[test]
    fn compare_diverged_counts_both_sides() {
        let r = compare_report(
            &compare_json("diverged", 2, 5, true),
            &local(),
            "me/f",
            "main",
        )
        .unwrap();
        assert_eq!(r.verdict, Verdict::Newer);
        assert_eq!(
            r.summary,
            "update available: me/f main has 2 commits your build cf54b88 lacks, and your build has 5 commits the branch lacks (diverged)"
        );
        assert_eq!(r.details.len(), 1);
    }

    #[test]
    fn compare_rejects_unknown_status_and_bad_json() {
        let err = compare_report(
            &compare_json("sideways", 0, 0, false),
            &local(),
            "me/f",
            "main",
        )
        .unwrap_err();
        assert!(matches!(err, UpdateError::Malformed(_)));
        let err = compare_report("<html>", &local(), "me/f", "main").unwrap_err();
        assert!(matches!(err, UpdateError::Malformed(_)));
        assert_eq!(err.exit_code(), EXIT_GITHUB);
    }

    // --- 404 fallback: branch head vs local date ---

    fn head_json(date: &str) -> String {
        format!(
            r#"{{"sha":"2a179c3aab137a9839d654a69b83d7ec1a954e6c","commit":{{"author":{{"name":"x","date":"{date}"}},"committer":{{"name":"x","date":"{date}"}},"message":"chore: bump version to 0.2.0"}}}}"#
        )
    }

    #[test]
    fn head_newer_by_date_is_an_update() {
        let r = head_report(&head_json("2026-11-01T11:11:43Z"), &local(), "me/f", "main").unwrap();
        assert_eq!(r.verdict, Verdict::Newer);
        assert_eq!(
            r.summary,
            "update available: me/f main head is 2a179c3, committed 2026-11-01; your build is cf54b88 from 2026-10-22, not found on the remote (not pushed yet, or rewritten)"
        );
        assert_eq!(
            r.details,
            vec!["remote head: 2a179c3 (2026-11-01) chore: bump version to 0.2.0"]
        );
    }

    #[test]
    fn head_same_day_or_older_is_not_an_update() {
        let r = head_report(&head_json("2026-10-22T23:59:59Z"), &local(), "me/f", "main").unwrap();
        assert_eq!(r.verdict, Verdict::UpToDate);
        assert!(
            r.summary.starts_with("nothing newer by date: "),
            "{}",
            r.summary
        );
        let r = head_report(&head_json("2026-08-08T11:11:43Z"), &local(), "me/f", "main").unwrap();
        assert_eq!(r.verdict, Verdict::UpToDate);
    }

    #[test]
    fn head_without_a_local_date_counts_as_newer() {
        let mut l = local();
        l.commit_date.clear();
        let r = head_report(&head_json("2026-08-08T11:11:43Z"), &l, "me/f", "main").unwrap();
        assert_eq!(r.verdict, Verdict::Newer);
        assert!(r.summary.contains("from an unknown date"), "{}", r.summary);
    }

    // --- release mode ---

    fn release_json(tag: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","name":"{tag}","html_url":"https://github.com/me/f/releases/tag/{tag}","published_at":"2026-08-08T11:15:31Z","assets":[]}}"#
        )
    }

    #[test]
    fn release_newer_same_older() {
        let r = release_report(&release_json("v0.3.0"), &local(), "me/f").unwrap();
        assert_eq!(r.verdict, Verdict::Newer);
        assert_eq!(
            r.summary,
            "update available: release v0.3.0 of me/f is newer than your 0.2.0"
        );
        assert_eq!(
            r.details,
            vec![
                "published 2026-08-08".to_owned(),
                "https://github.com/me/f/releases/tag/v0.3.0".to_owned()
            ]
        );

        let r = release_report(&release_json("v0.2.0"), &local(), "me/f").unwrap();
        assert_eq!(r.verdict, Verdict::UpToDate);
        assert_eq!(
            r.summary,
            "up to date: release v0.2.0 is the latest of me/f and matches your 0.2.0"
        );

        let r = release_report(&release_json("0.1.9"), &local(), "me/f").unwrap();
        assert_eq!(r.verdict, Verdict::UpToDate);
        assert_eq!(
            r.summary,
            "up to date: your 0.2.0 is newer than the latest release 0.1.9 of me/f"
        );
    }

    #[test]
    fn release_with_odd_tag_is_malformed() {
        let err = release_report(&release_json("nightly"), &local(), "me/f").unwrap_err();
        assert!(matches!(err, UpdateError::Malformed(_)));
        let err = release_report("{}", &local(), "me/f").unwrap_err();
        assert!(matches!(err, UpdateError::Malformed(_)));
    }

    // --- semver ---

    #[test]
    fn version_parse_and_compare() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version(" v0.0.0 "), Some((0, 0, 0)));
        assert_eq!(parse_version("garbage"), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.3-rc1"), None);
        assert_eq!(compare_versions("v1.0.0", "0.9.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.0.1", "1.0.0"), Some(Ordering::Greater));
        assert_eq!(
            compare_versions("v0.10.0", "0.9.0"),
            Some(Ordering::Greater)
        );
        assert_eq!(compare_versions("1.0.0", "1.0.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("0.9.9", "1.0.0"), Some(Ordering::Less));
        assert_eq!(compare_versions("x", "1.0.0"), None);
        assert_eq!(compare_versions("1.0.0", "x"), None);
    }

    // --- how-to lines ---

    #[test]
    fn how_to_names_the_recipe_and_never_promises_an_install() {
        let branch = Target::Branch {
            repo: "me/f".into(),
            branch: "main".into(),
        };
        let lines = how_to_update(&branch, Some("C:\\flow\\bin"));
        assert!(lines[0].contains("git pull (branch main)"), "{}", lines[0]);
        assert!(lines[0].contains("cargo build --release"));
        assert!(lines[0].contains("in C:\\flow\\bin"));
        assert!(lines[1].contains("never downloads"));

        let release = Target::Release {
            repo: "me/f".into(),
        };
        let lines = how_to_update(&release, None);
        assert!(
            lines[0].contains("https://github.com/me/f/releases/latest"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("over the installed flow.exe"));
    }

    #[test]
    fn commit_helpers_handle_short_and_empty_fields() {
        let c: CommitJson = serde_json::from_str(
            r#"{"sha":"abc","commit":{"committer":null,"author":{"date":"2026-01-02"},"message":""}}"#,
        )
        .unwrap();
        assert_eq!(c.short(), "abc");
        assert_eq!(c.date(), "2026-01-02");
        assert_eq!(c.title(), "");
        assert_eq!(c.describe(), "abc (2026-01-02) ");
        assert_eq!(date_only("2026-10-22T10:00:00Z"), "2026-10-22");
        assert_eq!(date_only(""), "");
    }
}
