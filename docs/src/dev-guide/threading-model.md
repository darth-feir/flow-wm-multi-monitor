# Threading Model

The `flowd` daemon uses exactly two threads and zero runtime locks. The hook thread
runs Win32 event hooks in the background; the IPC thread owns all application state
and processes both hook events and IPC commands. There is no `Arc<Mutex<T>>` anywhere
in the daemon — the borrow checker enforces exclusive access at compile time, which
is strictly safer than `Mutex` (runtime-only enforcement, potential deadlocks).

Two helper threads own no daemon state: the animator's frame worker
(`window-animation-worker`, fed over a channel, see [Animation](./animation.md))
and the pipe's accept thread, which only blocks in `ConnectNamedPipe` and
signals an event (`src/ipc/transport.rs`). The two threads this chapter is about
are the ones that see events and state.

## The Two Threads

```mermaid
sequenceDiagram
    participant OS as Windows OS
    participant Hook as Hook Thread
    participant Chan as mpsc Channel
    participant IPC as IPC Thread (main)
    participant Subsystems as Registry / ScrollingSpace / Animator

    Note over Hook: SetWinEventHook x9<br/>GetMessageW loop
    Note over IPC: owns FlowWM<br/>(all fields)

    OS->>Hook: WinEvent callback
    Hook->>Chan: sender.send(HookEvent)
    Hook->>IPC: SetEvent(hook_signal)
    IPC->>IPC: MsgWaitForMultipleObjects wakes
    IPC->>IPC: pump_messages()
    IPC->>IPC: ResetEvent(hook_signal)
    IPC->>Chan: try_recv() loop (drain all)
    IPC->>Subsystems: process_hook_events() via &mut self

    Note over IPC: ... later ...

    participant CLI as flow CLI
    CLI->>IPC: ConnectNamedPipe (background thread)
    Note over IPC: connected_event signaled
    IPC->>IPC: read_message() (blocking)
    IPC->>Subsystems: dispatch(msg) via &mut self
    IPC->>CLI: write_response()
```

### Hook Thread (background)

- Registers nine `SetWinEventHook` callbacks covering window lifecycle (create,
  destroy), focus changes, move/size start and end, minimize/restore, show/hide,
  state changes, name changes, DWM cloak/uncloak, and location changes.
  `EVENT_OBJECT_LOCATIONCHANGE` fires on every pixel of movement system-wide, so
  the callback drops it unless the window is a tracked float (while float
  tracking is on), the window being dragged, the foreground window, or a window
  watched for an app full-screen switch; only those reach the channel.
- Runs a `GetMessageW` message loop (required for `WINEVENT_OUTOFCONTEXT`).
- On each callback: sends a typed `HookEvent` through the `mpsc::Sender` and
  signals a Win32 manual-reset event (`SetEvent`) to wake the IPC thread.
- **Never touches any daemon state.** It does not hold references to
  `FlowWM`, the registry, or the layout. Its entire output is the
  `HookEvent` enum and a signal.
- Cleaned up by dropping the `HookThreadHandle`, which posts `WM_QUIT` to the
  hook thread's message loop. All hooks are unregistered before the thread exits.

### IPC Thread (main)

- Owns `FlowWM` and all subsystems directly — no `Arc`, no `Mutex`.
- Runs the main event loop via `MsgWaitForMultipleObjects`, waiting on two handles
  plus the thread's message queue (`QS_ALLINPUT`):
  1. **hook_signal** (index 0) — wakes when any `HookEvent` is sent.
  2. **connected_event** (index 1) — wakes when a CLI client connects.
  3. A window message for a window this thread owns (border overlays, the
     session sink).
