# Animation

The animation layer moves windows smoothly between layout positions. It is an
in-tree copy of a standalone `window-animation` crate, embedded directly into the
project rather than pulled as a dependency. This embedding strategy keeps the
animation code tightly coupled to the project's needs (specifically the
`InvisibleBounds` translation and the `AppliedLayout` bridge) while retaining a
clean backend abstraction for testing. See [design decisions](./design-decisions.md)
for the embedding rationale.

## Public API Surface

The animation layer exposes a small set of types through
[`src/animation/mod.rs`](../../src/animation/mod.rs):

- **`WindowAnimator`** — the main entry point. Owns a background worker thread
  that drives the frame loop. Submit animation batches with `animate()`.
- **`WindowTarget`** — desired final geometry for one window (window ref +
  target position + target size).
- **`WindowRef`** — opaque HWND wrapper (`isize` for `Send` safety).
- **`IVec2`** — 2D integer vector for positions and sizes.
- **`AnimRect`** — the animation engine's rectangle type, `{x, y, w, h}`.
- **`AnimatorConfig`** — duration, easing curves, interrupt policy, frame pacing.

The `AnimRect` type uses `{w, h}` for its size fields. This is intentionally
different from flow's `common::Rect` which uses `{width, height}`. The two types
coexist in different modules and are converted at integration boundaries. The
daemon bridge handles this conversion transparently.

## Data Flow

When a layout mutation produces an `AppliedLayout`, the daemon calls
`animate_layout()`, which converts the layout into animation targets and submits
them to the animator's worker thread.

```mermaid
flowchart TB
    SUB["ScrollingSpace<br/>layout mutation"]
    AL["AppliedLayout<br/>(virtual + actual)"]
    BRIDGE["daemon::animate_layout()<br/>visible-to-window-rect<br/>translation"]
    TARGETS["Vec&lt;WindowTarget&gt;"]
    ANIM["WindowAnimator::animate()<br/>build_tweens()"]
    TWEEN["AnimationBatch<br/>per-window tweens"]
    BACKEND["WindowBackend<br/>(Win32 or Mock)"]
    SWP["SetWindowPos per window"]

    SUB --> AL
    AL --> BRIDGE
    BRIDGE --> TARGETS
    TARGETS --> ANIM
    ANIM --> TWEEN
    TWEEN --> BACKEND
    BACKEND --> SWP
```

## The Frame Loop

`WindowAnimator` spawns a dedicated worker thread at construction time. The
worker drives a continuous loop:

1. When idle, it blocks on a `crossbeam_channel` waiting for commands.
2. When an `Animate` command arrives, it builds per-window tweens (from rect,
   to rect) and enters the frame loop.
3. Each frame, it samples the normalised progress `t` from wall-clock elapsed
   time, computes the interpolated rect for each tween, and calls
   `backend.apply_batch()` to reposition the windows.
4. After applying, it calls `backend.dwm_flush()`, which blocks until the
   Desktop Window Manager completes its current composition cycle (~16ms at 60Hz).
   This is the frame-pacing mechanism: rather than a fixed `sleep` interval,
   `DwmFlush` synchronises the animation with the display refresh rate.
5. The batch is cleared only when `t` reaches exactly 1.0 — the frame that was
   applied at the precise target rect. This prevents the "resizing column width
   comes out wrong" bug where `progress()` ticks past 1.0 during the
   `SetWindowPos` round-trip and the batch is retired without ever applying
   the final frame.

Progress is clamped to `[0.0, 1.0]` and never exceeds 1.0, even if wall-clock
time far exceeds the animation duration. This invariant makes `t >= 1.0` a
precise and safe completion signal.

## Easing Functions

Position and size channels are eased independently. The animation module
([`src/animation/easing.rs`](../../src/animation/easing.rs)) provides a
comprehensive catalog of 31 named easing curves plus arbitrary CSS cubic-bezier
curves (solved via Newton-Raphson iteration):

