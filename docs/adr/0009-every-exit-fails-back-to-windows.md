# Every exit fails back to the plain Windows desktop

**Status:** Accepted — covers the crash half of the roadmap's "Watchdog and
recovery snapshot" item without a watchdog process. The snapshot half
(restoring a layout after a crash) remains open.

Every way `flowd` can end — `flow stop`, a close or logoff message, a
console control event, a dead helper thread, a panic — runs one teardown,
`FlowWM::fail_back`, in a fixed order: cancel **and join** the animator,
rescue stranded windows on-screen, destroy the border overlays, stop the hook
thread, kill the hotkey script, cancel the pipe's pending accept, write the
status file, `std::process::exit(code)`. The hotkey script runs inside a
**Job Object** with `KILL_ON_JOB_CLOSE`, so the Win key returns to Windows
even when the daemon is killed with `taskkill /f`. Startup is staged, each
stage reports, and a failure after windows have moved runs the same rescue.
`flow rescue` does the rescue without a daemon, and `flow start` runs it
before spawning.

## Context

The user's requirement: *if a part fails, default to the normal Windows
desktop instead of a half-broken state — especially with the Win key
overrides.* Three failures were observed in the VM before this work: an
asynchronous animator cancel let the initial-layout tween overwrite the
rescue; a daemon that died outside IPC stayed alive until the next `flow`
touched the pipe, because `CloseHandle` does not cancel a pending
`ConnectNamedPipe`; and a dead daemon left AutoHotkey holding every Win
chord, with no `flow` to send `stop`.

## Considered Options

- **A separate watchdog process** (the roadmap's design). Deferred, not
  rejected: it is the only answer for restoring a *layout* after a crash,
  but the immediate need — windows on-screen, Win key released — is met
  in-process plus one kernel object. A watchdog can be added on top; nothing
  here depends on its absence.
- **Let each exit path clean up what it knows about.** Rejected: that is
  how the three observed failures arose. One teardown with a fixed order is
  the whole point; a new exit path calls it, it does not re-implement it.
- **Kill the hotkey script by pid from `flow stop` only.** Rejected: that
  needs a live `flow stop`. The job object is the backstop that needs no
  code to run; the daemon's own teardown kills the script through the handle
  it has held since the launch, and `flow stop`, which has only the pidfile,
  terminates the pid only if its image path matches the one recorded.
- **Inherit the job into everything the script starts.** Rejected after it
  killed the user's terminals on `flow stop`: the job sets
  `SILENT_BREAKAWAY_OK`, so the script is a member and its children are not.
- **Rely on destructors at exit.** Rejected: `main` leaves through an
  explicit `exit(code)` after the status write; helper threads winding down
  are never waited for beyond the bounded joins in the teardown.

## Consequences

- **Exit codes and status are part of the contract.** `stopped` / 0 for a
  clean stop or a session message, `error` / 2 for a runtime fail-back,
  `10 + stage` for a startup failure, with the stage name in
  `flowd.status` and in `flow start`'s message. The panic hook writes the
  status before logging so a crash while logging still leaves a readable
  file.
- **The loop wakes at least once a second.** A dead animator worker sets
  no event, so `LIVENESS_TICK_MS` bounds how long the loop may sleep; one
  wake per second is the cost of noticing a dead helper thread. The shutdown
  flag is checked on every wake and again after pumping window messages,
  which is where `WM_ENDSESSION` arrives.
- **Signals need a window.** Message-only windows receive no session
  broadcasts and the overlays exist only while there are windows to
  decorate, so the daemon creates an invisible top-level session-sink window
  whose procedure turns `WM_CLOSE` / `WM_QUERYENDSESSION` / `WM_ENDSESSION`
  into a shutdown request; the logoff is never vetoed.
- **The rescue is daemon-less by design.** `flow rescue` enumerates the
  windows the daemon's scan would consider, strips invisible borders and
  clamps any window whose visible content misses the virtual screen into the
  work area of its home monitor (the one sharing most of its horizontal or
  vertical band). It is what a user runs after a power loss or an older
  build; it is also why the parking rule (ADR-0007) can keep windows far
  off-screen with a clear conscience.
- **Accepted costs.** A hard kill still loses the layout (no snapshot);
  the console handler blocks up to 4 s on close/logoff/shutdown for the
  rescue to finish; a wedged daemon that holds the pipe busy is reported
  alive by the keymap's liveness gate for up to five polls before the keys
  fall through.
- **Tests.** Fault injection (`FLOW_FAIL_AT=<stage>`) exercises every
  startup stage's failure path; the VM harness kills the daemon with
  `taskkill /f` and checks that AutoHotkey is gone and the Win key is free,
  and that `WM_CLOSE` and `flow stop` both exit in about 0.1 s with status
  `stopped`. See `docs/src/dev-guide/ipc-and-watchdog.md` (*Fail-back*,
  *Startup stages*, *The hotkey script is tied to the daemon*).
