//! Live reload: the daemon notices an edited `flow.toml` / `flow-rules.toml`
//! and runs its own reload path, so a saved file takes effect without
//! `flow config reload`.
//!
//! The watch is a once-a-second look at the two files' modification times
//! from the event loop (which wakes at least that often for its liveness
//! check): no watcher thread, no new dependency. A change counts only once
//! it has been seen unchanged on a second look, which rides out editors
//! that truncate-then-write or write-then-rename. Why polling and not
//! `ReadDirectoryChangesW` is in the dev guide.
//! (`docs/src/dev-guide/config-and-persistence.md`, *Live reload*)

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use super::types::FlowWM;

/// How often the files are looked at.
pub(super) const CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The modification times of the watched files, in [`ConfigWatch::paths`] order.
type Stamps = Vec<Option<SystemTime>>;

/// Decides when an on-disk change becomes a reload. Pure: time and the file
/// system are passed in.
#[derive(Debug)]
pub(super) struct ConfigWatch {
    /// `flow.toml`, `flow-rules.toml`.
    paths: Vec<PathBuf>,
    /// The stamps the running config was loaded from.
    applied: Stamps,
    /// A change seen once, waiting to be seen again unchanged.
    pending: Option<Stamps>,
    /// When the files were last looked at.
    last_check: Option<Instant>,
    /// A change arrived while tiling was off (reload is refused then); it is
    /// applied when tiling resumes.
    deferred: bool,
}

impl ConfigWatch {
    /// Watch `flow.toml` and `flow-rules.toml` under `config_dir`, taking
    /// their current stamps as the applied ones.
    pub(super) fn new(config_dir: &Path) -> Self {
        let paths = vec![
            config_dir.join("flow.toml"),
            config_dir.join("flow-rules.toml"),
        ];
        let applied = paths.iter().map(|p| mtime(p)).collect();
        Self {
            paths,
            applied,
            pending: None,
            last_check: None,
            deferred: false,
        }
    }

    /// One look, if [`CHECK_INTERVAL`] has passed since the last: `true`
    /// when a change has been seen twice unchanged and should be applied
    /// now. `read` answers a path's modification time.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        read: &dyn Fn(&Path) -> Option<SystemTime>,
    ) -> bool {
        if self
            .last_check
            .is_some_and(|t| now.duration_since(t) < CHECK_INTERVAL)
        {
            return false;
        }
        self.last_check = Some(now);
        let current: Stamps = self.paths.iter().map(|p| read(p)).collect();
        if current == self.applied {
            self.pending = None;
            return false;
        }
        match &self.pending {
            Some(seen) if *seen == current => true,
            _ => {
                self.pending = Some(current);
                false
            }
        }
    }

    /// The pending change is the running config now (reload ran, refused or
    /// not: a refused file is retried only when it changes again).
    pub(super) fn mark_applied(&mut self, read: &dyn Fn(&Path) -> Option<SystemTime>) {
        self.applied = self.paths.iter().map(|p| read(p)).collect();
        self.pending = None;
        self.deferred = false;
    }

    /// Remember a change for when tiling resumes.
    pub(super) fn defer(&mut self, read: &dyn Fn(&Path) -> Option<SystemTime>) {
        self.mark_applied(read);
        self.deferred = true;
    }

    /// Whether a change waits for tiling to resume.
    pub(super) fn is_deferred(&self) -> bool {
        self.deferred
    }

    /// The watched `flow.toml`.
    pub(super) fn config_path(&self) -> &Path {
        &self.paths[0]
    }
}

