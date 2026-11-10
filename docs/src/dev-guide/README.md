# Developer Guide

This guide is the canonical narrative reference for `flow`. It explains *why*
the system is shaped the way it is, *how* the pieces fit together, and *where*
to look in the source for each concern.

## Suggested Reading Order

If you haven't built FlowWM yet, start with [Building from Source](./building-from-source.md).

1. **[Architecture](./architecture.md)** — the subsystem map and the
   single-orchestrator model. Start here.
2. **[Threading Model](./threading-model.md)** — why `flow` uses no
   `Arc<Mutex>` and how the hook thread talks to the IPC thread.
3. **[Event Pipelines](./event-pipelines.md)** — the two flows that drive every
   layout change: Win32 hooks and IPC commands.
4. **[Layout Overview](./layout/overview.md)** — the infinite canvas, the
   camera model, and the Virtual → Actual split. The heart of `flow`.
5. **[Mutation Pipeline](./layout/pipeline.md)** — how every command flows
   through `mutate → project → animate`.
6. The rest in any order: [Workspace](./workspace.md),
   [Window Registry](./window-registry.md), [Animation](./animation.md),
   [IPC & Watchdog](./ipc-and-watchdog.md),
   [Config & Persistence](./config-and-persistence.md).

## Reference

- **[Design Decisions](./design-decisions.md)** — consolidated "why not X"
  trade-offs (single package, pixel widths, TOML, etc.).
- **[Roadmap](./roadmap.md)** — what's implemented, what's stubbed, what's
  next.
## Repository Layout

```text
flow/
├── Cargo.toml
├── build.rs
├── default-config.toml              # hand-written EXAMPLE, the `flow config init` template (not read for defaults)
├── default-flow-rules.toml           # bundled default window rules (include_str!)
├── flow.ahk                         # AutoHotkey starter template (`flow config init --ahk`, include_str!)
├── keymaps/win-niri/                # niri-style keymap; binds.toml is the built-in [binds] default (include_str!)
├── docs/                            # this mdBook (src/) and the ADRs (adr/, outside the book)
├── schemas/                         # generated JSON schemas for config/rules
├── src/
│   ├── main.rs                      # flowd entry point
│   ├── lib.rs                       # shared library
│   ├── bin/
│   │   └── flow.rs                   # CLI client
│   ├── common/                      # shared types: Rect, WindowId, Direction, errors
│   ├── config/                      # TOML loading, schema gen, dirs, lifecycle, [binds], history
│   ├── registry/                    # WindowRegistry: OS sync, classification, hooks
│   ├── layout/                      # PURE layout math: types, mutations, projection, resize, preview
│   ├── workspace/                   # Monitor → Workspace → Scrolling/Floating spaces, y_offset
│   ├── animation/                   # embedded window-animation crate
│   ├── borders/                     # border overlay windows (colored ring per managed window)
│   ├── loadout/                     # loadout save/restore data model (pure serde types)
│   ├── ipc/                         # named-pipe transport + SocketMessage types
│   ├── daemon/                      # FlowWM orchestrator + submodules
│   ├── updater/                     # `flow update` release checker (never installs)
│   ├── autostart.rs                 # daemon autostart (`flow enable-autostart`) + hotkey-script lifecycle
│   ├── build_info.rs                # commit / branch / origin embedded at build time
│   ├── job.rs                       # kill-on-close job object for the hotkey script
│   ├── logging.rs                   # daemon log setup
│   └── rescue.rs                    # daemon-less off-screen window rescue (`flow rescue`)
└── tests/                           # integration tests (tests/cli.rs + tests/cli/)
```

The two big ideas to internalise before reading any source file:

1. **The layout pipeline is pure.** `src/layout/` has zero Win32 dependencies
   and is fully unit-testable on any platform. `src/workspace/scrolling_space.rs`
   orchestrates `mutate → project → animate` and is the only thing that touches
   all three layers.
2. **The daemon is the single coordinator.**
   `FlowWM` (`src/daemon/types.rs`) owns every subsystem and
   routes events between them. No subsystem knows about any other subsystem —
   they only expose methods that take inputs and return outputs.
