# Building from Source

FlowWM is a native Windows binary built with the MSVC toolchain. This page is
for contributors, or anyone who wants to build locally instead of installing
from crates.io with `cargo install flow-wm`.

> **This fork.** `cargo install flow-wm` installs upstream FlowWM
> ([CCpcalvin/flow-wm](https://github.com/CCpcalvin/flow-wm)). The fork
> ([darth-feir/flow-wm](https://github.com/darth-feir/flow-wm), branch
> `feat/multi-monitor`) is only built from source, as below.

> If you only want to *use* FlowWM, the high-level overview lives in the
> [User Guide](../user-guide/README.md). This page is the build detail.

## Prerequisites

- **Rust** (stable, MSVC toolchain) — install via [rustup](https://rustup.rs/).
- **Visual Studio Build Tools** — the "Desktop development with C++" workload.
  FlowWM links against Win32, so the MSVC linker and the Windows SDK are
  required. The MinGW / GNU toolchains will **not** work.

Set the MSVC target as your default so every `cargo` invocation uses it:

```powershell
rustup default stable-x86_64-pc-windows-msvc
```

## Clone and Build

```powershell
git clone -b feat/multi-monitor https://github.com/darth-feir/flow-wm.git
cd flow-wm
cargo build --release --locked   # optimised, stripped binaries (LTO)
cargo build                      # debug build
```

The two binaries — `flowd.exe` (daemon) and `flow.exe` (CLI client) — are
written to `target\release\` or `target\debug\`. `--locked` builds against the
committed `Cargo.lock`. The crate uses the 2024 edition, so it needs Rust 1.85
or newer; there is no `rust-toolchain` file pinning a version.

### Putting `flow` on your PATH

`flow config init` and the other commands assume `flow.exe` is reachable from
your shell. Either copy the built binaries into a directory that is already on
your `PATH`, or add the release folder to `PATH`:

```powershell
# Add the release folder to PATH for the current PowerShell session:
$env:PATH += ";$PWD\target\release"

# (Or, for a permanent user-scoped PATH update, use:
#  [Environment]::SetEnvironmentVariable("PATH", $env:PATH, "User") )
```

## First Run

```powershell
flow config init          # writes default config to %USERPROFILE%\.config\flow\
flow config init --ahk    # also writes flow.ahk — upstream's Alt-based AutoHotkey v2 script
                          # (the fork's Win-key keymap is keymaps\win-niri\*.ahk, copied by hand)
flow start                # launches the daemon in the background
```

The default config directory follows the XDG-style convention
`%USERPROFILE%\.config\flow\` (so it reads as `~/.config/flow/` on a Linux
mental model). You can override it with `--config <dir>` on `flow start` or
the `FLOW_CONFIG_DIR` environment variable. See
[Config & Persistence](./config-and-persistence.md) for the full resolution
chain.

Once the daemon is running, start your hotkey daemon (`flow start --ahk`, or
`flow enable-autostart --ahk` to auto-launch both at login) and you're tiling.
See the [User Guide](../user-guide/README.md) for everyday usage.

## Contributor Gates

Before sending a patch, make sure the usual gates pass:

```powershell
# Point the config and AppData directories at a scratch folder first, so the
# tests (some spawn a real flowd) never touch your own %USERPROFILE%\.config\flow:
$env:FLOW_CONFIG_DIR = "$env:TEMP\flow-test\flow"
$env:APPDATA = "$env:TEMP\flow-test-appdata"
cargo test                       # unit + integration tests
cargo clippy -- -D warnings      # lint (warnings are errors)
cargo fmt --check                # formatting check
```

Documentation builds:

```powershell
mdbook build docs                                  # the mdBook you are reading
```

When adding or changing a config field, update **both** the `Default` impl in
`src/config/types.rs` **and** the example file
`default-config.toml` in the repo root — a test
(`default_config_toml_matches_compiled_defaults`) enforces they stay in sync.
See [Config & Persistence](./config-and-persistence.md) and
[Design Decisions](./design-decisions.md) for why code is the single source of
truth for defaults.
