# Contributing & Reporting Issues

Contributions are welcome — bug reports, feature ideas, and pull requests all
help. This page is the user-facing short version. The canonical contributor
reference is the repo's [`CONTRIBUTING.md`](https://github.com/darth-feir/flow-wm-multi-monitor/blob/feat/multi-monitor/CONTRIBUTING.md),
and the architecture is documented in the
[Developer Guide](../dev-guide/index.md).

## Reporting bugs

A good bug report is reproducible. Please include:

- **FlowWM version** — `flow --version`.
- **Windows version** (e.g. Windows 11 23H2).
- **A log file.** FlowWM writes a date-stamped log by default:

  `%USERPROFILE%\.config\flow\logs\flowd-YYYY-MM-DD.log`

  (one file per day, appended across restarts).

  For a key binding that did nothing, add the keymap's log of the same day,
  `logs\flow-ahk-YYYY-MM-DD.log` in the same folder. It is off by default:
  set `keymap_log = true` under `[logging]` in `flow.toml`, restart the
  daemon, and reproduce the miss. It records each dispatch the keys sent;
  launcher queries are in it too, so look it over before attaching it.

  The cleanest way to capture a bug is to reproduce it with an isolated log:

  ```
  flow start --log-file <path>
  ```

  This redirects **all** logging to `<path>` and starts the file empty on each
  launch, so the file contains only that run. Attach that file to the issue.

- **Config files if relevant** (`flow.toml`, `flow-rules.toml`) — but redact any
  personal or sensitive content first.

### Raising the log level

To capture more detail, pass `--verbose` (debug level) or an explicit
`--log-level` to `flow start` (default is `debug` in debug builds, `info` in
release):

```
flow start --verbose --log-file <path>
flow start --log-level trace --log-file <path>
```

Setting `RUST_LOG` before starting still works (the flags above take
precedence when given):

```bat
:: Windows cmd
set RUST_LOG=trace && flow start --log-file <path>
```

```powershell
# PowerShell
$env:RUST_LOG='trace'; flow start --log-file <path>
```

### If the daemon fails to start

`flow start` waits for every startup step and tells you which one failed:

```
flow: FlowWM failed to start at step 'hooks': <reason> (log: <path>)
```

Include that line and the log it points at in your report. The step names
(`config`, `monitors`, `scan`, `pipe`, `hooks`, …) and the daemon's exit codes
are listed in the developer guide's IPC chapter.

### A note on config location

The config directory resolves in this priority order: `--config <dir>` flag →
`FLOW_CONFIG_DIR` env var → default `%USERPROFILE%\.config\flow\`. If you
override it, the `logs/` folder moves with it — mention your config dir in the
report.

> **Where to file:** open an issue at
> <https://github.com/darth-feir/flow-wm-multi-monitor/issues>. A bug that also happens
> with upstream FlowWM (without the fork's changes) belongs upstream, at
> <https://github.com/CCpcalvin/flow-wm/issues>.

## Requesting features

Feature requests are welcome too. The roadmap lives at
[Roadmap](../dev-guide/roadmap.md) — check there first to see whether your
idea is already planned or explicitly out of scope. If it isn't, open an issue
and describe the workflow you're trying to enable, not just the implementation
you imagine. "Here's the friction I hit" is more useful than "add this flag".

## Pull requests

This fork's work happens on the branch `feat/multi-monitor`, which is based
on upstream's `master` ([CCpcalvin/flow-wm](https://github.com/CCpcalvin/flow-wm));
the fork's own `master` tracks upstream. Branch off `feat/multi-monitor` and
open pull requests against it at
[darth-feir/flow-wm-multi-monitor](https://github.com/darth-feir/flow-wm-multi-monitor).

A change of behaviour or design comes with its rationale written down rather
than left in code comments: the fork's decisions are ADRs in `docs/adr/`
(the fork's are 0007 onwards, in the shape of ADR-0006: summary, context,
options considered, consequences), indexed in
[Design Decisions](../dev-guide/design-decisions.md#decisions-made-in-the-multi-monitor-fork);
how things work is written up in the Developer Guide (`docs/src/dev-guide/`).

Before opening a PR, make sure the basics are green:

```
cargo fmt
cargo clippy --all-targets
cargo test
```

A few project-wide conventions:

- **Document public items.** `#![warn(missing_docs)]` is enabled in `src/lib.rs`,
  so every public item needs at least a one-line `///` doc comment.
- **Config changes must update two places.** Default values live in the
  `Default` impls in `src/config/types.rs` (the single source of truth). If you
  add or change a field, also update `default-config.toml` — the
  `default_config_toml_matches_compiled_defaults` test enforces that they stay
  in sync.
- **Adding dependencies:** use `cargo add <crate>` rather than hand-editing
  `Cargo.toml`, so versions and the lockfile stay consistent.

For the full conventions (including the 3-layer documentation model and
contributor gates), see
[`CONTRIBUTING.md`](https://github.com/darth-feir/flow-wm-multi-monitor/blob/feat/multi-monitor/CONTRIBUTING.md)
in the repo root (upstream's text, which applies to the fork too).

## Understanding the codebase

For architecture, threading, the layout engine, and subsystem write-ups, see
the [Developer Guide](../dev-guide/index.md). Build it locally:

```
mdbook build docs/
```

The [Building from Source](../dev-guide/building-from-source.md) page covers
prerequisites, MSVC setup, and the contributor gates in detail.
