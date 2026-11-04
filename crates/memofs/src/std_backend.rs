use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use notify::{watcher, DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{DirEntry, Metadata, ReadDir, VfsBackend, VfsEvent};

const DEBOUNCE_DELAY: Duration = Duration::from_millis(50);

/// How long a removal notice waits for the debounced event of its path.
///
/// notify restarts a path's debounce timer on every event for it, so this is
/// generous. A rename can deliver its debounced event under a different path,
/// which would otherwise leave the notice behind forever.
const REMOVAL_NOTICE_LIFETIME: Duration = Duration::from_secs(2);

/// FSEvents watches paths, not inodes, so a macOS watch keeps reporting a path
/// after it is deleted and recreated. Parent watches and dropping the watches
/// of removed paths are unnecessary there, and costly: FSEvents is recursive
/// at the OS level, so a parent watch on the project folder wakes the stream
/// for every `.git` or `node_modules` change, and every watch change restarts
/// the stream, losing the events that arrive during the restart.
const PATH_BASED_WATCHES: bool = cfg!(target_os = "macos");

/// notify's inotify backend removes every watch below a path when that path
/// is unwatched, including the ones a recursive watch on an ancestor added.
/// Watching a path again adds to its existing watch instead of replacing it.
const INOTIFY_WATCHES: bool = cfg!(target_os = "linux");

/// `VfsBackend` that uses `std::fs` and the `notify` crate.
pub struct StdBackend {
    watcher: Option<RecommendedWatcher>,
    watcher_receiver: Receiver<VfsEvent>,
    watcher_sender: Sender<VfsEvent>,
    watches: HashSet<PathBuf>,
    parent_watches: HashSet<PathBuf>,
    /// Watch roots whose registration was dropped because they or one of
    /// their ancestors were removed. They are registered again once they
    /// reappear, since nothing else would read them again.
    lost_watches: HashSet<PathBuf>,
    /// Every path that has held a watch. Parent watches stay at or below
    /// these, so a missing path cannot put a watch on the home directory or
    /// the filesystem root.
    anchors: HashSet<PathBuf>,
    /// Paths that could not be watched for a reason other than not existing,
    /// such as running out of inotify watches or lacking permission. They are
    /// read without a watch instead of failing every snapshot that reads
    /// them, and are retried when the watcher asks for a rescan.
    degraded: HashSet<PathBuf>,
    /// Degraded paths that were already logged, so retries do not spam.
    reported_degraded: HashSet<PathBuf>,
}

impl StdBackend {
    pub fn new() -> io::Result<StdBackend> {
        let mut backend = Self::new_without_watch();
        backend.initialize_watcher()?;
        Ok(backend)
    }

    /// Creates a backend without allocating an OS watcher until the first watch.
    pub fn new_without_watch() -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            watcher: None,
            watcher_receiver: rx,
            watcher_sender: tx,
            watches: HashSet::new(),
            parent_watches: HashSet::new(),
            lost_watches: HashSet::new(),
            anchors: HashSet::new(),
            degraded: HashSet::new(),
            reported_degraded: HashSet::new(),
        }
    }

    fn initialize_watcher(&mut self) -> io::Result<()> {
        if self.watcher.is_some() {
            return Ok(());
        }
        let (notify_tx, notify_rx) = mpsc::channel();
        let watcher = watcher(notify_tx, DEBOUNCE_DELAY).map_err(notify_io_error)?;
        let tx = self.watcher_sender.clone();

        thread::spawn(move || {
            let mut removal_notices = HashMap::new();
            for event in notify_rx {
                for event in translate_event(event, &mut removal_notices, Instant::now()) {
                    tx.send(event)?;
                }
            }

            Result::<(), crossbeam_channel::SendError<VfsEvent>>::Ok(())
        });

        self.watcher = Some(watcher);
        Ok(())
    }

    fn is_covered(&self, path: &Path) -> bool {
        path.ancestors()
            .any(|ancestor| self.watches.contains(ancestor))
    }

    fn is_anchored(&self, path: &Path) -> bool {
        path.ancestors()
            .any(|ancestor| self.anchors.contains(ancestor))
    }

    /// Makes sure that the creation of `path` is reported by watching its
    /// nearest existing ancestor.
    ///
    /// A watch on a directory itself dies when that directory is deleted, so
    /// a surviving ancestor must report its recreation for mapped roots to
    /// recover after a package reinstall or branch switch. Failing to register
    /// that watch only costs this recovery, so it is logged instead of failing
    /// the read that asked for the watch.
    fn ensure_creation_watch(&mut self, path: &Path) {
        if PATH_BASED_WATCHES {
            return;
        }
        let mut previous: Option<PathBuf> = None;
        loop {
            let Some(ancestor) = nearest_existing_ancestor(path) else {
                return;
            };
            // An intermediate directory can appear between finding the nearest
            // ancestor and watching it, without the new watch reporting it.
            // Retry until the nearest ancestor is stable.
            if previous.as_deref() == Some(ancestor) {
                return;
            }
            if self.parent_watches.contains(ancestor) || self.is_covered(ancestor) {
                return;
            }
            // Only the parent of a path, or a directory inside something that
            // was watched before, may be watched for its creation. Otherwise a
            // missing absolute or `..` path could climb to the home directory.
            if path.parent() != Some(ancestor) && !self.is_anchored(ancestor) {
                log::debug!(
                    "Not watching {} for the creation of {}, since nothing in it was watched before",
                    ancestor.display(),
                    path.display()
                );
                return;
            }
            let Some(watcher) = self.watcher.as_mut() else {
                return;
            };
            if let Err(err) = watcher.watch(ancestor, RecursiveMode::NonRecursive) {
                log::warn!(
                    "Could not watch {} for changes to {}; it will not be resynced if it is deleted and recreated: {}",
                    ancestor.display(),
                    path.display(),
                    err
                );
                return;
            }
            self.parent_watches.insert(ancestor.to_owned());
            self.anchors.insert(ancestor.to_owned());
            previous = Some(ancestor.to_owned());
        }
    }

    /// Registers lost watch roots again once they exist.
    ///
    /// `trigger` is the path whose event led here. Roots other than it are
    /// reported as created, because the snapshot caused by that event does not
    /// necessarily reach them.
    fn recover_lost_watches(&mut self, roots: Vec<PathBuf>, trigger: Option<&Path>) {
        for root in roots {
            self.ensure_creation_watch(&root);
            if fs_err::metadata(&root).is_err() {
                continue;
            }
            match self.watch(&root) {
                Ok(()) => {
                    if trigger != Some(root.as_path()) {
                        let _ = self.watcher_sender.send(VfsEvent::Create(root));
                    }
                }
                Err(err) => {
                    log::warn!("Could not watch {} again: {}", root.display(), err);
                }
            }
        }
    }

    /// Remembers that `path` could not be watched, so reads of it still
    /// succeed and later events do not retry the registration each time.
    fn degrade(&mut self, path: &Path, err: &io::Error) {
        self.degraded.insert(path.to_owned());
        if !self.reported_degraded.insert(path.to_owned()) {
            log::debug!("Could not watch {} again: {}", path.display(), err);
            return;
        }
        // inotify reports ENOSPC when the per-user watch limit is reached.
        let hint = if INOTIFY_WATCHES && err.raw_os_error() == Some(28) {
            " Raising fs.inotify.max_user_watches may help."
        } else {
            ""
        };
        log::warn!(
            "Could not watch {}, so changes to it may not sync: {}.{}",
            path.display(),
            err,
            hint
        );
    }

    /// Registers an existing watch again after events may have been dropped.
    ///
    /// inotify watches inodes, so a root replaced while events were dropped
    /// keeps a watch on the deleted directory, and directories created in that
    /// time have no watch at all. notify adds to an existing inotify watch and
    /// walks the tree again, so watching the path again fixes both.
    fn rewatch(&mut self, path: &Path, mode: RecursiveMode) {
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        if !INOTIFY_WATCHES {
            // Other backends would keep the old registration running as well.
            let _ = watcher.unwatch(path);
        }
        if let Err(err) = watcher.watch(path, mode) {
            log::debug!("Could not watch {} again: {}", path.display(), err);
        }
    }
}

