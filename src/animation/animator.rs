//! Background-threaded window animator.
//!
//! [`WindowAnimator`] owns a single worker thread that drives the animation
//! loop. The public API sends commands over a [`crossbeam_channel`] and
//! reads the [`AtomicBool`] flag to report animation status without locking.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::animation::backend::WindowBackend;
use crate::animation::batch::{AnimationBatch, build_tweens};
use crate::animation::config::{AnimatorConfig, InterruptPolicy};
use crate::animation::types::{
    AnimationError, AnimationHandle, Rect, Result, WindowRef, WindowTarget,
};

// ---------------------------------------------------------------------------
// Internal command type
// ---------------------------------------------------------------------------

/// Commands sent from [`WindowAnimator`] to the worker thread.
enum AnimatorCmd {
    /// Start or interrupt an animation with the given targets.
    Animate {
        targets: Vec<WindowTarget>,
        config: AnimatorConfig,
    },
    /// Cancel any in-flight animation and clear the queue. When `ack` is
    /// `Some`, the worker sends `()` on it once the cancel has taken effect
    /// (see [`WindowAnimator::cancel_and_wait`]).
    Cancel {
        ack: Option<crossbeam_channel::Sender<()>>,
    },
    /// Replace the config for future animations.
    UpdateConfig(AnimatorConfig),
    /// Ask the worker to exit cleanly.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Internal worker-state types
// ---------------------------------------------------------------------------

/// An animation batch currently being driven by the worker.
struct ActiveBatch {
    batch: AnimationBatch,
    /// Configuration that was active when this batch was started.
    /// Frozen at start time so mid-animation `update_config` calls cannot
    /// alter the easing curve of an in-flight animation.
    config: AnimatorConfig,
}

/// An animation request waiting in the queue (used by [`InterruptPolicy::QueueAfterCurrent`]).
struct PendingBatch {
    targets: Vec<WindowTarget>,
    config: AnimatorConfig,
}

/// How long [`WindowAnimator::cancel_and_wait`] waits for the worker to
/// acknowledge a cancel before giving up. One frame is ~16 ms, so a healthy
/// worker answers well within this; the bound only matters when a backend
/// call (`SetWindowPos` to a hung window) is wedged.
pub const CANCEL_ACK_TIMEOUT: Duration = Duration::from_millis(500);

/// All mutable state owned exclusively by the worker thread.
struct WorkerState {
    /// Last config seen by the worker. Informational only — all animation
    /// decisions use `active.config` (frozen per batch) or the config
    /// embedded in `AnimatorCmd::Animate`. Updated by `UpdateConfig` and
    /// by `start_batch` so it stays in sync for potential future introspection.
    config: AnimatorConfig,
    active: Option<ActiveBatch>,
    queue: VecDeque<PendingBatch>,
}

// ---------------------------------------------------------------------------
// WindowAnimator — public API
// ---------------------------------------------------------------------------

/// Rect-based window animator backed by a dedicated worker thread.
///
/// Submit animation batches with [`Self::animate`]; the worker thread drives
/// the frame loop using the configured backend (Win32 on Windows, mock in
/// tests). Configuration can be updated at any time via [`Self::update_config`].
///
/// # Thread Safety
///
/// `WindowAnimator` is not `Sync` — all methods take `&mut self` to prevent
/// concurrent use from multiple threads. The worker thread communicates
/// exclusively through the internal channel and an [`AtomicBool`].
pub struct WindowAnimator {
    /// Current config; cloned into commands so the worker always receives it.
    config: AnimatorConfig,
    /// Sends commands to the worker thread.
    cmd_tx: crossbeam_channel::Sender<AnimatorCmd>,
    /// Set to `true` by the worker when an animation starts; cleared on completion or cancel.
    animating: Arc<AtomicBool>,
    /// Monotonically-increasing counter used to generate [`AnimationHandle`] IDs.
    next_id: Arc<AtomicU64>,
    /// `true` while the worker thread is running; cleared by the worker on
    /// the way out — whether it exited cleanly or panicked.
    alive: Arc<AtomicBool>,
    /// Worker thread handle. Kept for its drop behaviour (detaches thread on drop).
    _worker: std::thread::JoinHandle<()>,
}

impl WindowAnimator {
    /// Create a new animator with the given backend and initial configuration.
    ///
    /// Spawns the worker thread immediately. The thread exits when this
    /// `WindowAnimator` is dropped.
    pub fn new(backend: impl WindowBackend, config: AnimatorConfig) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<AnimatorCmd>();
        let animating = Arc::new(AtomicBool::new(false));
        let next_id = Arc::new(AtomicU64::new(1));
        let alive = Arc::new(AtomicBool::new(true));