| Category | Variants |
|----------|----------|
| Linear | `Linear` |
| Sine | `EaseInSine`, `EaseOutSine`, `EaseInOutSine` |
| Quadratic | `EaseInQuad`, `EaseOutQuad`, `EaseInOutQuad` |
| Cubic | `EaseInCubic`, `EaseOutCubic`, `EaseInOutCubic` |
| Quartic | `EaseInQuart`, `EaseOutQuart`, `EaseInOutQuart` |
| Quintic | `EaseInQuint`, `EaseOutQuint`, `EaseInOutQuint` |
| Exponential | `EaseInExpo`, `EaseOutExpo`, `EaseInOutExpo` |
| Circular | `EaseInCirc`, `EaseOutCirc`, `EaseInOutCirc` |
| Back | `EaseInBack`, `EaseOutBack`, `EaseInOutBack` |
| Elastic | `EaseInElastic`, `EaseOutElastic`, `EaseInOutElastic` |
| Bounce | `EaseInBounce`, `EaseOutBounce`, `EaseInOutBounce` |
| Custom | `CubicBezier(x1, y1, x2, y2)` |

The default config uses `EaseInOut` (maps to `EaseInOutCubic`) for position and
`Linear` for size. Position curves can be more expressive because resize-heavy
transitions look worse on Windows — window content must reflow each frame.

Size animation supports a `DisabledIfUnchanged` mode: when a window's width and
height already match the target, the size channels are skipped and the move is
treated as a pure translation. This avoids unnecessary content reflow.

## Batch Scheduling

The animation layer uses `AnimationBatch` ([`src/animation/batch.rs`](../../src/animation/batch.rs))
to manage all per-window tweens for one animation run. Key design points:

- **No-op filtering**: windows whose current rect already matches the target are
  silently excluded from the batch. This avoids wasting frames on windows that
  don't need to move.
- **Independent easing per channel**: position (x, y) and size (w, h) are eased
  with separate curves, allowing a fast position move combined with a slower
  size transition.
- **Wall-clock timing**: progress is computed from `Instant::now()` at batch
  start, not from frame counting. This makes animation duration independent of
  frame rate.

## Interrupt Policies

When a new `animate()` call arrives while an animation is already playing, the
configurable interrupt policy decides what happens:

| Policy | Behaviour |
|--------|-----------|
| `RetargetFromCurrent` (default) | Sample each window's current interpolated position, cancel the active batch, and start a new batch from those positions toward the new targets. Preserves visual continuity. |
| `QueueAfterCurrent` | Finish the current animation, then start the queued one. |
| `DropNew` | Silently discard the incoming request. |

The default `RetargetFromCurrent` policy is what enables smooth rapid-fire
operations (e.g., holding the scroll key). Each successive layout mutation
retargets windows from wherever they currently appear on screen, not from their
original position.

## The Two Backends

The `WindowBackend` trait ([`src/animation/backend/mod.rs`](../../src/animation/backend/mod.rs))
decouples the animation engine from Win32:

```mermaid
classDiagram
    class WindowBackend {
        <<trait>>
        +get_window_rect(WindowRef) Rect
        +apply_batch(updates) Result
        +dwm_flush() Result
    }
    class Win32Backend {
        Individual SetWindowPos calls
        + DwmFlush for vsync pacing
    }
    class MockBackend {
        In-memory HashMap of rects
        Records call history for assertions
    }
    WindowBackend <|-- Win32Backend
    WindowBackend <|-- MockBackend
```

**`Win32Backend`** ([`src/animation/backend/win32.rs`](../../src/animation/backend/win32.rs))
uses individual `SetWindowPos` calls rather than the batch `DeferWindowPos` API.
This is a deliberate resilience choice: `DeferWindowPos` is atomic, so a single
elevated admin window (protected by UIPI) causes the entire batch to fail with
`ERROR_ACCESS_DENIED`. Individual calls mean one failure logs a warning but does
not block the remaining windows.