- On every wake it first pumps window messages (`pump_messages()`, so the
  animator's cross-thread `SetWindowPos` on an overlay never blocks), then
  calls `process_hook_events()` to drain the channel, then serves an IPC
  session if a client connected. The connection is tested with a zero-timeout
  `WaitForSingleObject(connected_event)`, not the wait's return index: the
  hook signal is index 0 and would otherwise shadow it.
- All subsystem methods are called with `&mut self` — the borrow checker
  guarantees exclusive access at compile time.

## Why No `Arc<Mutex>`

The previous architecture (before `FlowWM` was introduced) used
`Arc<Mutex<WindowRegistry>>` because the IPC loop and hook events were consumed by
different parts of the code with no single coordination point. This had several
problems:

1. **Runtime-only safety.** `Mutex` only catches data races at runtime. A missed
   lock or a lock held too long causes undefined behavior or deadlocks — errors
   the compiler cannot see.
2. **No ownership clarity.** When every subsystem is behind an `Arc<Mutex>`,
   it is unclear *who* is responsible for routing events between them.
3. **Deadlock risk.** Nested locking (lock A, then B in one code path; B then A
   in another) is easy to introduce and hard to detect.

With `FlowWM` owning everything on one thread, all subsystem methods
take `&mut self`:

- The borrow checker rejects any code that would access a subsystem while another
  part of the code holds a mutable reference to it.
- There is no possibility of deadlock because there is no locking.
- Event routing is centralized — the daemon is the only place that calls subsystem
  methods, so the flow is always visible in one place.

The trade-off is that hook events are not processed *instantly* in the callback —
they are dispatched asynchronously through the channel and drained on the next
loop iteration. In practice, the signal event wakes `WaitForMultipleObjects`
within microseconds, so the latency is imperceptible.

## The Main Event Loop

The `run()` method in `src/daemon/run.rs` implements the
event-driven loop:

```mermaid
flowchart TB
    Start["start_accept()"] --> Loop["MsgWaitForMultipleObjects"]
    Loop -->|"hook_signal, message,<br/>connect or timeout"| Pump["pump_messages()"]
    Pump --> Reset["ResetEvent(hook_signal)"]
    Reset --> Drain["process_hook_events()"]
    Drain --> CheckIPC{"connected_event<br/>signaled?"}
    CheckIPC -->|"yes"| InnerLoop["IPC session loop"]
    CheckIPC -->|"no"| Loop
    InnerLoop --> PreRead["process_hook_events()"]
    PreRead --> Read["read_message()"]
    Read --> Dispatch["dispatch(msg)"]
    Dispatch --> Write["write_response()"]
    Write -->|"shutting_down?"| Exit["return"]
    Write -->|"more messages"| PreRead
    Write -->|"disconnect"| Disconnect["disconnect()"]
    Disconnect --> Accept["start_accept()"]
    Accept --> Loop
```

Key details:

- **Race-free drain.** `ResetEvent` is called *before* `try_recv()`. Any event
  pushed between the reset and the drain is caught by `try_recv`. Any event
  pushed after the drain sets the signal again, so the next `WaitForMultipleObjects`
  wakes immediately. No events are lost.
- **Pending-creation retries.** `EVENT_OBJECT_CREATE` fires before windows are
  fully initialized (not yet visible, no title, styles not finalized). Windows
  that fail classification are added to `pending_creations` and retried on each
  `process_hook_events()` call. When this list is non-empty, the wait timeout
  is at most 100 ms, ensuring retries happen even without new events.
- **The wait is never infinite.** Its timeout is the soonest of: the 100 ms
  pending-creation retry, the float-tracking resume deadline, the next
  foreground reconciliation (`[focus] foreground_sync_interval_ms`), a drag's
  edge-scroll repeat, the display-topology settle deadline, and the 1 s liveness
  tick (`LIVENESS_TICK_MS`) that checks the helper threads and the config files.
- **IPC is one-shot.** The CLI connects, sends one command, reads the response,
  and disconnects. The inner loop rarely runs more than once.
- **Hook events are drained between IPC reads.** Inside the IPC inner loop,
  `process_hook_events()` runs before every `read_message()`, so layout changes
  from hook events are never delayed by an active IPC session.

## Hook Registration

Nine hooks are registered (`register_hooks` in `src/registry/hooks.rs`):
five ranges and four single-event hooks. `STATECHANGE` (`0x800A`),
`LOCATIONCHANGE` (`0x800B`) and `NAMECHANGE` (`0x800C`) each get a hook of their
own so the ranges stay disjoint, and so the noisy `LOCATIONCHANGE` can be
filtered in the callback (see above) before anything is sent.

| Hook | Event Range | Purpose |
|------|-------------|---------|
| Create / Destroy | `0x8000` -- `0x8001` | Window lifecycle |
| Foreground | `0x0003` | Focus changes |
| Move / Size | `0x000A` -- `0x000B` | Tile-drag and resize-drag detection |
| Minimize / Restore | `0x0016` -- `0x0017` | Minimize and restore |
| Show / Hide | `0x8002` -- `0x8003` | Tray-hide, re-show |
| State Change | `0x800A` (single) | Maximized/fullscreen recovery |
| Name Change | `0x800C` (single) | Late-title recovery |
| Location Change | `0x800B` (single, filtered) | Float and drag position tracking, foreground resize, app full-screen watch |
| Cloak / Uncloak | `0x8017` -- `0x8018` | UWP frames: registered when DWM uncloaks them, hidden while cloaked |

`flow ping` reports the registered-hook count.

The recovery hooks (`STATECHANGE` and `NAMECHANGE`) handle windows that
`EVENT_OBJECT_CREATE` misses — see [Event Pipelines](./event-pipelines.md) for the
full recovery flows.
