//! The foreground push runs on a thread of its own, bounded in time.
//!
//! [`win32::push_foreground_window`](super::win32::push_foreground_window)
//! hands the push to a `foreground-push` worker and waits at most
//! [`PUSH_BOUND`]. A push that does not come back in time (a thread that
//! stopped answering in the moment between the responsiveness checks and
//! the push) is logged with the window that held the foreground,
//! abandoned, and reported to the IPC request as an error
//! ([`take_abandoned_push`]); the worker is left to finish on its own and
//! a fresh one takes the next push. This is the backstop behind the
//! responsiveness gate, not the fix (ADR-0035).

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::win32::{self, ForegroundPush};

/// How long the main thread waits for a foreground push. A healthy push
/// (two 50 ms responsiveness probes, the calls, the 50 ms settle) takes
/// well under this.
pub const PUSH_BOUND: Duration = Duration::from_millis(1000);

/// Abandoned pushes still running before new pushes are refused outright
/// (each holds a thread that is waiting on another process).
const MAX_STUCK: usize = 4;

/// [`Job::state`]: the push is running.
const RUNNING: u8 = 0;
/// [`Job::state`]: the push returned in time; its outcome is in the reply.
const FINISHED: u8 = 1;
/// [`Job::state`]: the main thread stopped waiting.
pub const ABANDONED: u8 = 2;

/// The push the worker runs ([`win32::push_now`]; a stand-in in tests).
type PushFn = fn(isize, bool, &AtomicU8) -> ForegroundPush;

/// One push for the worker. `state` is decided once, by whichever side
/// moves it off [`RUNNING`] first.
struct Job {
    hwnd: isize,
    quiet: bool,
    push: PushFn,
    state: Arc<AtomicU8>,
    reply: SyncSender<ForegroundPush>,
}

/// The current worker's queue (`None` until the first push, and after a
/// push was abandoned).
static WORKER: Mutex<Option<Sender<Job>>> = Mutex::new(None);
/// Abandoned pushes whose worker has not returned yet.
static STUCK: AtomicUsize = AtomicUsize::new(0);
/// The note of the last abandoned push, for the IPC request it happened in.
static ABANDONED_NOTE: Mutex<Option<String>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The note of a push abandoned since the last call, clearing it. The
/// dispatcher turns it into the request's error answer.
#[must_use]
pub fn take_abandoned_push() -> Option<String> {
    lock(&ABANDONED_NOTE).take()
}

/// Abandoned pushes whose thread is still waiting.
#[must_use]
pub fn stuck_pushes() -> usize {
    STUCK.load(Ordering::Acquire)
}

fn spawn_worker() -> Option<Sender<Job>> {
    let (tx, rx) = channel::<Job>();
    std::thread::Builder::new()
        .name("foreground-push".to_owned())
        .spawn(move || run_worker(&rx))
        .map_err(|e| log::warn!("foreground push: cannot start the push thread: {e}"))
        .ok()?;
    Some(tx)
}

fn run_worker(rx: &Receiver<Job>) {
    win32::ensure_message_queue();
    while let Ok(job) = rx.recv() {
        let outcome = (job.push)(job.hwnd, job.quiet, &job.state);
        if job
            .state
            .compare_exchange(RUNNING, FINISHED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            STUCK.fetch_sub(1, Ordering::AcqRel);
            log::info!(
                "foreground push: the abandoned push to hwnd {:#x} returned ({outcome:?})",
                job.hwnd
            );
            // The main thread has a fresh worker; this one retires.
            return;
        }
        let _ = job.reply.send(outcome);
    }
}

/// Run the push for `hwnd` on the worker and wait at most [`PUSH_BOUND`].
pub(super) fn bounded(hwnd: isize, quiet: bool) -> ForegroundPush {
    bounded_with(hwnd, quiet, win32::push_now)
}