/// Converts a debounced notify event into the events the Vfs understands.
///
/// `removal_notices` holds paths whose removal notify announced before its
/// debounced event, and when. notify merges a removal followed by a recreation
/// of the same path into one write, which would otherwise hide that every
/// watch registered on the old directory is now dead.
fn translate_event(
    event: DebouncedEvent,
    removal_notices: &mut HashMap<PathBuf, Instant>,
    now: Instant,
) -> Vec<VfsEvent> {
    removal_notices
        .retain(|_, noticed| now.saturating_duration_since(*noticed) < REMOVAL_NOTICE_LIFETIME);
    match event {
        DebouncedEvent::NoticeRemove(path) => {
            removal_notices.insert(path, now);
            Vec::new()
        }
        DebouncedEvent::Create(path) => {
            removal_notices.remove(&path);
            vec![VfsEvent::Create(path)]
        }
        DebouncedEvent::Write(path) => {
            if removal_notices.remove(&path).is_some() {
                vec![VfsEvent::Remove(path.clone()), VfsEvent::Create(path)]
            } else {
                vec![VfsEvent::Write(path)]
            }
        }
        DebouncedEvent::Remove(path) => {
            removal_notices.remove(&path);
            vec![VfsEvent::Remove(path)]
        }
        DebouncedEvent::Rename(from, to) => {
            removal_notices.remove(&from);
            let mut events = Vec::with_capacity(3);
            // Something was renamed over a path that was removed first.
            if removal_notices.remove(&to).is_some() {
                events.push(VfsEvent::Remove(to.clone()));
            }
            events.push(VfsEvent::Remove(from));
            events.push(VfsEvent::Create(to));
            events
        }
        DebouncedEvent::Rescan => vec![VfsEvent::Rescan],
        DebouncedEvent::Error(err, path) => match path {
            // The state of that path is unknown, so treat it as changed.
            Some(path) => {
                log::warn!("Filesystem watcher error at {}: {}", path.display(), err);
                vec![VfsEvent::Write(path)]
            }
            None => {
                log::warn!("Filesystem watcher error: {}", err);
                Vec::new()
            }
        },
        DebouncedEvent::NoticeWrite(_) | DebouncedEvent::Chmod(_) => Vec::new(),
    }
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors()
        .skip(1)
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .find(|ancestor| ancestor.is_dir())
}

