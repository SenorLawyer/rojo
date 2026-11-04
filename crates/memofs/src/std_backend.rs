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
            for event in notify_rx {
                match event {
                    DebouncedEvent::Create(path) => {
                        tx.send(VfsEvent::Create(path))?;
                    }
                    DebouncedEvent::Write(path) => {
                        tx.send(VfsEvent::Write(path))?;
                    }
                    DebouncedEvent::Remove(path) => {
                        tx.send(VfsEvent::Remove(path))?;
                    }
                    DebouncedEvent::Rename(from, to) => {
                        tx.send(VfsEvent::Remove(from))?;
                        tx.send(VfsEvent::Create(to))?;
                    }
                    _ => {}
                }
            }

            Result::<(), crossbeam_channel::SendError<VfsEvent>>::Ok(())
        });

        self.watcher = Some(watcher);
        Ok(())
    }
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
        if self.watches.contains(path)
            || path
                .ancestors()
                .any(|ancestor| self.watches.contains(ancestor))
        {
            Ok(())
        } else {
            self.initialize_watcher()?;
            // A watch on a directory itself dies when that directory is
            // deleted. Its surviving parent must report recreation so mapped
            // roots recover after a package reinstall or branch switch.
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                if !self.parent_watches.contains(parent)
                    && !parent
                        .ancestors()
                        .any(|ancestor| self.watches.contains(ancestor))
                {
                    self.watcher
                        .as_mut()
                        .unwrap()
                        .watch(parent, RecursiveMode::NonRecursive)
                        .map_err(notify_io_error)?;
                    self.parent_watches.insert(parent.to_owned());
                }
            }
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
            Ok(())
        }
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
            self.watches.remove(&watched);
            self.parent_watches.remove(&watched);
            if let Some(watcher) = &mut self.watcher {
                if let Err(err) = watcher.unwatch(&watched) {
                    result = Err(notify_io_error(err));
                }
            }
        }
        // Unwatching an unregistered child can restart an entire notify
        // event stream on macOS and lose unrelated events during the restart.
        result
    }
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
        thread::sleep(Duration::from_millis(150));
        fs_err::create_dir(&root).unwrap();

        let receiver = backend.event_receiver();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let VfsEvent::Create(path) = receiver.recv_deadline(deadline).unwrap() {
                if path.file_name() == root.file_name() {
                    break;
                }
            }
        }
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
}
