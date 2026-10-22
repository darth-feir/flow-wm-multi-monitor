# Update Checker (`flow update`)

`flow update` is a **checker, never an installer**. It asks the GitHub API
whether something newer than the running build exists, prints the answer and
the recipe for updating, and exits with a code that scripts can read. It does
not download archives, verify digests, stage files or swap binaries — this
fork is built from source, and a source build is updated with `git pull` +
`cargo build --release`. (The upstream self-updater with its PowerShell swap
shim was removed in this fork, together with the `zip` and `sha2`
dependencies it needed.)

| Command | Action |
|---------|--------|
| `flow update` | Compare this build with the configured branch or the latest release; print the verdict and how to update. Safe while the daemon runs. |
| `flow update --check` | The same thing. `--check` is accepted for compatibility and changes nothing. |
| `flow --version` / `flowd --version` | `flow 0.2.0 (feat/multi-monitor @ cf54b88, 2026-10-22)` — the crate version plus the embedded branch, commit and commit date. |

## Build metadata

`build.rs` runs `git` in the checkout being built and hands the result to the
crate as `FLOW_BUILD_*` environment variables, which
[`src/build_info.rs`](../../src/build_info.rs) exposes as constants:

| Constant | Source | Example |
|----------|--------|---------|
| `VERSION` | `CARGO_PKG_VERSION` | `0.2.0` |
| `COMMIT` / `COMMIT_SHORT` | `git rev-parse HEAD` / `--short HEAD` | `cf54b88…` / `cf54b88` |
| `BRANCH` | `git rev-parse --abbrev-ref HEAD` (empty for a detached `HEAD`) | `feat/multi-monitor` |
| `DIRTY` | `git status --porcelain --untracked-files=no` non-empty | `false` |
| `COMMIT_DATE` | `git log -1 --format=%cs` (committer date) | `2026-10-22` |
| `ORIGIN_URL` | `git config --get remote.origin.url` | `https://github.com/CCpcalvin/flow-wm` |

Every variable is always set — to the empty string when `git` is not
installed or the source is not a checkout (a crates.io tarball) — so the crate
reads them with `env!` and the build never fails for lack of git. The script
declares `rerun-if-changed` on `.git/HEAD`, `.git/index`, `.git/packed-refs`
and the current branch's ref file, so a new commit or checkout is picked up
by the next `cargo build`. A dirty tree shows as `cf54b88-dirty` in
`--version` and in the checker's output.

`build_info::github_repo_from_url` turns the origin URL into `owner/name`
for the https (`https://github.com/owner/name.git`), ssh
(`git@github.com:owner/name.git`, `ssh://git@github.com/owner/name.git`)
and `git://` spellings; any other host yields `None`.

## Configuration