fn is_missing(path: &Path) -> bool {
    matches!(fs_err::symlink_metadata(path), Err(err) if err.kind() == io::ErrorKind::NotFound)
}

impl VfsBackend for StdBackend {
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>> {
        fs_err::read(path)
    }

    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        fs_err::write(path, data)
    }

    fn exists(&mut self, path: &Path) -> io::Result<bool> {
        std::fs::exists(path)
    }

    fn read_dir(&mut self, path: &Path) -> io::Result<ReadDir> {
        let entries: Result<Vec<_>, _> = fs_err::read_dir(path)?.collect();
        let mut entries = entries?;

        entries.sort_by_cached_key(|entry| entry.file_name());

        let inner = entries
            .into_iter()
            .map(|entry| Ok(DirEntry { path: entry.path() }));

        Ok(ReadDir {
            inner: Box::new(inner),
        })
    }

    fn create_dir(&mut self, path: &Path) -> io::Result<()> {
        fs_err::create_dir(path)
    }

    fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        fs_err::create_dir_all(path)
    }

    fn remove_file(&mut self, path: &Path) -> io::Result<()> {
        fs_err::remove_file(path)
    }

    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()> {
        fs_err::remove_dir_all(path)
    }

    fn metadata(&mut self, path: &Path) -> io::Result<Metadata> {
        let inner = fs_err::metadata(path)?;

        Ok(Metadata {
            is_file: inner.is_file(),
        })
    }

    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        dunce::canonicalize(path)
    }

    fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent> {
        self.watcher_receiver.clone()
    }

    /// Watches `path` recursively.
    ///
    /// Only a missing path fails, so callers can still recognize `NotFound`.
    /// Other failures are logged once and the path is read without a watch.
    fn watch(&mut self, path: &Path) -> io::Result<()> {
        if self.is_covered(path) {
            // The covering watch reports these roots coming back.
            self.lost_watches.retain(|lost| !lost.starts_with(path));
            return Ok(());
        }
        if self.degraded.contains(path) {
            return Ok(());
        }
        if let Err(err) = self.initialize_watcher() {
            self.degrade(path, &err);
            return Ok(());
        }
        self.ensure_creation_watch(path);
        let watcher = self.watcher.as_mut().unwrap();
        if self.parent_watches.remove(path) {
            // The recursive watch below replaces it, so a failure is harmless.
            let _ = watcher.unwatch(path);
        }
        if let Err(err) = watcher.watch(path, RecursiveMode::Recursive) {
            let err = notify_io_error(err);
            // Failed registrations of missing paths stay retryable after a
            // transient deletion.
            if is_missing(path) {
                return Err(io::Error::new(io::ErrorKind::NotFound, err));
            }
            self.degrade(path, &err);
            return Ok(());
        }
        self.watches.insert(path.to_path_buf());
        self.anchors.insert(path.to_path_buf());
        // Lost roots below this path are covered by its recursive watch now.
        self.lost_watches.retain(|lost| !lost.starts_with(path));
        // So are parent watches below it. inotify replaced them while walking
        // the tree, but other backends keep each registration separately.
        let nested: Vec<_> = self
            .parent_watches
            .iter()
            .filter(|parent| parent.starts_with(path))
            .cloned()
            .collect();
        for parent in nested {
            self.parent_watches.remove(&parent);
            if !INOTIFY_WATCHES {
                if let Some(watcher) = &mut self.watcher {
                    let _ = watcher.unwatch(&parent);
                }
            }
        }
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) -> io::Result<()> {
        if PATH_BASED_WATCHES {
            // The registration keeps reporting the path once it is recreated,
            // and changing it would restart the event stream.
            return Ok(());
        }
        // A root registered above `path` keeps reporting it, and its watches
        // below `path` belong to that root as well.
        let covered_by_ancestor = path
            .ancestors()
            .skip(1)
            .any(|ancestor| self.watches.contains(ancestor));
        let removed_watches: Vec<_> = self
            .watches
            .iter()
            .chain(self.parent_watches.iter())
            .filter(|watched| watched.starts_with(path))
            .cloned()
            .collect();
        // Whatever is created here next deserves a fresh attempt.
        self.degraded.retain(|degraded| !degraded.starts_with(path));
        let mut result = Ok(());
        for watched in removed_watches {
            if self.watches.remove(&watched) && !covered_by_ancestor {
                self.lost_watches.insert(watched.clone());
            }
            self.parent_watches.remove(&watched);
            if INOTIFY_WATCHES && covered_by_ancestor {
                // notify would remove the ancestor's watches on whatever is at
                // this path now, such as a directory recreated in its place.
                continue;
            }
            if let Some(watcher) = &mut self.watcher {
                if let Err(err) = watcher.unwatch(&watched) {
                    if !is_removed_watch_error(&watched, &err) {
                        result = Err(notify_io_error(err));
                    }
                }
            }
        }
        // Dropping the parent watches above may have removed the only watch
        // that would report the lost roots coming back.
        let lost: Vec<_> = self
            .lost_watches
            .iter()
            .filter(|lost| lost.starts_with(path))
            .cloned()
            .collect();
        for root in lost {
            self.ensure_creation_watch(&root);
        }
        // Unwatching an unregistered child can restart an entire notify
        // event stream on macOS and lose unrelated events during the restart.
        result
    }

    fn recover_watches(&mut self, path: &Path) -> io::Result<()> {
        let roots: Vec<_> = self
            .lost_watches
            .iter()
            .filter(|lost| lost.starts_with(path))
            .cloned()
            .collect();
        self.recover_lost_watches(roots, Some(path));
        Ok(())
    }

    fn revalidate_watches(&mut self) -> io::Result<()> {
        if PATH_BASED_WATCHES {
            return Ok(());
        }
        // Whatever kept these from being watched may have been resolved.
        self.degraded.clear();
        // Removals reported while events were dropped never reached
        // `unwatch`, so find the roots that disappeared.
        let missing: Vec<_> = self
            .watches
            .iter()
            .chain(self.parent_watches.iter())
            .filter(|watched| fs_err::symlink_metadata(watched).is_err())
            .cloned()
            .collect();
        for path in missing {
            let _ = self.unwatch(&path);
        }
        // Roots that still exist may have been replaced, or gained
        // directories, while events were dropped.
        let roots: Vec<_> = self.watches.iter().cloned().collect();
        for root in roots {
            self.rewatch(&root, RecursiveMode::Recursive);
        }
        let parents: Vec<_> = self.parent_watches.iter().cloned().collect();
        for parent in parents {
            self.rewatch(&parent, RecursiveMode::NonRecursive);
        }
        let roots = self.lost_watches.iter().cloned().collect();
        self.recover_lost_watches(roots, None);
        Ok(())
    }
}