        let animating_worker = Arc::clone(&animating);
        let alive_worker = Arc::clone(&alive);
        let initial_config = config.clone();

        // SAFETY / rationale: thread spawn can fail only if the OS has
        // exhausted thread resources, which is unrecoverable for this crate.
        // `.expect()` is the documented exception to the no-unwrap rule.
        let _worker = std::thread::Builder::new()
            .name("window-animation-worker".into())
            .spawn(move || {
                // A panic inside the frame loop must not vanish silently: the
                // process-wide panic hook (if any) has already logged it by
                // the time we get here; we additionally mark the worker dead
                // so the daemon's event loop can notice and fail back. The
                // channel receiver is dropped on unwind, so any later
                // `animate()` also fails with `WorkerDead`.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_worker(Box::new(backend), cmd_rx, animating_worker, initial_config);
                }));
                alive_worker.store(false, Ordering::Release);
                if let Err(payload) = result {
                    log::error!(
                        "animation worker thread panicked: {}",
                        panic_payload_message(payload.as_ref())
                    );
                }
            })
            .expect("failed to spawn animation worker thread");

        Self {
            config,
            cmd_tx,
            animating,
            next_id,
            alive,
            _worker,
        }
    }

    /// Submit a new animation batch.
    ///
    /// Behaviour when an animation is already playing depends on
    /// `config.interrupt_policy`:
    /// - [`InterruptPolicy::RetargetFromCurrent`]: cancel the active batch and
    ///   start the new one from the current interpolated positions.
    /// - [`InterruptPolicy::QueueAfterCurrent`]: finish the current batch then
    ///   play the new one.
    /// - [`InterruptPolicy::DropNew`]: discard the new request silently.
    ///
    /// # Errors
    ///
    /// Returns [`AnimationError::EmptyBatch`] if `targets` is empty.
    /// Returns [`AnimationError::WorkerDead`] if the worker thread has exited.
    pub fn animate(&mut self, targets: Vec<WindowTarget>) -> Result<AnimationHandle> {
        if targets.is_empty() {
            return Err(AnimationError::EmptyBatch);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cmd = AnimatorCmd::Animate {
            targets,
            config: self.config.clone(),
        };
        self.cmd_tx
            .send(cmd)
            .map_err(|_| AnimationError::WorkerDead)?;
        Ok(AnimationHandle(id))
    }

    /// Replace the configuration.
    ///
    /// Takes effect at the start of the next animation. Any animation currently
    /// in flight continues under its original configuration.
    pub fn update_config(&mut self, config: AnimatorConfig) {
        self.config = config.clone();
        // Best-effort: ignore send error if the worker has already exited.
        let _ = self.cmd_tx.send(AnimatorCmd::UpdateConfig(config));
    }

    /// Returns `true` while an animation is in progress.
    pub fn is_animating(&self) -> bool {
        self.animating.load(Ordering::Acquire)
    }

    /// Cancel any in-flight animation immediately and clear the pending queue.
    ///
    /// Asynchronous: the worker may still be inside the frame it was
    /// applying when this returns. Use [`cancel_and_wait`](Self::cancel_and_wait)
    /// when the caller is about to move windows itself (the shutdown rescue).
    pub fn cancel(&mut self) {
        // Best-effort: ignore send error if the worker has already exited.
        let _ = self.cmd_tx.send(AnimatorCmd::Cancel { ack: None });
    }

    /// Cancel any in-flight animation and **block** until the worker has
    /// acknowledged it (bounded by [`CANCEL_ACK_TIMEOUT`]).
    ///
    /// Once this returns `true`, the worker has cleared its active batch and
    /// is parked on the command channel: it will issue no further
    /// `apply_batch` (`SetWindowPos`) calls unless a new `animate()` is
    /// submitted. The daemon calls this before the shutdown rescue so a
    /// still-running tween cannot overwrite the rescued positions.
    ///
    /// Returns `false` if the worker did not answer in time (it is wedged in
    /// a backend call or has died); the caller should proceed anyway — a dead
    /// worker cannot move windows, and a wedged one is logged at `warn`.
    pub fn cancel_and_wait(&mut self) -> bool {
        let (ack_tx, ack_rx) = crossbeam_channel::bounded::<()>(1);
        if self
            .cmd_tx
            .send(AnimatorCmd::Cancel { ack: Some(ack_tx) })
            .is_err()
        {
            // Worker gone: nothing can be animating, so the guarantee holds
            // trivially. Report it honestly though — the caller may want to
            // know the thread is dead.
            log::warn!("cancel_and_wait: animation worker is not running");
            return false;
        }
        match ack_rx.recv_timeout(CANCEL_ACK_TIMEOUT) {
            Ok(()) => true,
            Err(_) => {
                log::warn!(
                    "cancel_and_wait: animation worker did not acknowledge within {:?}",
                    CANCEL_ACK_TIMEOUT
                );
                false
            }
        }
    }

    /// `true` while the worker thread is running.
    ///
    /// Becomes `false` when the worker exits — on `Drop`/shutdown, or
    /// because its frame loop panicked. The daemon polls this from its event
    /// loop and fails back when the worker dies underneath it.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

/// Best-effort text of a panic payload (`&str` or `String`), else a fixed
/// placeholder.
pub fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_owned()
    }
}