Two pieces of `flow.toml` drive the checker (see the
[Config Reference](../user-guide/config-reference.md#update--update-checker)):

```toml
check_for_updates = false   # true: `flow start` runs the checker and prints a one-line notice

[update]
# repo = "owner/name"       # default: parsed from the build's origin URL, else "CCpcalvin/flow-wm"
# branch = "main"           # default: the build's branch; "" = compare against releases instead
```

`updater::resolve_target` turns these into a `Target`:

- **Branch mode** (`Target::Branch`) when a branch is known — from
  `[update] branch`, else the build's branch. The embedded commit is compared
  with that branch of the repository.
- **Release mode** (`Target::Release`) when `branch = ""` is set, or when no
  branch is known at all (detached `HEAD`, tarball). The crate version is
  compared with the latest release's tag.

`repo` is validated to be `owner/name` (letters, digits, `-`, `_`, `.`);
anything else is reported as a config error (exit code 3) rather than sent to
GitHub.

## The check

All requests go through one `ureq` agent built with `https_only(true)`, a
5-second global timeout and `User-Agent: flow-wm/<version>`, without a token
(unauthenticated GitHub allows 60 requests per hour per address; a 403/429 is
reported with that hint). `ureq` reads `HTTPS_PROXY` / `HTTP_PROXY` /
`ALL_PROXY` / `NO_PROXY` from the environment by itself.

### Branch mode

1. `GET https://api.github.com/repos/{repo}/compare/{local_full_sha}...{branch}`.
   GitHub's `status` is from the branch's point of view, so the mapping is:

   | `status` | Meaning | Verdict | Output |
   |----------|---------|---------|--------|
   | `identical` | the build is the branch head | up to date | `up to date: your build cf54b88 is the head of {repo} {branch}` |
   | `ahead` | the branch has commits the build lacks | **newer** | `update available: your build cf54b88 is N commits behind {repo} {branch}` + `newest remote commit: a1b2c3d (date) first line` |
   | `behind` | the build has commits the branch lacks | up to date | `up to date: your build cf54b88 is N commits ahead of {repo} {branch} (nothing newer on the remote)` |
   | `diverged` | both sides have commits the other lacks | **newer** | `update available: {repo} {branch} has N commits your build lacks, and your build has M commits the branch lacks (diverged)` + newest remote commit |

   The newest remote commit is the last entry of the response's `commits`
   list (short sha, committer date, first line of the message).

2. **404 fallback.** GitHub answers 404 when the local commit is unknown to it
   — the fork has not been pushed, or the branch was rewritten. The checker
   then asks `GET /repos/{repo}/commits/{branch}` for the branch head alone
   and judges by **commit date**: newer when the head was committed on a later
   day than the build's commit. Output:
   `update available: {repo} {branch} head is 2a179c3, committed 2026-11-01;
   your build is cf54b88 from 2026-10-22, not found on the remote (not pushed
   yet, or rewritten)` (or `nothing newer by date: …`). A second 404 means the
   branch or the repository does not exist and is reported as such.

   Only a 404 falls back (`should_fall_back`); a 403, a 5xx or a network error
   is reported directly.

3. A build without an embedded commit (not a git checkout) cannot use branch
   mode: `UpdateError::NoBuildInfo` tells the user to set `branch = ""`.

### Release mode

`GET /repos/{repo}/releases/latest`; the tag (`v0.3.0` or `0.3.0`) and
`CARGO_PKG_VERSION` are parsed as `MAJOR.MINOR.PATCH` triples and compared
(`compare_versions`). Newer → `update available: release v0.3.0 of {repo} is
newer than your 0.2.0`; equal → `up to date: release v0.2.0 is the latest of
{repo} and matches your 0.2.0`; older → `up to date: your 0.2.0 is newer than
the latest release v0.1.9 of {repo}`. The publish date and release page URL
follow. A tag that is not a plain triple is reported as malformed (no
pre-release suffixes are modelled; FlowWM does not use them).

### Output and exit codes

`flow update` prints, each line prefixed `flow: `:

1. what is being checked and the local build
   (`checking owner/name branch main; your build: 0.2.0 (cf54b88, 2026-10-22)`),
2. the verdict line (the `summary` above),
3. any detail lines,
4. how to update — for branch mode
   `to update a source build: git pull (branch main), cargo build --release,
   flow stop, then copy target\release\flow.exe and flowd.exe over flow.exe and
   flowd.exe in <dir of the running flow.exe>`; for release mode the
   `https://github.com/{repo}/releases/latest` page — and the reminder that
   `flow update` never downloads or replaces anything itself.

Errors go to stderr as `flow: update check failed: <reason>`.

| Exit code | Constant | Meaning |
|-----------|----------|---------|
| `0` | `EXIT_UP_TO_DATE` | Nothing newer: identical, ahead, same or newer than the latest release, or not newer by date in the fallback. |
| `10` | `EXIT_NEWER` | Something newer exists (behind, diverged, the fallback head is newer by date, or a newer release). |
| `2` | `EXIT_NETWORK` | `could not reach api.github.com: …` — offline, DNS, TLS or the 5 s timeout. |
| `3` | `EXIT_NOT_FOUND` | The repository, branch or release does not exist (404 twice, or on the release endpoint); `[update] repo` is not `owner/name`; `flow.toml` could not be read. |
| `4` | `EXIT_GITHUB` | GitHub answered with another status (403 rate limit, 5xx) or something that did not parse. |
| `5` | `EXIT_NO_BUILD_INFO` | Branch mode on a build with no embedded commit. |

## Start-time notice

With `check_for_updates = true`, `flow start` runs the same `updater::check`
**after** the daemon has reported ready and prints one line when — and only
when — the verdict is "newer":

```
flow: update available: your build cf54b88 is 3 commits behind owner/name main — see "flow update"
```

Every failure (offline, a bogus `[update] repo`, a GitHub outage) is silent and
the daemon is already running, so the notice can never block or break a
start. It is **off by default in this fork**: a window manager has no reason
to contact the network at every start.

## Code path

| Step | Location |
|------|----------|
| Git metadata at build time | [`build.rs`](../../build.rs) |
| Constants, `version_line`, `github_repo_from_url` | [`src/build_info.rs`](../../src/build_info.rs) |
| `[update]` table | `UpdateConfig` in [`src/config/types.rs`](../../src/config/types.rs) |
| Target resolution, requests, JSON → `Report`, exit codes | [`src/updater/mod.rs`](../../src/updater/mod.rs) (`resolve_target`, `check_target`, `compare_report`, `head_report`, `release_report`, `how_to_update`) |
| CLI wiring | `cmd_update` and `maybe_notify_update` in [`src/bin/flow.rs`](../../src/bin/flow.rs) |

Everything that interprets GitHub's answers is a pure function over the JSON
text and a `LocalBuild` value, so the unit tests feed canned responses and
never touch the network. The only network code is `check_target` and the
`get` helper beneath it.

## Cross-references

- [Config & Persistence](config-and-persistence.md) — where `flow.toml` is
  read from
- [Building from Source](building-from-source.md) — the `git pull` +
  `cargo build --release` recipe the checker points at