fn is_removed_watch_error(path: &Path, err: &notify::Error) -> bool {
    // Linux removes inotify watches automatically when the watched inode is
    // deleted. Removing that already-gone watch returns EINVAL.
    let already_removed = matches!(err, notify::Error::WatchNotFound)
        || matches!(err, notify::Error::Io(err) if err.kind() == io::ErrorKind::InvalidInput);
    already_removed && is_missing(path)
}

fn notify_io_error(err: notify::Error) -> io::Error {
    match err {
        notify::Error::Io(err) => err,
        notify::Error::PathNotFound => io::Error::new(io::ErrorKind::NotFound, err),
        err => io::Error::other(err),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Returns a temporary directory and its canonical path. Watchers report
    /// canonical paths, such as `/private/var` instead of macOS's `/var`.
    fn tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dunce::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    #[test]
    fn watcher_is_allocated_only_when_requested() {
        let mut backend = StdBackend::new_without_watch();
        let (_dir, dir) = tempdir();
        backend.read_dir(&dir).unwrap();
        assert!(backend.watcher.is_none());
        backend.watch(&dir).unwrap();
        assert!(backend.watcher.is_some());
        assert!(backend.watches.contains(&dir));
    }

    #[test]
    fn failed_watch_registration_can_be_retried() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let missing = dir.join("missing");
        let err = backend.watch(&missing).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!backend.watches.contains(&missing));
        assert!(!backend.degraded.contains(&missing));
        fs_err::create_dir(&missing).unwrap();
        backend.watch(&missing).unwrap();
        assert!(backend.watches.contains(&missing));
    }

    #[test]
    fn unwatching_unknown_children_keeps_the_registered_root() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        backend.watch(&dir).unwrap();
        backend.unwatch(&dir.join("unregistered")).unwrap();
        assert!(backend.watches.contains(&dir));
    }

    #[test]
    fn removing_a_directory_forgets_watches_on_its_descendants() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let child = dir.join("child");
        fs_err::create_dir(&child).unwrap();
        backend.watch(&child).unwrap();
        backend.unwatch(&dir).unwrap();
        // FSEvents keeps reporting removed paths, so macOS keeps the watch.
        assert_eq!(backend.watches.is_empty(), !PATH_BASED_WATCHES);
        backend.watch(&child).unwrap();
        assert!(backend.watches.contains(&child));
    }

    /// Applies an event the way `Vfs::commit_event` does.
    fn commit(backend: &mut StdBackend, event: &VfsEvent) {
        match event {
            VfsEvent::Remove(path) => {
                let _ = backend.unwatch(path);
            }
            VfsEvent::Create(path) | VfsEvent::Write(path) => {
                backend.recover_watches(path).unwrap();
            }
            VfsEvent::Rescan => backend.revalidate_watches().unwrap(),
        }
    }

    fn event_path(event: &VfsEvent) -> Option<&Path> {
        match event {
            VfsEvent::Create(path) | VfsEvent::Write(path) | VfsEvent::Remove(path) => Some(path),
            VfsEvent::Rescan => None,
        }
    }

    /// Commits every event that arrives until none has arrived for `quiet`.
    fn settle(backend: &mut StdBackend, quiet: Duration) {
        let receiver = backend.event_receiver();
        while let Ok(event) = receiver.recv_timeout(quiet) {
            commit(backend, &event);
        }
    }

    /// Discards every event that arrives until none has arrived for `quiet`,
    /// like a watcher whose queue overflowed.
    fn drop_events(backend: &StdBackend, quiet: Duration) {
        let receiver = backend.event_receiver();
        while receiver.recv_timeout(quiet).is_ok() {}
    }

    /// Commits events until one arrives for `path`, panicking after a timeout.
    fn wait_for_event(backend: &mut StdBackend, path: &Path) -> VfsEvent {
        let receiver = backend.event_receiver();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let event = receiver
                .recv_deadline(deadline)
                .unwrap_or_else(|_| panic!("no event arrived for {}", path.display()));
            commit(backend, &event);
            if event_path(&event) == Some(path) {
                return event;
            }
        }
    }

    #[test]
    fn parent_watch_reports_recreation_of_a_removed_watch_root() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let root = dir.join("packages");
        fs_err::create_dir(&root).unwrap();
        backend.watch(&root).unwrap();
        fs_err::remove_dir_all(&root).unwrap();
        backend.unwatch(&root).unwrap();
        assert_eq!(backend.parent_watches.contains(&dir), !PATH_BASED_WATCHES);
        // Let the removal settle before recreating, so pending descendant
        // events cannot accidentally hide the lost-root-watch regression.
        settle(&mut backend, Duration::from_millis(300));
        fs_err::create_dir(&root).unwrap();

        // Depending on timing, notify reports the recreation as a creation or
        // as a write merged with the removal.
        while !matches!(
            wait_for_event(&mut backend, &root),
            VfsEvent::Create(_) | VfsEvent::Write(_)
        ) {}

        let module = root.join("module.luau");
        fs_err::write(&module, "return 1").unwrap();
        wait_for_event(&mut backend, &module);
    }

    #[test]
    fn edits_inside_a_watch_root_replaced_within_one_debounce_window_are_reported() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let root = dir.join("Packages");
        fs_err::create_dir(&root).unwrap();
        fs_err::write(root.join("old.luau"), "return 1").unwrap();
        backend.watch(&root).unwrap();
        settle(&mut backend, Duration::from_millis(300));

        // notify merges a removal and recreation of the same path within its
        // debounce window into a single write, like a package reinstall.
        fs_err::remove_dir_all(&root).unwrap();
        fs_err::create_dir(&root).unwrap();
        settle(&mut backend, Duration::from_millis(500));

        let module = root.join("new.luau");
        fs_err::write(&module, "return 2").unwrap();
        wait_for_event(&mut backend, &module);
    }

    #[test]
    fn watch_roots_recover_after_their_parent_is_replaced() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let project = dir.join("default.project.json");
        let src = dir.join("src");
        let server = src.join("server");
        let client = src.join("client");
        fs_err::write(&project, "{}").unwrap();
        fs_err::create_dir_all(&server).unwrap();
        fs_err::create_dir_all(&client).unwrap();
        backend.watch(&project).unwrap();
        backend.watch(&server).unwrap();
        backend.watch(&client).unwrap();
        settle(&mut backend, Duration::from_millis(300));

        // A branch switch removes and recreates the shared parent of several
        // mapped roots. Nothing reads `src` itself, so only the lost roots can
        // bring its watches back.
        fs_err::remove_dir_all(&src).unwrap();
        settle(&mut backend, Duration::from_millis(300));
        fs_err::create_dir(&src).unwrap();
        settle(&mut backend, Duration::from_millis(300));
        fs_err::create_dir(&server).unwrap();
        fs_err::create_dir(&client).unwrap();
        settle(&mut backend, Duration::from_millis(500));
        assert!(backend.watches.contains(&server));
        assert!(backend.watches.contains(&client));

        let module = client.join("module.luau");
        fs_err::write(&module, "return 1").unwrap();
        wait_for_event(&mut backend, &module);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn lost_roots_that_reappear_with_their_parent_are_reported() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let src = dir.join("src");
        let server = src.join("server");
        fs_err::create_dir_all(&server).unwrap();
        backend.watch(&server).unwrap();
        backend.unwatch(&src).unwrap();
        assert!(backend.lost_watches.contains(&server));

        // The root already exists when its parent's creation is handled, so
        // no event of its own will follow.
        backend.recover_watches(&src).unwrap();
        assert!(backend.watches.contains(&server));
        assert!(backend.lost_watches.is_empty());
        let receiver = backend.event_receiver();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let VfsEvent::Create(path) = receiver.recv_deadline(deadline).unwrap() {
                if path == server {
                    break;
                }
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn rescan_recovers_roots_removed_without_an_event() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let root = dir.join("Packages");
        fs_err::create_dir(&root).unwrap();
        backend.watch(&root).unwrap();
        fs_err::remove_dir_all(&root).unwrap();
        backend.revalidate_watches().unwrap();
        assert!(!backend.watches.contains(&root));
        assert!(backend.lost_watches.contains(&root));
        fs_err::create_dir(&root).unwrap();
        backend.revalidate_watches().unwrap();
        assert!(backend.watches.contains(&root));
    }

    #[test]
    fn rescan_rewatches_a_root_replaced_while_events_were_dropped() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let root = dir.join("Packages");
        fs_err::create_dir(&root).unwrap();
        backend.watch(&root).unwrap();
        settle(&mut backend, Duration::from_millis(300));

        // The root still exists when the rescan is handled, but inotify's
        // watch belongs to the deleted directory.
        fs_err::remove_dir_all(&root).unwrap();
        fs_err::create_dir_all(root.join("nested")).unwrap();
        drop_events(&backend, Duration::from_millis(300));
        backend.revalidate_watches().unwrap();
        assert!(backend.watches.contains(&root));

        // Directories created while events were dropped are watched too.
        let module = root.join("nested").join("module.luau");
        fs_err::write(&module, "return 1").unwrap();
        wait_for_event(&mut backend, &module);
    }

    #[test]
    fn replacing_a_root_nested_in_another_keeps_it_watched() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let outer = dir.join("src");
        let inner = outer.join("server");
        fs_err::create_dir_all(&inner).unwrap();
        // A project that maps both folders can read the inner one first.
        backend.watch(&inner).unwrap();
        backend.watch(&outer).unwrap();
        assert!(backend.watches.contains(&inner));
        settle(&mut backend, Duration::from_millis(300));

        fs_err::remove_dir_all(&inner).unwrap();
        fs_err::create_dir(&inner).unwrap();
        // The removal is handled only after the new directory was already
        // watched through `outer`, as when notify merges it with the
        // recreation into a single write.
        thread::sleep(Duration::from_millis(300));
        commit(&mut backend, &VfsEvent::Remove(inner.clone()));
        commit(&mut backend, &VfsEvent::Create(inner.clone()));
        assert!(!backend.lost_watches.contains(&inner));
        settle(&mut backend, Duration::from_millis(300));

        let module = inner.join("module.luau");
        fs_err::write(&module, "return 1").unwrap();
        wait_for_event(&mut backend, &module);
    }

    #[test]
    fn lost_roots_below_a_covered_path_are_forgotten() {
        let mut backend = StdBackend::new_without_watch();
        let (_dir, dir) = tempdir();
        let lost = dir.join("src").join("server");
        backend.watches.insert(dir.clone());
        backend.lost_watches.insert(lost.clone());
        // Otherwise every later event below `dir` would report it as created.
        backend.watch(&lost).unwrap();
        assert!(backend.lost_watches.is_empty());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn creation_watches_stay_inside_previously_watched_directories() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let src = dir.join("src");
        let server = src.join("server");
        fs_err::create_dir_all(&server).unwrap();
        backend.watch(&server).unwrap();
        assert!(backend.parent_watches.contains(&src));

        // With `src` gone, the nearest surviving ancestor is a directory
        // nothing watched before, like the home directory for a `..` path.
        fs_err::remove_dir_all(&src).unwrap();
        backend.unwatch(&src).unwrap();
        assert!(backend.lost_watches.contains(&server));
        assert!(backend.parent_watches.is_empty());

        // Missing paths whose parent is missing climb no further either.
        assert!(backend.watch(&dir.join("a").join("b")).is_err());
        assert!(backend.parent_watches.is_empty());

        // Their parent is watched, as it is the only way to see them appear.
        assert!(backend.watch(&dir.join("a")).is_err());
        assert!(backend.parent_watches.contains(&dir));
    }

    #[cfg(unix)]
    #[test]
    fn unwatchable_directories_are_read_without_a_watch() {
        use std::os::unix::fs::PermissionsExt;

        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let root = dir.join("root");
        let locked = root.join("locked");
        fs_err::create_dir_all(&locked).unwrap();
        fs_err::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = fs_err::read_dir(&locked).is_err();

        // inotify needs read access to every directory below the root.
        let result = backend.watch(&root);
        let degraded = backend.degraded.contains(&root);
        fs_err::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
        if INOTIFY_WATCHES && denied {
            assert!(degraded);
            assert!(!backend.watches.contains(&root));
            // Later reads do not retry until a rescan.
            backend.watch(&root).unwrap();
            assert!(!backend.watches.contains(&root));
            backend.revalidate_watches().unwrap();
            assert!(backend.degraded.is_empty());
            backend.watch(&root).unwrap();
            assert!(backend.watches.contains(&root));
        }
    }

    #[test]
    fn writes_merged_with_a_removal_are_reported_as_a_replacement() {
        let mut notices = HashMap::new();
        let now = Instant::now();
        let path = PathBuf::from("Packages");
        let mut translate = |event| translate_event(event, &mut notices, now);
        assert!(matches!(
            translate(DebouncedEvent::Write(path.clone())).as_slice(),
            [VfsEvent::Write(_)]
        ));
        assert!(translate(DebouncedEvent::NoticeRemove(path.clone())).is_empty());
        assert!(matches!(
            translate(DebouncedEvent::Write(path.clone())).as_slice(),
            [VfsEvent::Remove(removed), VfsEvent::Create(created)]
                if *removed == path && *created == path
        ));
        // The notice is consumed, so later writes are ordinary again.
        assert!(matches!(
            translate(DebouncedEvent::Write(path.clone())).as_slice(),
            [VfsEvent::Write(_)]
        ));

        let from = PathBuf::from("Packages.tmp");
        translate(DebouncedEvent::NoticeRemove(path.clone()));
        assert!(matches!(
            translate(DebouncedEvent::Rename(from.clone(), path.clone())).as_slice(),
            [VfsEvent::Remove(replaced), VfsEvent::Remove(old), VfsEvent::Create(new)]
                if *replaced == path && *old == from && *new == path
        ));
        assert!(notices.is_empty());
    }

    #[test]
    fn removal_notices_expire() {
        let mut notices = HashMap::new();
        let start = Instant::now();
        let path = PathBuf::from("Packages");
        // A rename can report its debounced event under another path, so the
        // notice for this one is never consumed.
        translate_event(
            DebouncedEvent::NoticeRemove(path.clone()),
            &mut notices,
            start,
        );
        let later = start + REMOVAL_NOTICE_LIFETIME;
        assert!(matches!(
            translate_event(DebouncedEvent::Write(path.clone()), &mut notices, later).as_slice(),
            [VfsEvent::Write(_)]
        ));
        assert!(notices.is_empty());
    }

    #[test]
    fn watcher_rescans_and_errors_are_not_dropped() {
        let mut notices = HashMap::new();
        let now = Instant::now();
        assert!(matches!(
            translate_event(DebouncedEvent::Rescan, &mut notices, now).as_slice(),
            [VfsEvent::Rescan]
        ));
        let path = PathBuf::from("Packages");
        let error = || notify::Error::Generic("overflow".into());
        assert!(matches!(
            translate_event(DebouncedEvent::Error(error(), Some(path.clone())), &mut notices, now)
                .as_slice(),
            [VfsEvent::Write(written)] if *written == path
        ));
        assert!(
            translate_event(DebouncedEvent::Error(error(), None), &mut notices, now).is_empty()
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn parent_watch_can_be_promoted_to_a_recursive_watch() {
        let mut backend = StdBackend::new().unwrap();
        let (_dir, dir) = tempdir();
        let root = dir.join("packages");
        fs_err::create_dir(&root).unwrap();
        backend.watch(&root).unwrap();
        assert!(backend.parent_watches.contains(&dir));
        backend.watch(&dir).unwrap();
        assert!(!backend.parent_watches.contains(&dir));
        assert!(backend.watches.contains(&dir));
    }

    #[test]
    fn notify_io_errors_preserve_the_os_error() {
        let err = notify_io_error(notify::Error::Io(io::Error::from_raw_os_error(5)));
        assert_eq!(err.raw_os_error(), Some(5));
        let err = notify_io_error(notify::Error::PathNotFound);
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn automatic_watch_removal_is_ignored_only_for_missing_paths() {
        let (_dir, dir) = tempdir();
        let path = dir.join("removed");
        let already_removed = notify::Error::Io(io::Error::from(io::ErrorKind::InvalidInput));
        assert!(is_removed_watch_error(&path, &already_removed));
        fs_err::create_dir(&path).unwrap();
        assert!(!is_removed_watch_error(&path, &already_removed));
        fs_err::remove_dir(&path).unwrap();
        let denied = notify::Error::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(!is_removed_watch_error(&path, &denied));
    }
}