Each call goes through `place_window`, the one helper that positions a managed
window (the teleports in `registry::win32::set_window_rect`, fail-back and
`flow rescue` use it too). It reads the window's current rect and lets the pure
`swp_flags(current, next, virtual_screen)` pick the flags: always
`SWP_NOZORDER | SWP_NOACTIVATE`; plus `SWP_NOSIZE` when the size is unchanged,
so a tween frame is a pure move; plus `SWP_NOSENDCHANGING` when the size changes
and the target lies entirely off the virtual screen (a parking slot), so the app
cannot rewrite a parked position; the plain flags for an on-screen resize and
for the first placement back from a parking slot, so the app's min/max tracking
size applies there. Visual Studio-shell windows (SSMS 21) answer a *sized* move
past the right edge by moving themselves to `x = -width`, onto the left monitor
(ADR-0019, `docs/adr/0019-pure-moves-and-silent-parked-resizes.md`).

**`MockBackend`** ([`src/animation/backend/mock.rs`](../../src/animation/backend/mock.rs))
stores rects in a `HashMap` and records call history. Always compiled (no feature
gate) so both unit tests and integration tests can use it freely.

## The Bridge: daemon::animation

The daemon's animation bridge ([`src/daemon/animation.rs`](../../src/daemon/animation.rs))
is the integration point between the layout engine and the animation system. Its
responsibilities:

1. **Registry sync**: always calls `update_tiling_slots_from_layout()` and
   `update_tiled_rects()` on the registry, even when no windows physically moved.
   Logical positions (col/row) may change without pixel-level moves.

2. **Visible-rect to window-rect translation**: the layout engine computes
   visible rects (what the user sees), but `SetWindowPos` uses window rects
   (which include invisible borders). The bridge looks up each window's
   `InvisibleBounds` from the registry and calls `visible_to_window()` to
   translate. Without this, windows would appear with gaps larger than
   configured.

3. **Type conversion**: maps flow's `WindowId(isize)` to `WindowRef(isize)`,
   and the visible rect's `{width, height}` to `IVec2::new(width, height)`.

4. **Submit all windows**: every entry in `actual_layout` becomes a target,
   not just the ones that changed. The animator's `build_tweens` drops no-ops
   internally, but passing all windows ensures that mid-flight retargeting
   works correctly even when a window's target rect didn't change.

A standalone `animate_layout_raw()` function performs the same conversion but
takes `&mut WindowAnimator` directly. This is used during daemon construction
when `FlowWM` doesn't exist yet but the animator needs to snap
windows to their initial positions (with `Duration::ZERO` for an instant snap).

## Mid-Flight Retargeting

If a new layout arrives while a previous animation is still running, the
`RetargetFromCurrent` policy ensures smooth transitions. The worker samples
each window's current interpolated position from the active batch (using the
batch's frozen config for consistency) and starts a new batch from those
positions toward the new targets. This means rapid-fire operations like
scrolling or swapping produce continuous motion with no visible restarts.

## Instant targets and the neighbour-teleport rule

On a multi-monitor desktop a column that leaves the viewport is *parked*
beyond the Win32 virtual screen (see [Projection](./layout/projection.md)). A
tween from the viewport to a parking slot that lies behind a neighbouring
monitor would sweep the window across that monitor, so the bridge replaces it
with an instant move.

**The rule** (`daemon/animation.rs`, `plan_move`): a move is **instant** when
*either* endpoint is a parking slot on a side that has a neighbour
(`MonitorInfo::neighbour_left/right/up/down`) — sliding out toward the
neighbour, sliding in from behind it, or sliding from one blocked slot across
to the other. Parking on a free side keeps the ordinary slide, as does any
on-screen to on-screen move. When the previous rect equals the new one *and*
that slot is on a blocked side, the window is already there (its last move was
instant, so no tween can be in flight) and **no target is emitted** at all
(`Move::Stay`), so a layout does not re-`SetWindowPos` every parked window.
Free-side parked windows are always resubmitted because their slide may still
be mid-flight and a superseding batch would otherwise strand them.

