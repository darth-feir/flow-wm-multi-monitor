# A bad `[binds]` table is visible at start

**Status:** Accepted (stage 38, 2026-11-12). Changes what `flow start`,
`flow config check`, `flow query binds` and the daemon's start do with a
`[binds]` table that does not resolve; the reload path is unchanged. Builds
on [ADR-0015](0015-binds-are-config.md).

## Context

A reload with a bad `[binds]` table is refused and the running table kept.
At start the same table was only logged: `load_app_config` wrote one joined
`config validation warning` line to the daemon's log and returned the config,
the daemon kept the bad table, `flow query binds` answered with an error and
the keymap fell back to its baked defaults. `flow start`'s pre-flight check
failed only on a file that does not parse, so it printed nothing either. On
the autostart path (the Startup-folder shortcut runs `flow.exe start --ahk`
in a hidden console of its own) nothing at all was visible: the user got the
default chords without knowing why their own did not work. A failed start on
that path was just as silent: the cause went to a console nobody sees, and a
pre-flight failure (the daemon never ran) left nothing in the log either.

## Considered Options

- **What the daemon runs with.**
  - **(a) The built-in defaults for the whole table.** Chosen: the table is
    one unit everywhere else (a reload accepts or refuses all of it), and
    it is what the keymap already registered in this case.
  - (b) The good entries over the defaults, the bad ones dropped. Rejected:
    a table that is half applied is harder to explain than one set aside,
    and it would differ from the reload rule.
  - (c) Refuse to start. Rejected: a typo in one chord would leave the user
    with no window manager.
- **How a start nobody watches tells the user.**
  - **(a) A notification-area balloon (`Shell_NotifyIconW`, `NIF_INFO`)**,
    which Windows 10/11 show as a toast attributed to `flow.exe`. Chosen:
    no AppUserModelID, no shortcut, no registry; `windows` already has it.
  - (b) A timed `MessageBoxW` (the first build of this stage). Replaced at
    the user's request: a toast is the Windows way to report from the
    background, and a dialog at logon is in the way.
  - (c) A WinRT toast. Rejected: it needs a registered AppUserModelID.
- **When stderr has no reader.**
  - **(a) No console, or a console no other process shares**
    (`GetConsoleWindow`, `GetConsoleProcessList`). Chosen: a console program
    started from a shortcut or by AutoHotkey's `Run(..., "Hide")` gets a
    hidden console of its own that closes when it exits; one started from a
    shell shares the shell's.
  - (b) stderr is not a terminal. Rejected: the hidden console of its own is
    a terminal too, so the autostart path would never be detected.

## Decision

- One wording for every bind problem: `config::binds_problems` turns each
  `BindsConfig::resolve` problem into `flow.toml: binds."<key>": <reason>`.
  `load_app_config` no longer validates `[binds]` (it validates the rest with
  `FlowConfig::validate_except_binds`); every caller reports the table.
- **Daemon start:** `config::set_aside_bad_binds` replaces a table with
  problems by the built-in one; the daemon logs one `flowd: <problem>` warning
  per entry and `flowd: the daemon starts with the built-in default binds
  (...)`, keeps the problems, and `flow query binds` lists them (`set_aside`
  in the JSON, a `set_aside <n>` record in `--plain`, the source `defaults`).
  The next applied reload clears them. A reload still refuses a bad table.
- **`flow start`:** the pre-flight prints `flow: warning: <problem>` per entry
  and `flow: warning: the daemon starts with the built-in default binds (the
  whole [binds] table is set aside); fix it in <path> and save to apply it`.
  With no reader (above) it sends, after the daemon reports ready, a notice
  titled `FlowWM`: `flow started with default binds for N key(s): <keys>.
  Check the log at <log>`.
- **A failed `flow start`** (pre-flight, spawn, a failed stage, the ready
  timeout, a missing `flow.ahk`; not "already running") appends
  `flow start failed: <cause>` to the log the daemon would have written, and
  with no reader sends `flow start failed: <cause>. Check the log at <log>`.
- **`flow config check`** lists the same lines, with the daemon and without.
- The notice: a hidden `STATIC` window owns a notify icon with the system
  warning or error icon; `flow.exe` keeps it for 10 s, then deletes the icon
  and exits.

### Why a toast here when the keymap never uses `TrayTip`

`flow.ahk` and its README ban `TrayTip` because a Windows 11 toast held the
foreground in the VM: `SetForegroundWindow` and `WinActivate` failed while it
was up, so a chord acted on the wrong window. That ban is about a running
daemon. Here the failure notice comes when no daemon runs, so there is no
focus to disturb; the warning notice comes after the daemon is ready and has
placed the windows it found, so its own start does not overlap it. The VM
case B (stage 38) opens a window while the toast is up and checks that it is
seated and focused.

## Consequences

- Windows drops a balloon notice together with its icon: it is shown as a
  toast for its display time (5 s by default) and is not kept in the
  notification center, before or after `flow.exe` exits (stage 38 VM probe).
  The log is the durable record, and the notice names it.
- `flow start` from autostart takes about 10 s longer to return when it has
  something to say; nothing waits on it.
- A `flow start` whose output a caller redirects from a shared console (a
  script, `cmd /c ... > file`) prints and sends no notice.
- Windows writes its own `NotifyIconGeneratedAumid_*` entry under the user's
  notification settings for `flow.exe`; FlowWM writes no registry value.
- A script that read `flow query binds` failing as "the table is bad" now
  gets the defaults and `set_aside`; `flow config check` lines gained the
  `flow.toml: ` prefix.