fn bounded_with(hwnd: isize, quiet: bool, push: PushFn) -> ForegroundPush {
    let stuck = STUCK.load(Ordering::Acquire);
    if stuck >= MAX_STUCK {
        log::warn!(
            "foreground push: hwnd {hwnd:#x} not pushed, {stuck} earlier pushes are still waiting on a window that does not answer"
        );
        return ForegroundPush::Abandoned;
    }
    let state = Arc::new(AtomicU8::new(RUNNING));
    let (reply, answer) = sync_channel(1);
    let mut job = Job {
        hwnd,
        quiet,
        push,
        state: Arc::clone(&state),
        reply,
    };
    {
        let mut worker = lock(&WORKER);
        for _ in 0..2 {
            if worker.is_none() {
                *worker = spawn_worker();
            }
            let Some(tx) = worker.as_ref() else {
                // No thread: push on this one, unbounded, as before.
                drop(worker);
                return push(hwnd, quiet, &state);
            };
            match tx.send(job) {
                Ok(()) => break,
                Err(std::sync::mpsc::SendError(back)) => {
                    // The worker is gone (it panicked): a fresh one.
                    job = back;
                    *worker = None;
                }
            }
        }
    }
    match answer.recv_timeout(PUSH_BOUND) {
        Ok(outcome) => outcome,
        Err(_) => {
            // Counted before the state flips, so the worker's decrement
            // can never come first.
            STUCK.fetch_add(1, Ordering::AcqRel);
            if state
                .compare_exchange(RUNNING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // It finished in the same instant: the reply is there.
                STUCK.fetch_sub(1, Ordering::AcqRel);
                return answer.recv().unwrap_or(ForegroundPush::Abandoned);
            }
            *lock(&WORKER) = None;
            let fg = win32::get_foreground_window().unwrap_or(0);
            let note = format!(
                "foreground push to hwnd {hwnd:#x} ({}) abandoned: no answer within {} ms; the foreground was hwnd {fg:#x} ({})",
                win32::window_owner_description(hwnd),
                PUSH_BOUND.as_millis(),
                win32::window_owner_description(fg),
            );
            log::warn!("{note}");
            *lock(&ABANDONED_NOTE) = Some(note);
            ForegroundPush::Abandoned
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick(_: isize, _: bool, _: &AtomicU8) -> ForegroundPush {
        ForegroundPush::Focused
    }

    /// Holds the push longer than the bound, then returns.
    fn wedged(_: isize, _: bool, state: &AtomicU8) -> ForegroundPush {
        std::thread::sleep(PUSH_BOUND + Duration::from_millis(300));
        assert_eq!(state.load(Ordering::Acquire), ABANDONED);
        ForegroundPush::Focused
    }

    /// One test drives the shared worker through a normal push, an
    /// abandoned one (error note, counted while it waits, uncounted when
    /// it returns) and a normal push on the fresh worker after it.
    #[test]
    fn a_push_past_the_bound_is_abandoned_and_the_next_one_gets_a_fresh_thread() {
        let _ = take_abandoned_push();
        assert_eq!(bounded_with(0x10, true, quick), ForegroundPush::Focused);
        assert!(take_abandoned_push().is_none());

        let started = std::time::Instant::now();
        assert_eq!(bounded_with(0x20, true, wedged), ForegroundPush::Abandoned);
        assert!(started.elapsed() < PUSH_BOUND + Duration::from_millis(250));
        let note = take_abandoned_push().expect("an abandoned push leaves a note");
        assert!(note.starts_with("foreground push to hwnd 0x20 ("), "{note}");
        assert!(
            note.contains("abandoned: no answer within 1000 ms"),
            "{note}"
        );
        // Other tests push through the same worker, so the counter is
        // compared, not matched.
        let stuck = stuck_pushes();
        assert!(stuck >= 1);

        // The next push runs at once on a new thread.
        assert_eq!(bounded_with(0x30, true, quick), ForegroundPush::Focused);
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            stuck_pushes() < stuck,
            "the wedged push returned and retired"
        );
    }
}
