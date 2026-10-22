# The daemon trusts its own user only, and nothing it did not write

**Status:** Accepted — the result of a security review of the fork
(2026-10-22) with every finding fixed in one pass.

The trust boundary is the **interactive user the daemon runs as**. The pipe
`\\.\pipe\flow` is created with a protected DACL naming SYSTEM, Builtin
Administrators and that user's SID only, rejects remote clients, and the CLI
opens it with identification-only quality of service. Every input that
crosses a boundary is bounded or validated: pipe lines (64 KiB), the
`[loadout] default_path` and explicit loadout paths, the hotkey script's
view of where `flow.exe` is (`FLOW_BIN` and the config directory as its
working directory), the pidfile (`flow stop` kills a pid only if its image
path is the one recorded), and the border overlay's window procedure
(a process-local map, not `GWLP_USERDATA`). The daemon writes no window
titles to disk below `debug` and prunes its logs. `flow update` only
*checks*; nothing in the program downloads or replaces binaries.

## Context

The fork runs on a managed work machine, so the user asked for a full
review before trusting it as a daily driver: three independent reviewers
(code review, dependency audit, behaviour under the VM) plus `cargo audit` /
`cargo deny`. Nothing untoward was found, but the review listed what a local
attacker or a hostile window could do: any local account could connect to
the pipe and dispatch; the default pipe QoS lets an impostor server
impersonate the CLI's user; a `flow.toml` with `default_path = ..\..\x`
made the daemon write there on every stop; the keymap ran `flow` by bare
name from an inherited cwd, which `CreateProcess` searches before `PATH`;
`flow stop` would kill whatever process now held a stale pidfile's pid; the
overlay dereferenced a pointer any process can write with
`SetWindowLongPtr`; info-level logs and the loadout file kept every window
title; the upstream self-updater downloaded a zip, verified a sidecar hash
from the same origin and swapped binaries through a PowerShell shim.

## Considered Options

- **Keep the default pipe security** (upstream). Rejected: on a
  multi-account or shared machine any account can drive the window manager
  of another. The DACL costs one SDDL string. The user's SID is read from
  the process token because the creator-owner placeholder `CO` is only
  substituted during ACL inheritance and means nothing in a pipe's own
  DACL.
- **Authenticate IPC messages** (a token in each line). Rejected: the DACL
  already answers "who may connect"; a token would have to be stored where
  the same user can read it.
- **Validate loadout paths in the CLI only.** Rejected: the daemon does not
  trust the pipe to have checked. Both ends run the same pure functions
  (`src/loadout/path.rs`): the default path is a plain file name, an explicit
  path is `.json` with no `..`, a relative one resolves against the config
  directory, never the daemon's cwd.
- **Fix the keymap's `flow` lookup in the keymap only.** Rejected: a user's
  own keymap would have the same hole. The daemon starts the script with the
  config directory as cwd and `FLOW_BIN` in an explicit environment block;
  the shipped keymaps resolve `%FLOW_BIN%\flow.exe` first and run it by
  absolute path.
- **Keep the self-updater and sign the archive.** Rejected: the fork is
  built from source, and a window manager has no reason to download and
  execute. `flow update` compares the embedded build metadata (`build.rs`:
  commit, branch, date, origin) with a GitHub branch or release over one
  `https_only` agent with a 5 s timeout and prints the recipe. The `zip` and
  `sha2` dependencies went with it (177 → 165 crates). `check_for_updates`
  stays `false` by default.

## Consequences

- **Elevated clients still work** (Administrators are in the DACL), so an
  admin terminal can `flow dispatch`; other local accounts get `Access is
  denied` at open. `PIPE_REJECT_REMOTE_CLIENTS` is set although the pipe is
  never exported.
- **A flooding or silent client cannot wedge the daemon**: a line over 64 KiB
  is `InvalidData` and the client is disconnected; the read deadline is
  re-checked before every chunk; a line that does not decode is logged with
  a ~200-byte preview, never in full.
- **The pidfile is advisory.** The daemon kills its script through the
  handle it has held since the launch; `flow stop` compares the image path;
  a live pid with another image is treated as stale with an `info` log. The
  AutoHotkey install dir is read from `HKLM` before `HKCU` because the
  per-user key needs no rights to write.
- **The overlay trusts only its own registry**: `Border::create` registers
  the HWND in a process-local `HWND → Weak<BorderInner>` map and the window
  procedure acts only on HWNDs it knows.
- **Privacy is a default, not a flag.** Titles appear at `debug` only
  (`flow start --verbose`); `[logging] keep_days` (default 14, 0 = forever)
  deletes `flowd-YYYY-MM-DD.log` files older than that by the date in the
  name; the loadout file no longer writes `title` (older files still load).
- **Accepted costs.** The pipe DACL means a daemon started by one account
  cannot be driven by a `flow` run as another (by design); `flow update` on
  an unpushed fork can only compare by date and says so; the test suite must
  be pointed at scratch `FLOW_CONFIG_DIR` and `APPDATA` directories on a
  machine where FlowWM is installed, because two autostart tests resolve the
  real Startup folder.
- **Tests.** The SDDL has no Everyone entry and converts for the live SID;
  a 200 KB newline-free flood is refused and the next client served; the
  path rules; the pidfile image check against a real child process; the job
  breakaway (a member's child is never in the job's pid list); the updater's
  verdicts over canned GitHub JSON. See `docs/src/dev-guide/ipc-and-watchdog.md`
  (*Pipe security*, *The hotkey script is tied to the daemon*),
  `docs/src/dev-guide/loadout.md` (*Where the file may be*) and
  `docs/src/dev-guide/updater.md`.
