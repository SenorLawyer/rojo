use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::{collections::HashSet, io};

use crossbeam_channel::{Receiver, Sender};
use notify::{watcher, DebouncedEvent, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{DirEntry, Metadata, ReadDir, VfsBackend, VfsEvent};

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
        }
    }

    fn initialize_watcher(&mut self) -> io::Result<()> {
        if self.watcher.is_some() {
            return Ok(());
        }
        let (notify_tx, notify_rx) = mpsc::channel();
        let watcher = watcher(notify_tx, Duration::from_millis(50)).map_err(notify_io_error)?;
        let tx = self.watcher_sender.clone();

        thread::spawn(move || {
            let mut removal_notices = HashSet::new();
            for event in notify_rx {
                for event in translate_event(event, &mut removal_notices) {
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

    /// Makes sure that the creation of `path` is reported by watching its
    /// nearest existing ancestor.
    ///
    /// A watch on a directory itself dies when that directory is deleted, so
    /// a surviving ancestor must report its recreation for mapped roots to
    /// recover after a package reinstall or branch switch. Failing to register
    /// that watch only costs this recovery, so it is logged instead of failing
    /// the read that asked for the watch.
    fn ensure_creation_watch(&mut self, path: &Path) {
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
}

/// Converts a debounced notify event into the events the Vfs understands.
///
/// `removal_notices` holds paths whose removal notify announced before its
/// debounced event. notify merges a removal followed by a recreation of the
/// same path into one write, which would otherwise hide that every watch
/// registered on the old directory is now dead.
fn translate_event(event: DebouncedEvent, removal_notices: &mut HashSet<PathBuf>) -> Vec<VfsEvent> {
    match event {
        DebouncedEvent::NoticeRemove(path) => {
            removal_notices.insert(path);
            Vec::new()
        }
        DebouncedEvent::Create(path) => {
            removal_notices.remove(&path);
            vec![VfsEvent::Create(path)]
        }
        DebouncedEvent::Write(path) => {
            if removal_notices.remove(&path) {
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
            if removal_notices.remove(&to) {
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

    fn watch(&mut self, path: &Path) -> io::Result<()> {
        if self.is_covered(path) {
            return Ok(());
        }
        self.initialize_watcher()?;
        self.ensure_creation_watch(path);
        let watcher = self.watcher.as_mut().unwrap();
        if self.parent_watches.remove(path) {
            watcher.unwatch(path).map_err(notify_io_error)?;
        }
        watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(notify_io_error)?;
        // Failed registrations must remain retryable after a transient
        // deletion or access error.
        self.watches.insert(path.to_path_buf());
        // Lost roots below this path are covered by its recursive watch now.
        self.lost_watches.retain(|lost| !lost.starts_with(path));
        Ok(())
    }

    fn unwatch(&mut self, path: &Path) -> io::Result<()> {
        let removed_watches: Vec<_> = self
            .watches
            .iter()
            .chain(self.parent_watches.iter())
            .filter(|watched| watched.starts_with(path))
            .cloned()
            .collect();
        let mut result = Ok(());
        for watched in removed_watches {
            if self.watches.remove(&watched) {
                self.lost_watches.insert(watched.clone());
            }
            self.parent_watches.remove(&watched);
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
    already_removed
        && matches!(fs_err::symlink_metadata(path), Err(err) if err.kind() == io::ErrorKind::NotFound)
}

fn notify_io_error(err: notify::Error) -> io::Error {
    match err {
        notify::Error::Io(err) => err,
        err => io::Error::other(err),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn watcher_is_allocated_only_when_requested() {
        let mut backend = StdBackend::new_without_watch();
        let dir = tempfile::tempdir().unwrap();
        backend.read_dir(dir.path()).unwrap();
        assert!(backend.watcher.is_none());
        backend.watch(dir.path()).unwrap();
        assert!(backend.watcher.is_some());
        assert!(backend.watches.contains(dir.path()));
    }

    #[test]
    fn failed_watch_registration_can_be_retried() {
        let mut backend = StdBackend::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(backend.watch(&missing).is_err());
        assert!(!backend.watches.contains(&missing));
        fs_err::create_dir(&missing).unwrap();
        backend.watch(&missing).unwrap();
        assert!(backend.watches.contains(&missing));
    }

    #[test]
    fn unwatching_unknown_children_keeps_the_registered_root() {
        let mut backend = StdBackend::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        backend.watch(dir.path()).unwrap();
        backend.unwatch(&dir.path().join("unregistered")).unwrap();
        assert!(backend.watches.contains(dir.path()));
    }

    #[test]
    fn removing_a_directory_forgets_watches_on_its_descendants() {
        let mut backend = StdBackend::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        fs_err::create_dir(&child).unwrap();
        backend.watch(&child).unwrap();
        backend.unwatch(dir.path()).unwrap();
        assert!(backend.watches.is_empty());
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

    /// Commits events until one arrives for `path`, panicking after a timeout.
    fn wait_for_event(backend: &mut StdBackend, path: &Path) -> VfsEvent {
        let receiver = backend.event_receiver();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
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
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("packages");
        fs_err::create_dir(&root).unwrap();
        backend.watch(&root).unwrap();
        fs_err::remove_dir_all(&root).unwrap();
        backend.unwatch(&root).unwrap();
        assert!(backend.parent_watches.contains(dir.path()));
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
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Packages");
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
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("default.project.json");
        let src = dir.path().join("src");
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

    #[test]
    fn lost_roots_that_reappear_with_their_parent_are_reported() {
        let mut backend = StdBackend::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
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
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let VfsEvent::Create(path) = receiver.recv_deadline(deadline).unwrap() {
                if path == server {
                    break;
                }
            }
        }
    }

    #[test]
    fn rescan_recovers_roots_removed_without_an_event() {
        let mut backend = StdBackend::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Packages");
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
    fn writes_merged_with_a_removal_are_reported_as_a_replacement() {
        let mut notices = HashSet::new();
        let path = PathBuf::from("Packages");
        assert!(matches!(
            translate_event(DebouncedEvent::Write(path.clone()), &mut notices).as_slice(),
            [VfsEvent::Write(_)]
        ));
        assert!(
            translate_event(DebouncedEvent::NoticeRemove(path.clone()), &mut notices).is_empty()
        );
        assert!(matches!(
            translate_event(DebouncedEvent::Write(path.clone()), &mut notices).as_slice(),
            [VfsEvent::Remove(removed), VfsEvent::Create(created)]
                if *removed == path && *created == path
        ));
        // The notice is consumed, so later writes are ordinary again.
        assert!(matches!(
            translate_event(DebouncedEvent::Write(path.clone()), &mut notices).as_slice(),
            [VfsEvent::Write(_)]
        ));

        let from = PathBuf::from("Packages.tmp");
        translate_event(DebouncedEvent::NoticeRemove(path.clone()), &mut notices);
        assert!(matches!(
            translate_event(DebouncedEvent::Rename(from.clone(), path.clone()), &mut notices)
                .as_slice(),
            [VfsEvent::Remove(replaced), VfsEvent::Remove(old), VfsEvent::Create(new)]
                if *replaced == path && *old == from && *new == path
        ));
        assert!(notices.is_empty());
    }

    #[test]
    fn watcher_rescans_and_errors_are_not_dropped() {
        let mut notices = HashSet::new();
        assert!(matches!(
            translate_event(DebouncedEvent::Rescan, &mut notices).as_slice(),
            [VfsEvent::Rescan]
        ));
        let path = PathBuf::from("Packages");
        let error = || notify::Error::Generic("overflow".into());
        assert!(matches!(
            translate_event(DebouncedEvent::Error(error(), Some(path.clone())), &mut notices)
                .as_slice(),
            [VfsEvent::Write(written)] if *written == path
        ));
        assert!(translate_event(DebouncedEvent::Error(error(), None), &mut notices).is_empty());
    }

    #[test]
    fn parent_watch_can_be_promoted_to_a_recursive_watch() {
        let mut backend = StdBackend::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("packages");
        fs_err::create_dir(&root).unwrap();
        backend.watch(&root).unwrap();
        assert!(backend.parent_watches.contains(dir.path()));
        backend.watch(dir.path()).unwrap();
        assert!(!backend.parent_watches.contains(dir.path()));
        assert!(backend.watches.contains(dir.path()));
    }

    #[test]
    fn notify_io_errors_preserve_the_os_error() {
        let err = notify_io_error(notify::Error::Io(io::Error::from_raw_os_error(5)));
        assert_eq!(err.raw_os_error(), Some(5));
    }

    #[test]
    fn automatic_watch_removal_is_ignored_only_for_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("removed");
        let already_removed = notify::Error::Io(io::Error::from(io::ErrorKind::InvalidInput));
        assert!(is_removed_watch_error(&path, &already_removed));
        fs_err::create_dir(&path).unwrap();
        assert!(!is_removed_watch_error(&path, &already_removed));
        fs_err::remove_dir(&path).unwrap();
        let denied = notify::Error::Io(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(!is_removed_watch_error(&path, &denied));
    }
}