impl Drop for WindowAnimator {
    fn drop(&mut self) {
        // Ask the worker to exit cleanly.
        // Note: JoinHandle::join() requires ownership, which cannot be taken
        // from &mut self, so we only signal — the worker exits on its own.
        let _ = self.cmd_tx.send(AnimatorCmd::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

/// Entry-point for the background worker thread.
///
/// Drives the animation loop: when idle it blocks on the channel; when
/// animating it polls commands between frames.
fn run_worker(
    backend: Box<dyn WindowBackend>,
    rx: crossbeam_channel::Receiver<AnimatorCmd>,
    animating: Arc<AtomicBool>,
    initial_config: AnimatorConfig,
) {
    let mut state = WorkerState {
        config: initial_config,
        active: None,
        queue: VecDeque::new(),
    };

    'main: loop {
        // When animating: non-blocking poll so we can tick the frame loop.
        // When idle: block on the channel to avoid busy-spinning.
        let cmd_opt: Option<AnimatorCmd> = if state.active.is_some() {
            rx.try_recv().ok()
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break 'main, // channel disconnected — exit cleanly
            }
        };

        if let Some(cmd) = cmd_opt {
            match cmd {
                AnimatorCmd::Shutdown => break 'main,
                AnimatorCmd::Cancel { ack } => {
                    state.active = None;
                    state.queue.clear();
                    animating.store(false, Ordering::Release);
                    // Acknowledge only after the batch is gone: with `active`
                    // cleared, the tick below is skipped and the next
                    // iteration blocks on `recv()`, so no `apply_batch` can
                    // follow this send.
                    if let Some(ack) = ack {
                        let _ = ack.send(());
                    }
                }
                AnimatorCmd::UpdateConfig(cfg) => {
                    state.config = cfg;
                }
                AnimatorCmd::Animate { targets, config } => {
                    handle_animate(&mut state, &*backend, targets, config, &animating);
                }
            }
        }

        // Tick the active animation, if there is one.
        if let Some(active) = &state.active {
            let t = active.batch.progress();

            // Gather interpolated positions for this frame.
            let updates: Vec<(WindowRef, Rect)> = active
                .batch
                .tweens
                .iter()
                .map(|tween| {
                    // Use the config frozen at batch-start time, not the current
                    // state.config, so mid-animation update_config() has no effect.
                    let r = AnimationBatch::interpolated_rect(tween, &active.config, t);
                    (tween.window_ref, r)
                })
                .collect();

            // Apply the batch — best-effort; backend errors are non-fatal here.
            if let Err(e) = backend.apply_batch(&updates) {
                log::warn!("apply_batch error at t={t:.3}: {e}");
            }

            // Use the same clamped `t` we just applied — NOT a fresh
            // `is_complete()` / `progress()` re-read. The `apply_batch` call
            // above (SetWindowPos round-trip) consumes wall-clock time, so a
            // second progress() sample can tick past 1.0 *while this frame was
            // interpolated at `t < 1.0`*. That would clear the batch without
            // ever applying the precise t = 1.0 frame, leaving windows a few
            // pixels short of their target geometry (the "resizing column
            // width comes out wrong" bug). Since `progress()` already clamps
            // to 1.0, gating completion on this same `t` guarantees the batch
            // is only cleared on the frame that was applied at exactly the
            // target rect.
            if t >= 1.0 {
                // Animation complete — clear the active batch.
                log::trace!("animation complete: batch finished at t={t:.3}");
                state.active = None;
                animating.store(false, Ordering::Release);

                // Start the next queued batch if one is waiting.
                if let Some(pending) = state.queue.pop_front() {
                    start_batch(&mut state, &*backend, pending, &animating);
                }
            } else {
                // Pace with DwmFlush — blocks until the next DWM composition
                // cycle (~16 ms at 60 Hz). Non-fatal if it fails.
                let _ = backend.dwm_flush();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// handle_animate — interrupt policy dispatcher
// ---------------------------------------------------------------------------

/// Dispatch a new [`AnimatorCmd::Animate`] command according to the configured
/// interrupt policy.
fn handle_animate(
    state: &mut WorkerState,
    backend: &dyn WindowBackend,
    targets: Vec<WindowTarget>,
    config: AnimatorConfig,
    animating: &Arc<AtomicBool>,
) {
    match config.interrupt_policy {
        InterruptPolicy::DropNew => {
            if state.active.is_some() {
                // Silently discard the incoming request.
                return;
            }
        }
        InterruptPolicy::QueueAfterCurrent => {
            if state.active.is_some() {
                state.queue.push_back(PendingBatch { targets, config });
                return;
            }
        }
        InterruptPolicy::RetargetFromCurrent => {
            // Fall through: start_batch will sample current interpolated
            // positions and replace the active batch.
        }
    }

    let pending = PendingBatch { targets, config };
    start_batch(state, backend, pending, animating);
}

// ---------------------------------------------------------------------------
// start_batch — build tweens and activate
// ---------------------------------------------------------------------------

/// Build an [`AnimationBatch`] from the current window geometry and make it
/// the active animation.
///
/// For windows that are mid-animation, the current interpolated position is
/// used as the starting rect so the transition appears continuous. For all
/// other windows the backend is queried.
///
/// If all tweens are no-ops (source rect == target rect) the function returns
/// early without starting a batch.
fn start_batch(
    state: &mut WorkerState,
    backend: &dyn WindowBackend,
    pending: PendingBatch,
    animating: &Arc<AtomicBool>,
) {
    // Sample current progress of the active batch, if any.
    // `progress()` already clamps to [0.0, 1.0] internally.
    let current_t = state.active.as_ref().map(|a| a.batch.progress());

    // Resolve the current rect for each target window.
    let from_rects: Vec<(WindowRef, Rect)> = pending
        .targets
        .iter()
        .map(|target| {
            let rect = if let (Some(t), Some(active)) = (current_t, &state.active) {
                // Prefer the interpolated position from the active batch when
                // this window is part of it (RetargetFromCurrent case).
                active
                    .batch
                    .tweens
                    .iter()
                    .find(|tw| tw.window_ref == target.window_ref)
                    // Use the *active* batch's frozen config so retarget sampling
                    // is consistent with what the frame-tick was rendering.
                    .map(|tw| AnimationBatch::interpolated_rect(tw, &active.config, t))
                    .unwrap_or_else(|| {
                        // If the window's rect can't be queried (e.g., it was destroyed or hidden),
                        // start from a zero rect. The animation still runs; the visual may be jarring.
                        backend
                            .get_window_rect(target.window_ref)
                            .unwrap_or_default()
                    })
            } else {
                // No active animation — query the backend directly.
                // If the window's rect can't be queried (e.g., it was destroyed or hidden),
                // start from a zero rect. The animation still runs; the visual may be jarring.
                backend
                    .get_window_rect(target.window_ref)
                    .unwrap_or_default()
            };
            (target.window_ref, rect)
        })
        .collect();

    let tweens = build_tweens(&pending.targets, &from_rects, &pending.config);

    log::debug!(
        "start_batch: {} targets, {} from_rects, {} tweens built, duration={:?}",
        pending.targets.len(),
        from_rects.len(),
        tweens.len(),
        pending.config.duration,
    );
    for (i, tw) in tweens.iter().enumerate() {
        log::trace!(
            "  tween[{}]: win={:?} from=({},{},{},{}) to=({},{},{},{})",
            i,
            tw.window_ref,
            tw.from_rect.x,
            tw.from_rect.y,
            tw.from_rect.w,
            tw.from_rect.h,
            tw.to_rect.x,
            tw.to_rect.y,
            tw.to_rect.w,
            tw.to_rect.h,
        );
    }

    // All tweens are no-ops — nothing to animate. This is normal during
    // continuous drag-preview (resubmitting an unchanged layout), so trace.
    if tweens.is_empty() {
        log::trace!("start_batch: all tweens are no-ops, skipping animation");
        return;
    }

    let batch = AnimationBatch::new(tweens, pending.config.duration);
    state.active = Some(ActiveBatch {
        batch,
        config: pending.config.clone(),
    });
    state.config = pending.config;
    animating.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::backend::mock::{MockBackend, MockCall, MockState};
    use crate::animation::config::AnimatorConfig;
    use crate::animation::types::{IVec2, WindowRef, WindowTarget};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Build an animator backed by a [`MockBackend`] with two pre-seeded windows.
    fn make_animator_with_mock() -> (WindowAnimator, Arc<Mutex<MockState>>) {
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let config = AnimatorConfig {
            duration: Duration::from_millis(50),
            ..AnimatorConfig::default()
        };
        let animator = WindowAnimator::new(backend, config);
        (animator, state)
    }

    /// Convenient one-window target pointing at a position different from the seeded rect.
    fn target_w1() -> Vec<WindowTarget> {
        vec![WindowTarget::new(
            WindowRef(1),
            IVec2::new(500, 500),
            IVec2::new(200, 200),
        )]
    }

    // -----------------------------------------------------------------------
    // Test 1: new animator starts idle
    // -----------------------------------------------------------------------

    #[test]
    fn animator_starts_idle() {
        let (animator, _state) = make_animator_with_mock();
        assert!(
            !animator.is_animating(),
            "new animator should not be animating"
        );
    }

    // -----------------------------------------------------------------------
    // Test 2: empty batch returns EmptyBatch error
    // -----------------------------------------------------------------------

    #[test]
    fn animate_empty_batch_returns_error() {
        let (mut animator, _state) = make_animator_with_mock();
        let result = animator.animate(vec![]);
        assert!(
            matches!(result, Err(AnimationError::EmptyBatch)),
            "expected EmptyBatch, got {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: valid batch returns an AnimationHandle
    // -----------------------------------------------------------------------

    #[test]
    fn animate_returns_handle() {
        let (mut animator, _state) = make_animator_with_mock();
        let result = animator.animate(target_w1());
        assert!(result.is_ok(), "expected Ok(handle), got {result:?}");
        let handle = result.unwrap();
        assert!(handle.0 >= 1, "handle ID should be >= 1");
    }

    // -----------------------------------------------------------------------
    // Test 4: cancel stops the animation
    // -----------------------------------------------------------------------

    /// Number of `ApplyBatch` calls recorded so far.
    fn applied_count(state: &Arc<Mutex<MockState>>) -> usize {
        state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| matches!(c, MockCall::ApplyBatch(_)))
            .count()
    }

    #[test]
    fn cancel_and_wait_blocks_until_no_more_apply_batch() {
        // Arrange: a long batch whose frames are paced by a slow mock
        // DwmFlush, so the worker is reliably mid-flight when we cancel.
        let state = Arc::new(Mutex::new(MockState {
            rects: [(WindowRef(1), Rect::new(0, 0, 100, 100))].into(),
            dwm_flush_delay_ms: 5,
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let config = AnimatorConfig {
            duration: Duration::from_millis(2_000),
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, config);
        animator.animate(target_w1()).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        assert!(animator.is_animating(), "batch should be in flight");
        assert!(
            applied_count(&state) > 0,
            "worker should have applied frames"
        );

        // Act.
        assert!(animator.cancel_and_wait(), "worker must acknowledge");
        let applied_at_return = applied_count(&state);

        // Assert: nothing else is applied after the call returned, even
        // after giving the worker ample time to (wrongly) tick again.
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            applied_at_return,
            applied_count(&state),
            "no apply_batch may follow cancel_and_wait"
        );
        assert!(!animator.is_animating());
        assert!(animator.is_alive());
    }

    #[test]
    fn cancel_and_wait_on_idle_worker_returns_true() {
        let (mut animator, _state) = make_animator_with_mock();
        assert!(animator.cancel_and_wait());
        // Repeatable.
        assert!(animator.cancel_and_wait());
    }

    /// A backend whose `apply_batch` panics, to exercise the worker's
    /// panic guard.
    struct PanickingBackend;

    impl crate::animation::backend::WindowBackend for PanickingBackend {
        fn get_window_rect(&self, _window: WindowRef) -> crate::animation::types::Result<Rect> {
            Ok(Rect::new(0, 0, 10, 10))
        }
        fn apply_batch(
            &self,
            _updates: &[(WindowRef, Rect)],
        ) -> crate::animation::types::Result<()> {
            panic!("injected backend panic");
        }
        fn dwm_flush(&self) -> crate::animation::types::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn worker_panic_marks_animator_dead() {
        // The default panic hook prints the injected panic to stderr; that
        // output is expected for this test.
        let mut animator = WindowAnimator::new(PanickingBackend, AnimatorConfig::default());
        assert!(animator.is_alive());
        animator.animate(target_w1()).unwrap();
        // The worker panics on the first frame; wait for it to die.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while animator.is_alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!animator.is_alive(), "worker should be marked dead");
        // Later submissions report the dead worker instead of hanging.
        assert!(matches!(
            animator.animate(target_w1()),
            Err(AnimationError::WorkerDead)
        ));
        // cancel_and_wait must not block on a dead worker.
        let t0 = std::time::Instant::now();
        assert!(!animator.cancel_and_wait());
        assert!(t0.elapsed() < CANCEL_ACK_TIMEOUT);
    }

    #[test]
    fn panic_payload_message_extracts_str_and_string() {
        let boxed: Box<dyn std::any::Any + Send> = Box::new("static text");
        assert_eq!(panic_payload_message(boxed.as_ref()), "static text");
        let boxed: Box<dyn std::any::Any + Send> = Box::new(String::from("owned text"));
        assert_eq!(panic_payload_message(boxed.as_ref()), "owned text");
        let boxed: Box<dyn std::any::Any + Send> = Box::new(42_u8);
        assert_eq!(
            panic_payload_message(boxed.as_ref()),
            "<non-string panic payload>"
        );
    }

    #[test]
    fn cancel_stops_animation() {
        let (mut animator, _state) = make_animator_with_mock();

        animator.animate(target_w1()).unwrap();
        // Give the worker a moment to pick up the command and set the flag.
        std::thread::sleep(Duration::from_millis(10));

        animator.cancel();
        // Give the worker time to process the Cancel command.
        std::thread::sleep(Duration::from_millis(20));

        assert!(
            !animator.is_animating(),
            "animator should not be animating after cancel"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: DropNew policy — second animate() is silently accepted at the
    // API level (returns Ok) but does not replace the first animation.
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Test 6 (W2 fix verification): zero-duration animation completes
    // instantly — the animator is not animating after a zero-duration
    // batch finishes. This validates that the daemon's initial snap
    // (constructed with Duration::ZERO) does not leave the animator
    // in an "animating" state.
    // -----------------------------------------------------------------------

    #[test]
    fn zero_duration_animation_completes_instantly() {
        // Arrange: animator with Duration::ZERO snap config.
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let snap_config = AnimatorConfig {
            duration: Duration::ZERO,
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, snap_config);

        // Act: submit an animation batch with the zero-duration config.
        let targets = vec![WindowTarget::new(
            WindowRef(1),
            IVec2::new(500, 500),
            IVec2::new(200, 200),
        )];
        animator.animate(targets).unwrap();

        // Allow the worker thread a small amount of time to process the
        // command. With zero duration, progress() returns 1.0 immediately
        // so the worker should finish in one tick.
        std::thread::sleep(Duration::from_millis(20));

        // Assert: animator should NOT be animating — zero duration means
        // the batch completed on the very first tick.
        assert!(
            !animator.is_animating(),
            "animator with zero-duration config should complete instantly \
             and not be animating after a short delay"
        );

        // Assert: the backend received an ApplyBatch call (windows were moved).
        let locked = state.lock().unwrap();
        let batch_calls: Vec<_> = locked
            .calls
            .iter()
            .filter(|c| matches!(c, MockCall::ApplyBatch(_)))
            .collect();
        assert!(
            !batch_calls.is_empty(),
            "zero-duration animation should still have applied at least one batch"
        );
    }

    // -----------------------------------------------------------------------
    // Test 7 (W3 fix verification): after update_config switches from
    // zero-duration to a non-zero duration, subsequent animations run at
    // the new (non-zero) duration and actually animate (is_animating true).
    // -----------------------------------------------------------------------

    #[test]
    fn update_config_switches_to_runtime_duration() {
        // Arrange: animator starts with zero-duration (initial snap config),
        // simulating the daemon's construction-time behaviour.
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let snap_config = AnimatorConfig {
            duration: Duration::ZERO,
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, snap_config);

        // Act: initial snap (instant, zero-duration) — same as daemon init step 8.
        let snap_targets = vec![WindowTarget::new(
            WindowRef(1),
            IVec2::new(500, 500),
            IVec2::new(200, 200),
        )];
        animator.animate(snap_targets).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            !animator.is_animating(),
            "after zero-duration snap, animator should be idle"
        );

        // Act (W3): switch to runtime config — 250ms animation enabled.
        // This mirrors daemon init step 9: derive_animator_config + update_config.
        let runtime_config = AnimatorConfig {
            duration: Duration::from_millis(250),
            ..AnimatorConfig::default()
        };
        animator.update_config(runtime_config);

        // Act: trigger a runtime layout change that should animate.
        let runtime_targets = vec![WindowTarget::new(
            WindowRef(1),
            IVec2::new(0, 0),
            IVec2::new(960, 1080),
        )];
        animator.animate(runtime_targets).unwrap();

        // Give the worker time to pick up the command.
        std::thread::sleep(Duration::from_millis(15));

        // Assert: animator IS animating — the runtime config has 250ms duration.
        assert!(
            animator.is_animating(),
            "after update_config to 250ms, subsequent animate should be in-progress \
             (not instant)"
        );

        // Cancel to clean up (avoid worker spinning for 250ms).
        animator.cancel();
    }

    // -----------------------------------------------------------------------
    // Test 8: update_config does not affect an animation already in flight.
    // The currently-active batch keeps its original config.
    // -----------------------------------------------------------------------

    #[test]
    fn update_config_does_not_affect_inflight_animation() {
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let config = AnimatorConfig {
            duration: Duration::from_millis(200),
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, config);

        // Start a long animation.
        animator.animate(target_w1()).unwrap();
        std::thread::sleep(Duration::from_millis(15));
        assert!(animator.is_animating());

        // Update config mid-flight — should NOT affect the running batch.
        animator.update_config(AnimatorConfig {
            duration: Duration::ZERO,
            ..AnimatorConfig::default()
        });

        // The animation should still be running (original 200ms config).
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            animator.is_animating(),
            "changing config mid-flight should not cancel the active animation"
        );

        animator.cancel();
    }

    // -----------------------------------------------------------------------
    // Test 5: DropNew policy — second animate() is silently accepted at the
    // API level (returns Ok) but does not replace the first animation.
    // -----------------------------------------------------------------------

    #[test]
    fn drop_new_policy_discards_second_animate() {
        let state = Arc::new(Mutex::new(MockState {
            rects: [
                (WindowRef(1), Rect::new(0, 0, 100, 100)),
                (WindowRef(2), Rect::new(100, 0, 100, 100)),
            ]
            .into(),
            ..MockState::default()
        }));
        let backend = MockBackend::with_state(Arc::clone(&state));
        let config = AnimatorConfig {
            duration: Duration::from_millis(200), // long enough to stay active
            interrupt_policy: InterruptPolicy::DropNew,
            ..AnimatorConfig::default()
        };
        let mut animator = WindowAnimator::new(backend, config);

        // First animate — should start.
        let h1 = animator
            .animate(target_w1())
            .expect("first animate should succeed");

        // Brief pause so the worker processes the first command and sets animating=true.
        std::thread::sleep(Duration::from_millis(15));
        assert!(
            animator.is_animating(),
            "should be animating after first call"
        );

        // Second animate — API returns Ok (the request is accepted into the channel),
        // but the worker will silently drop it.
        let second_targets = vec![WindowTarget::new(
            WindowRef(2),
            IVec2::new(0, 0),
            IVec2::new(50, 50),
        )];
        let h2 = animator
            .animate(second_targets)
            .expect("second animate should return Ok");

        // The handles must be distinct.
        assert_ne!(h1.0, h2.0, "handles should have distinct IDs");

        // Animation should still be running (not interrupted by the second request).
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            animator.is_animating(),
            "animation should still be running under DropNew policy"
        );
    }
}