/// A file's modification time, `None` when it is absent or unreadable.
pub(super) fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl FlowWM {
    /// The event loop's look at the config files: apply a settled on-disk
    /// change through the ordinary reload path, or hold it while tiling is
    /// off and apply it when tiling is back on.
    pub(super) fn poll_config_files(&mut self) {
        let tiling_on = self.tiling_enabled();
        if self.config_watch.is_deferred() && tiling_on {
            log::info!("config: applying the change held while tiling was off");
            self.reload_from_disk();
            return;
        }
        if !self.config_watch.observe(Instant::now(), &mtime) {
            return;
        }
        if !tiling_on {
            log::info!(
                "config: {} changed while tiling is off; it is reloaded when tiling resumes",
                self.config_watch.config_path().display()
            );
            self.config_watch.defer(&mtime);
            return;
        }
        log::info!(
            "config: {} changed on disk; reloading",
            self.config_watch.config_path().display()
        );
        self.reload_from_disk();
    }

    /// Run the reload path and record the files as applied either way.
    fn reload_from_disk(&mut self) {
        match self.dispatch_reload_config() {
            crate::ipc::message::SocketResponse::Ok => log::info!("config: reloaded"),
            crate::ipc::message::SocketResponse::Error { message } => {
                log::warn!("config: reload refused: {message}; keeping the running config")
            }
            other => log::warn!("config: unexpected reload reply {other:?}"),
        }
        self.config_watch.mark_applied(&mtime);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A fake file system: path → stamp, with a counter for distinct times.
    struct Disk(RefCell<HashMap<PathBuf, Option<SystemTime>>>);

    impl Disk {
        fn new(dir: &Path, flow: u64, rules: Option<u64>) -> Self {
            let mut m = HashMap::new();
            m.insert(dir.join("flow.toml"), Some(stamp(flow)));
            m.insert(dir.join("flow-rules.toml"), rules.map(stamp));
            Self(RefCell::new(m))
        }
        fn set(&self, dir: &Path, file: &str, t: u64) {
            self.0.borrow_mut().insert(dir.join(file), Some(stamp(t)));
        }
        fn read(&self) -> impl Fn(&Path) -> Option<SystemTime> + '_ {
            move |p: &Path| self.0.borrow().get(p).copied().flatten()
        }
    }

    fn stamp(n: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + n)
    }

    fn watch(dir: &Path, disk: &Disk) -> ConfigWatch {
        let mut w = ConfigWatch::new(dir);
        w.mark_applied(&disk.read());
        w
    }

    #[test]
    fn a_change_is_applied_once_it_has_been_seen_twice() {
        let dir = PathBuf::from(r"C:\cfg");
        let disk = Disk::new(&dir, 1, None);
        let mut w = watch(&dir, &disk);
        let t0 = Instant::now();
        // Unchanged: nothing.
        assert!(!w.observe(t0, &disk.read()));
        // The file changes: first look records it, second look applies.
        disk.set(&dir, "flow.toml", 2);
        assert!(!w.observe(t0 + Duration::from_secs(1), &disk.read()));
        assert!(w.observe(t0 + Duration::from_secs(2), &disk.read()));
        w.mark_applied(&disk.read());
        // Applied: quiet again.
        assert!(!w.observe(t0 + Duration::from_secs(3), &disk.read()));
        assert!(!w.observe(t0 + Duration::from_secs(4), &disk.read()));
    }

    #[test]
    fn a_file_still_being_written_waits_for_a_stable_read() {
        let dir = PathBuf::from(r"C:\cfg");
        let disk = Disk::new(&dir, 1, None);
        let mut w = watch(&dir, &disk);
        let t0 = Instant::now();
        disk.set(&dir, "flow.toml", 2);
        assert!(!w.observe(t0 + Duration::from_secs(1), &disk.read()));
        // Another write before the second look: start over.
        disk.set(&dir, "flow.toml", 3);
        assert!(!w.observe(t0 + Duration::from_secs(2), &disk.read()));
        assert!(w.observe(t0 + Duration::from_secs(3), &disk.read()));
    }

    #[test]
    fn looks_are_rate_limited_to_the_interval() {
        let dir = PathBuf::from(r"C:\cfg");
        let disk = Disk::new(&dir, 1, None);
        let mut w = watch(&dir, &disk);
        let t0 = Instant::now();
        disk.set(&dir, "flow.toml", 2);
        assert!(!w.observe(t0, &disk.read()));
        // 300 ms later is too soon to count as the second look.
        assert!(!w.observe(t0 + Duration::from_millis(300), &disk.read()));
        assert!(!w.observe(t0 + Duration::from_millis(900), &disk.read()));
        assert!(w.observe(t0 + Duration::from_millis(1000), &disk.read()));
    }

    #[test]
    fn the_rules_file_is_watched_too_and_a_deleted_file_counts() {
        let dir = PathBuf::from(r"C:\cfg");
        let disk = Disk::new(&dir, 1, Some(5));
        let mut w = watch(&dir, &disk);
        let t0 = Instant::now();
        disk.set(&dir, "flow-rules.toml", 6);
        assert!(!w.observe(t0 + Duration::from_secs(1), &disk.read()));
        assert!(w.observe(t0 + Duration::from_secs(2), &disk.read()));
        w.mark_applied(&disk.read());
        // Deleting flow.toml is a change (defaults apply).
        disk.0.borrow_mut().insert(dir.join("flow.toml"), None);
        assert!(!w.observe(t0 + Duration::from_secs(3), &disk.read()));
        assert!(w.observe(t0 + Duration::from_secs(4), &disk.read()));
    }

    #[test]
    fn a_change_can_be_deferred_and_is_remembered() {
        let dir = PathBuf::from(r"C:\cfg");
        let disk = Disk::new(&dir, 1, None);
        let mut w = watch(&dir, &disk);
        let t0 = Instant::now();
        disk.set(&dir, "flow.toml", 2);
        w.observe(t0, &disk.read());
        assert!(w.observe(t0 + Duration::from_secs(1), &disk.read()));
        w.defer(&disk.read());
        assert!(w.is_deferred());
        // Deferred: the same change is not reported again.
        assert!(!w.observe(t0 + Duration::from_secs(2), &disk.read()));
        w.mark_applied(&disk.read());
        assert!(!w.is_deferred());
    }
}