`Stay` is only sound when "the previous rect" is the rect the window was
**last sent to by any path**, not the registry's last *committed* rect: drag
and resize previews, bystander teleports and the hotplug re-seat all move
windows without committing. `FlowWM::submitted_rects` keeps that per-HWND
record — the visible rect with any workspace offset applied, written by every
submit path (`push_targets` collects it, the teleports record only a
`SetWindowPos` that succeeded) and read first by every target builder, with
the registry rect or the caller's snapshot as the fallback for a window never
submitted since start-up. Without it a parked column that one preview brought
on-screen would be told to `Stay` parked by the next preview (left overhanging
the neighbour), or the preview after that would tween it from a far slot it no
longer occupies. The entry is dropped when the window leaves the layout
(destroyed, minimized, hidden).

The rule covers both axes. Columns park **left/right** of the virtual screen;
inactive workspaces park **above/below** it when the monitor has a display
stacked on that side (`WorkspaceParking`, see
[Projection](./layout/projection.md#neighbours-on-four-sides)), and one
monitor height away — which may or may not leave the virtual screen — when the
side is free. `park_sides` classifies a rect on each axis independently and a
rect is blocked when **any** of its parked sides has a neighbour: a column
parked right (free) inside a workspace parked above a stacked display (blocked)
teleports, and so does a column parked behind a left neighbour inside a
workspace parked on a free vertical side. `side_blocked` is the one place that
reads the four flags, so a workspace switch, a loadout seating pass and a
column scroll all make the same decision through `push_targets`. On a stacked
pair with both vertical sides blocked every workspace switch is instant; with
one side free the strip still slides in from (or out to) that side.

Every submit path builds its targets through one helper (`push_targets`):
`animate_layout`'s `collect_targets`, `animate_workspaces[_on]` (workspace
switches, float toggles, the loadout apply) and the construction-time
`animate_layout_raw`. The decision needs each window's **pre-batch** rect:
the last submitted one when there is a record (see above), else the one the
caller names — callers submit before syncing the registry's tiled rects
(`PrevRects::Registry`), or capture the rects first and pass
`PrevRects::Snapshot`. The registry's rects are *workspace-local* — a window
in a parked workspace is recorded at its on-screen layout rect — so
`PrevRects::Registry` is only right for the active workspace's windows (the
float toggles). Anything that moves a parked workspace (a switch, the loadout
seating) captures `snapshot_stacked_prev_rects`, which shifts each window by
its workspace's current parking offset; without that, a workspace returning
from behind a stacked neighbour reads as an on-screen → on-screen move and
tweens down through it. A window's border overlay always follows the window's
decision so both move in lockstep.

**Inside the animator** an instant move is a `WindowTarget` with
`instant: true` (`WindowTarget::instant` / `with_instant`). When the worker
starts a batch it first writes every instant target through the backend in a
single `apply_batch`, evicts those windows from any batch still in flight
(so no later frame of the superseded tween moves them again — under
`RetargetFromCurrent` the old tween's interpolated position would otherwise
become the new tween's start and the window would slide across the
neighbour anyway), and then uses the instant rect as the window's *from*
rect so `build_tweens` drops it as a no-op. Instant targets never query
`GetWindowRect`. A batch that contains only instant targets leaves the
animator idle.

**Book-mode piles** (ADR-0022, [Book Mode: Piles](./piles.md)). A window
whose target is a pile slot (`ActualEntry::pile`) is placed instantly by
the layout paths (`collect_targets` → `push_targets_as`): it lands under its
pile's top, already seated below it, while a tween would cross the visible
columns. With the animator idle, a member whose target is the rect it was
last sent to gets `Stay`, so a flip moves only the windows that change
place. Workspace batches leave piles to travel with their workspace.

## Cross-References

- [Layout pipeline](./layout/pipeline.md) — how `AppliedLayout` is produced by
  the mutate-then-project pipeline.
- [Event pipelines](./event-pipelines.md) — when `animate_layout` is called in
  response to hook events and IPC commands.
- [Window registry](./window-registry.md) — how `InvisibleBounds` is measured
  and stored per window.
