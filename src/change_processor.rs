use crossbeam_channel::{select, Receiver, RecvError, Sender};
use jod_thread::JoinHandle;
use memofs::{IoResultExt, Vfs, VfsEvent};
use rbx_dom_weak::types::{Ref, Variant};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{
    fs, io,
    sync::{Arc, Mutex},
};

use crate::{
    message_queue::MessageQueue,
    snapshot::{
        apply_patch_set, compute_patch_set, AppliedPatchSet, InstigatingSource, PatchSet, RojoTree,
    },
    snapshot_middleware::{
        canonicalize_existing_ancestor, get_dir_middleware, snapshot_from_vfs,
        snapshot_project_node, Middleware,
    },
};

/// Processes file change events, updates the DOM, and sends those updates
/// through a channel for other stuff to consume.
///
/// Owns the connection between Rojo's VFS and its DOM by holding onto another
/// thread that processes messages.
///
/// Consumers of ChangeProcessor, like ServeSession, are intended to communicate
/// with this object via channels.
///
/// ChangeProcessor expects to be the only writer to the RojoTree and Vfs
/// objects passed to it.
pub struct ChangeProcessor {
    /// Controls the runtime of the processor thread. When signaled, the job
    /// thread will finish its current work and terminate.
    ///
    /// This channel should be signaled before dropping ChangeProcessor or we'll
    /// hang forever waiting for the message processing loop to terminate.
    shutdown_sender: Sender<()>,

    /// A handle to the message processing thread. When dropped, we'll block
    /// until it's done.
    ///
    /// Allowed to be unused because dropping this value has side effects.
    #[allow(unused)]
    job_thread: JoinHandle<Result<(), RecvError>>,
}

impl ChangeProcessor {
    /// Spin up the ChangeProcessor, connecting it to the given tree, VFS, and
    /// outbound message queue.
    pub fn start(
        tree: Arc<Mutex<RojoTree>>,
        vfs: Arc<Vfs>,
        message_queue: Arc<MessageQueue<AppliedPatchSet>>,
        tree_mutation_receiver: Receiver<PatchSet>,
    ) -> Self {
        let (shutdown_sender, shutdown_receiver) = crossbeam_channel::bounded(1);
        let vfs_receiver = vfs.event_receiver();
        let task = JobThreadContext::new(tree, vfs, message_queue);

        let job_thread = jod_thread::Builder::new()
            .name("ChangeProcessor thread".to_owned())
            .spawn(move || {
                log::trace!("ChangeProcessor thread started");

                loop {
                    let resync_timer = match task.pending_resync_deadline() {
                        Some(deadline) => crossbeam_channel::at(deadline),
                        None => crossbeam_channel::never(),
                    };
                    select! {
                        recv(vfs_receiver) -> event => {
                            task.handle_vfs_event(event?);
                        },
                        recv(resync_timer) -> _ => {
                            task.resync_tree();
                        },
                        recv(tree_mutation_receiver) -> patch_set => {
                            task.handle_tree_event(patch_set?);
                        },
                        recv(shutdown_receiver) -> _ => {
                            log::trace!("ChangeProcessor shutdown signal received...");
                            return Ok(());
                        },
                    }
                }
            })
            .expect("Could not start ChangeProcessor thread");

        Self {
            shutdown_sender,
            job_thread,
        }
    }
}

impl Drop for ChangeProcessor {
    fn drop(&mut self) {
        // Signal the job thread to start spinning down. Without this we'll hang
        // forever waiting for the thread to finish its infinite loop.
        let _ = self.shutdown_sender.send(());

        // After this function ends, the job thread will be joined. It might
        // block for a small period of time while it processes its last work.
    }
}

/// Contains all of the state needed to synchronize the DOM and VFS.
struct JobThreadContext {
    /// A handle to the DOM we're managing.
    tree: Arc<Mutex<RojoTree>>,

    /// A handle to the VFS we're managing.
    vfs: Arc<Vfs>,

    /// Whenever changes are applied to the DOM, we should push those changes
    /// into this message queue to inform any connected clients.
    message_queue: Arc<MessageQueue<AppliedPatchSet>>,

    /// When the whole tree was last resynced because of a rescan.
    last_resync: Cell<Option<Instant>>,

    /// Whether a rescan arrived too soon after the last resync and still
    /// needs one.
    resync_pending: Cell<bool>,
}

/// The shortest time between two resyncs of the whole tree.
///
/// Watchers can report a rescan for every burst of changes they could not
/// keep up with, such as a large `git checkout` or package install, and each
/// resync snapshots the entire project.
const RESYNC_INTERVAL: Duration = Duration::from_secs(1);

impl JobThreadContext {
    fn new(
        tree: Arc<Mutex<RojoTree>>,
        vfs: Arc<Vfs>,
        message_queue: Arc<MessageQueue<AppliedPatchSet>>,
    ) -> Self {
        Self {
            tree,
            vfs,
            message_queue,
            last_resync: Cell::new(None),
            resync_pending: Cell::new(false),
        }
    }

    /// Computes and applies patches to the DOM for a given file path.
    ///
    /// This function finds the nearest ancestor to the given path that has associated instances
    /// in the tree.
    /// It then computes and applies changes for each affected instance ID and
    /// publishes them before releasing the tree.
    fn apply_patches(&self, path: PathBuf) {
        let mut tree = self.tree.lock().unwrap();
        let mut applied_patches = Vec::new();

        // Find the nearest ancestor to this path that has
        // associated instances in the tree. This helps make sure
        // that we handle additions correctly, especially if we
        // receive events for descendants of a large tree being
        // created all at once.
        let mut current_path = path.as_path();
        let affected_ids = loop {
            let ids = tree.get_ids_at_path(current_path);

            log::trace!("Path {} affects IDs {:?}", current_path.display(), ids);

            if !ids.is_empty() {
                break ids.to_vec();
            }

            log::trace!("Trying parent path...");
            match current_path.parent() {
                Some(parent) => current_path = parent,
                None => break Vec::new(),
            }
        };

        for id in affected_ids {
            if let Some(patch) = compute_and_apply_changes(&mut tree, &self.vfs, id) {
                if !patch.is_empty() {
                    applied_patches.push(patch);
                }
            }
        }

        // Initial reads take the tree lock before sampling the queue cursor.
        // Publish under that lock so a reconnect cannot replay an addition
        // already present in the tree it just received.
        if !applied_patches.is_empty() {
            self.message_queue.push_messages(&applied_patches);
        }
    }

    /// Resyncs the whole tree after the watcher reported that it may have
    /// dropped events, unless it was resynced very recently. Then the resync
    /// is deferred, so a burst of rescans costs at most one more.
    fn request_resync(&self) {
        let recently_resynced = self
            .last_resync
            .get()
            .is_some_and(|last| last.elapsed() < RESYNC_INTERVAL);
        if recently_resynced {
            self.resync_pending.set(true);
        } else {
            self.resync_tree();
        }
    }

    /// When a deferred resync is due, if there is one.
    fn pending_resync_deadline(&self) -> Option<Instant> {
        if !self.resync_pending.get() {
            return None;
        }
        Some(
            self.last_resync
                .get()
                .map_or_else(Instant::now, |last| last + RESYNC_INTERVAL),
        )
    }

    /// Snapshots the whole tree again after the watcher reported that it may
    /// have dropped events.
    fn resync_tree(&self) {
        self.resync_pending.set(false);
        log::warn!("The filesystem watcher may have missed changes; resyncing the whole project");
        // Watches can be lost along with the events, so recover them first.
        if let Err(err) = self.vfs.commit_event(&VfsEvent::Rescan) {
            log::error!("Error recovering filesystem watches: {:?}", err);
        }
        let mut tree = self.tree.lock().unwrap();
        let root_id = tree.get_root_id();
        if let Some(patch) = compute_and_apply_changes(&mut tree, &self.vfs, root_id) {
            if !patch.is_empty() {
                self.message_queue.push_messages(&[patch]);
            }
        }
        self.last_resync.set(Some(Instant::now()));
    }

    fn handle_vfs_event(&self, event: VfsEvent) {
        log::trace!("Vfs event: {:?}", event);

        // Recovering every watch is deferred along with the resync.
        if let VfsEvent::Rescan = event {
            self.request_resync();
            return;
        }

        // Update the VFS immediately with the event.
        if let Err(err) = self.vfs.commit_event(&event) {
            log::error!("Error applying VFS change: {:?}", err);
            return;
        }

        // For a given VFS event, we might have many changes to different parts
        // of the tree. Calculate and apply all of these changes.
        match canonicalize_event_path(&self.vfs, &event) {
            Some(Ok(path)) => self.apply_patches(path),
            Some(Err(err)) => {
                log::error!("Error resolving filesystem event {:?}: {:?}", event, err);
            }
            None => log::warn!("Unhandled VFS event: {:?}", event),
        }
    }

    fn handle_tree_event(&self, patch_set: PatchSet) {
        log::trace!("Applying PatchSet from client: {:#?}", patch_set);

        {
            let mut tree = self.tree.lock().unwrap();

            for &id in &patch_set.removed_instances {
                if let Some(instance) = tree.get_instance(id) {
                    if let Some(instigating_source) = &instance.metadata().instigating_source {
                        match instigating_source {
                            InstigatingSource::Path(path) => {
                                if let Err(err) = fs::remove_file(path) {
                                    log::error!(
                                        "Failed to remove file {}: {}",
                                        path.display(),
                                        err
                                    );
                                }
                            }
                            InstigatingSource::ProjectNode { .. } => {
                                log::warn!(
                                    "Cannot remove instance {:?}, it's from a project file",
                                    id
                                );
                            }
                        }
                    } else {
                        // TODO
                        log::warn!(
                            "Cannot remove instance {:?}, it is not an instigating source.",
                            id
                        );
                    }
                } else {
                    log::warn!("Cannot remove instance {:?}, it does not exist.", id);
                }
            }

            for update in &patch_set.updated_instances {
                let id = update.id;

                if let Some(instance) = tree.get_instance(id) {
                    if update.changed_name.is_some() {
                        log::warn!("Cannot rename instances yet.");
                    }

                    if update.changed_class_name.is_some() {
                        log::warn!("Cannot change ClassName yet.");
                    }

                    if update.changed_metadata.is_some() {
                        log::warn!("Cannot change metadata yet.");
                    }

                    for (key, changed_value) in &update.changed_properties {
                        if key == "Source" {
                            if let Some(instigating_source) =
                                &instance.metadata().instigating_source
                            {
                                match instigating_source {
                                    InstigatingSource::Path(path) => {
                                        if let Some(Variant::String(value)) = changed_value {
                                            match resolve_source_path(&self.vfs, path) {
                                                Ok(None) => {
                                                    // Writing would bring back a file
                                                    // whose removal is still on its way.
                                                    log::warn!(
                                                        "Not writing {}, since it no longer exists",
                                                        path.display()
                                                    );
                                                }
                                                Ok(Some(source_path)) => {
                                                    if let Err(err) = fs::write(&source_path, value)
                                                    {
                                                        log::error!(
                                                            "Failed to write file {}: {}",
                                                            source_path.display(),
                                                            err
                                                        );
                                                    }
                                                }
                                                Err(err) => {
                                                    log::error!(
                                                        "Failed to find the source file of {}: {}",
                                                        path.display(),
                                                        err
                                                    );
                                                }
                                            }
                                        } else {
                                            log::warn!("Cannot change Source to non-string value.");
                                        }
                                    }
                                    InstigatingSource::ProjectNode { .. } => {
                                        log::warn!(
                                            "Cannot remove instance {:?}, it's from a project file",
                                            id
                                        );
                                    }
                                }
                            } else {
                                log::warn!(
                                    "Cannot update instance {:?}, it is not an instigating source.",
                                    id
                                );
                            }
                        } else {
                            log::warn!("Cannot change properties besides BaseScript.Source.");
                        }
                    }
                } else {
                    log::warn!("Cannot update instance {:?}, it does not exist.", id);
                }
            }

            let applied_patch = apply_patch_set(&mut tree, patch_set);
            if !applied_patch.is_empty() {
                self.message_queue.push_messages(&[applied_patch]);
            }
        }
    }
}

/// Returns the file that holds the source of the instance created from
/// `path`, or `None` if `path` no longer exists.
fn resolve_source_path(vfs: &Vfs, path: &Path) -> io::Result<Option<PathBuf>> {
    let Some(meta) = vfs.metadata(path).with_not_found()? else {
        return Ok(None);
    };
    if meta.is_file() {
        return Ok(Some(path.to_owned()));
    }

    let (middleware, _, init_path) = get_dir_middleware(vfs, path).map_err(io::Error::other)?;
    match middleware {
        Middleware::ModuleScriptDir
        | Middleware::ServerScriptDir
        | Middleware::ClientScriptDir
        | Middleware::PluginScriptDir => Ok(Some(init_path)),
        _ => Err(io::Error::other(format!(
            "directory {} does not contain an init script",
            path.display()
        ))),
    }
}

/// Preserve canonical tree keys even when queued events outlive their files.
fn canonicalize_event_path(vfs: &Vfs, event: &VfsEvent) -> Option<io::Result<PathBuf>> {
    match event {
        VfsEvent::Create(path) | VfsEvent::Write(path) => {
            Some(canonicalize_existing_ancestor(vfs, path))
        }
        // Something may already exist at a removed path again, such as a
        // recreated symlink. Resolving it would map the removal to wherever
        // that points now, so only resolve the parent.
        VfsEvent::Remove(path) => Some(match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                canonicalize_existing_ancestor(vfs, parent).map(|parent| parent.join(name))
            }
            _ => canonicalize_existing_ancestor(vfs, path),
        }),
        _ => None,
    }
}

fn compute_and_apply_changes(tree: &mut RojoTree, vfs: &Vfs, id: Ref) -> Option<AppliedPatchSet> {
    let metadata = tree
        .get_metadata(id)
        .expect("metadata missing for instance present in tree");

    let instigating_source = match &metadata.instigating_source {
        Some(path) => path,
        None => {
            log::error!(
                "Instance {:?} did not have an instigating source, but was considered for an update.",
                id
            );
            log::error!("This is a bug. Please file an issue!");
            return None;
        }
    };

    // How we process a file change event depends on what created this
    // file/folder in the first place.
    let applied_patch_set = match instigating_source {
        InstigatingSource::Path(path) => match vfs.metadata(path).with_not_found() {
            Ok(Some(_)) => {
                // Our instance was previously created from a path and that
                // path still exists. We can generate a snapshot starting at
                // that path and use it as the source for our patch.

                let snapshot = match snapshot_from_vfs(&metadata.context, vfs, path) {
                    Ok(snapshot) => snapshot,
                    Err(err) => {
                        log::error!("Snapshot error: {:?}", err);
                        return None;
                    }
                };
                if snapshot.is_none() && id == tree.get_root_id() {
                    log::warn!(
                        "{} no longer produces an instance. Keeping the last synced project.",
                        path.display()
                    );
                    return None;
                }

                let patch_set = compute_patch_set(snapshot, tree, id);
                apply_patch_set(tree, patch_set)
            }
            Ok(None) if id == tree.get_root_id() => {
                // The root cannot be removed. Keep the last synced tree, which
                // still maps the path, so it is resynced once the path returns.
                log::warn!(
                    "{} no longer exists. Keeping the last synced project until it is restored.",
                    path.display()
                );
                return None;
            }
            Ok(None) => {
                // Our instance was previously created from a path, but that
                // path no longer exists.
                //
                // We associate deleting the instigating file for an
                // instance with deleting that instance.

                let mut patch_set = PatchSet::new();
                patch_set.removed_instances.push(id);

                apply_patch_set(tree, patch_set)
            }
            Err(err) => {
                log::error!("Error processing filesystem change: {:?}", err);
                return None;
            }
        },

        InstigatingSource::ProjectNode {
            path,
            name,
            node,
            parent_class,
        } => {
            // This instance is the direct subject of a project node. Since
            // there might be information associated with our instance from
            // the project file, we snapshot the entire project node again.

            let snapshot_result = snapshot_project_node(
                &metadata.context,
                path,
                name,
                node,
                vfs,
                parent_class.as_ref().map(|name| name.as_str()),
            );

            let snapshot = match snapshot_result {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    log::error!("{:?}", err);
                    return None;
                }
            };

            // An optional node whose path went missing produces no instance.
            // Its parent maps that path while it is missing, so resync the
            // parent to remove this instance and to notice the path returning.
            if snapshot.is_none() {
                let parent_id = tree.get_instance(id).map(|instance| instance.parent());
                if let Some(parent_id) = parent_id.filter(|parent_id| parent_id.is_some()) {
                    return compute_and_apply_changes(tree, vfs, parent_id);
                }
                // The root cannot be removed.
                if id == tree.get_root_id() {
                    log::warn!("The project root no longer produces an instance");
                    return None;
                }
            }

            let patch_set = compute_patch_set(snapshot, tree, id);
            apply_patch_set(tree, patch_set)
        }
    };

    Some(applied_patch_set)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::snapshot::{InstanceContext, InstanceSnapshot, PatchUpdate};
    use rbx_dom_weak::{ustr, UstrMap};

    fn context(path: &Path) -> JobThreadContext {
        context_with_vfs(Vfs::new_default_without_watch(), path)
    }

    fn context_with_vfs(vfs: Vfs, path: &Path) -> JobThreadContext {
        let snapshot = snapshot_from_vfs(&InstanceContext::default(), &vfs, path)
            .unwrap()
            .unwrap();
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let patch = compute_patch_set(Some(snapshot), &tree, tree.get_root_id());
        apply_patch_set(&mut tree, patch);
        JobThreadContext::new(
            Arc::new(Mutex::new(tree)),
            Arc::new(vfs),
            Arc::new(MessageQueue::new()),
        )
    }

    #[test]
    fn directory_write_reconciles_children_without_a_child_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let task = context(&root);
        let added = root.join("package.luau");
        fs::write(&added, "return 'new'").unwrap();
        task.handle_vfs_event(VfsEvent::Write(root));
        assert!(!task.tree.lock().unwrap().get_ids_at_path(&added).is_empty());
    }

    #[test]
    fn unchanged_project_directory_write_does_not_publish_a_patch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let packages = root.join("Packages");
        fs::create_dir(&packages).unwrap();
        fs::write(packages.join("module.luau"), "return 1").unwrap();
        let project = root.join("default.project.json");
        fs::write(
            &project,
            r#"{"name":"test","tree":{"$className":"Folder","Packages":{"$path":"Packages"}}}"#,
        )
        .unwrap();
        let task = context(&project);
        task.handle_vfs_event(VfsEvent::Write(packages));
        assert_eq!(task.message_queue.cursor(), 0);
    }

    #[test]
    fn unchanged_reference_directory_write_does_not_publish_a_patch() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("rojo-test/serve-tests/ref_properties_remove");
        let task = context(&fixture.join("default.project.json"));
        task.handle_vfs_event(VfsEvent::Write(fixture.join("src")));
        let mut pending = task.message_queue.subscribe(0);
        assert_eq!(task.message_queue.cursor(), 0, "{:?}", pending.try_recv());
    }

    #[test]
    fn stale_create_write_and_remove_events_reconcile_and_keep_processing() {
        for make_event in [VfsEvent::Create, VfsEvent::Write, VfsEvent::Remove] {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            let nested = root.join("nested").join("deeper");
            fs::create_dir_all(&nested).unwrap();
            let script = nested.join("script.luau");
            fs::write(&script, "return 1").unwrap();
            let task = context(&root);
            let script_id = task.tree.lock().unwrap().get_ids_at_path(&script)[0];

            fs::remove_dir_all(root.join("nested")).unwrap();
            task.handle_vfs_event(make_event(script));
            task.handle_vfs_event(VfsEvent::Remove(nested));
            task.handle_vfs_event(VfsEvent::Remove(root.join("nested")));
            assert!(task.tree.lock().unwrap().get_instance(script_id).is_none());
            assert!(task.tree.lock().unwrap().root().children().is_empty());

            let replacement = root.join("replacement.luau");
            fs::write(&replacement, "return 2").unwrap();
            task.handle_vfs_event(VfsEvent::Create(replacement.clone()));
            let tree = task.tree.lock().unwrap();
            let id = tree.get_ids_at_path(&replacement)[0];
            assert_eq!(
                tree.get_instance(id).unwrap().properties()[&ustr("Source")],
                Variant::String("return 2".into())
            );
        }
    }

    fn write_project(root: &Path, tree: &str) -> PathBuf {
        let project = root.join("default.project.json");
        fs::write(&project, format!(r#"{{"name":"test","tree":{tree}}}"#)).unwrap();
        project
    }

    #[test]
    fn recreating_a_missing_project_path_resyncs_its_node() {
        for packages_node in [
            r#"{"$className":"Folder","$path":"Packages"}"#,
            r#"{"$path":{"optional":"Packages"}}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dunce::canonicalize(dir.path()).unwrap();
            let packages = root.join("Packages");
            fs::create_dir(&packages).unwrap();
            fs::write(packages.join("old.luau"), "return 1").unwrap();
            let project = write_project(
                &root,
                &format!(r#"{{"$className":"Folder","Packages":{packages_node}}}"#),
            );
            let task = context(&project);

            // A package manager deletes the folder and only later recreates it.
            fs::remove_dir_all(&packages).unwrap();
            task.handle_vfs_event(VfsEvent::Remove(packages.clone()));
            assert!(task
                .tree
                .lock()
                .unwrap()
                .get_ids_at_path(&packages.join("old.luau"))
                .is_empty());

            fs::create_dir(&packages).unwrap();
            let added = packages.join("new.luau");
            fs::write(&added, "return 2").unwrap();
            task.handle_vfs_event(VfsEvent::Create(packages.clone()));
            assert!(
                !task.tree.lock().unwrap().get_ids_at_path(&added).is_empty(),
                "{packages_node}"
            );
        }
    }

    #[test]
    fn recreating_the_parent_of_mapped_roots_resyncs_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let src = root.join("src");
        for name in ["server", "client"] {
            fs::create_dir_all(src.join(name)).unwrap();
            fs::write(src.join(name).join("old.luau"), "return 1").unwrap();
        }
        let project = write_project(
            &root,
            r#"{"$className":"Folder","Server":{"$path":"src/server"},"Client":{"$path":"src/client"}}"#,
        );
        let task = context_with_vfs(Vfs::new_default().unwrap(), &project);

        let receiver = task.vfs.event_receiver();
        let settle = || {
            while let Ok(event) = receiver.recv_timeout(std::time::Duration::from_millis(300)) {
                task.handle_vfs_event(event);
            }
        };

        // A branch switch replaces `src`, which nothing maps directly. Its
        // removal drops every watch below it, including the one that would
        // report its children coming back.
        fs::remove_dir_all(&src).unwrap();
        settle();
        task.handle_vfs_event(VfsEvent::Remove(src.clone()));
        fs::create_dir(&src).unwrap();
        settle();

        let added = src.join("client").join("new.luau");
        fs::create_dir(src.join("server")).unwrap();
        fs::create_dir(src.join("client")).unwrap();
        fs::write(&added, "return 2").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while task.tree.lock().unwrap().get_ids_at_path(&added).is_empty() {
            let event = receiver
                .recv_deadline(deadline)
                .expect("recreated roots were never resynced");
            task.handle_vfs_event(event);
        }
    }

    #[test]
    fn rescan_resyncs_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        let task = context(&root);
        let added = nested.join("missed.luau");
        fs::write(&added, "return 1").unwrap();
        task.handle_vfs_event(VfsEvent::Rescan);
        assert!(!task.tree.lock().unwrap().get_ids_at_path(&added).is_empty());
    }

    #[test]
    fn rescans_soon_after_a_resync_are_coalesced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let task = context(&root);
        task.handle_vfs_event(VfsEvent::Rescan);
        assert!(task.pending_resync_deadline().is_none());

        let added = root.join("missed.luau");
        fs::write(&added, "return 1").unwrap();
        task.handle_vfs_event(VfsEvent::Rescan);
        task.handle_vfs_event(VfsEvent::Rescan);
        assert!(task.tree.lock().unwrap().get_ids_at_path(&added).is_empty());
        let deadline = task.pending_resync_deadline().unwrap();
        assert!(deadline <= Instant::now() + RESYNC_INTERVAL);

        // The processor thread resyncs once the deadline passes.
        task.resync_tree();
        assert!(task.pending_resync_deadline().is_none());
        assert!(!task.tree.lock().unwrap().get_ids_at_path(&added).is_empty());
    }

    #[test]
    fn removing_the_served_path_keeps_the_root() {
        // Served as a project file, and as a plain directory.
        for serve_project in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let base = dunce::canonicalize(dir.path()).unwrap();
            let root = base.join("game");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("module.luau"), "return 1").unwrap();
            let served = if serve_project {
                write_project(&base, r#"{"$path":"game"}"#)
            } else {
                root.clone()
            };
            let task = context(&served);
            let root_id = task.tree.lock().unwrap().get_root_id();

            // A branch switch can remove the served path, and a rescan or its
            // own removal event can be handled before it comes back.
            if serve_project {
                fs::remove_file(&served).unwrap();
            } else {
                fs::remove_dir_all(&served).unwrap();
            }
            task.handle_vfs_event(VfsEvent::Rescan);
            task.handle_vfs_event(VfsEvent::Remove(served.clone()));
            assert!(task.tree.lock().unwrap().get_instance(root_id).is_some());

            // The processor keeps going, and resyncs once the path is back.
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("added.luau"), "return 2").unwrap();
            if serve_project {
                write_project(&base, r#"{"$path":"game"}"#);
            }
            task.handle_vfs_event(VfsEvent::Create(served.clone()));
            let tree = task.tree.lock().unwrap();
            assert!(
                !tree.get_ids_at_path(&root.join("added.luau")).is_empty(),
                "serve_project: {serve_project}"
            );
        }
    }

    #[test]
    fn two_way_sync_does_not_recreate_a_deleted_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let script = root.join("module.luau");
        fs::write(&script, "return 1").unwrap();
        let task = context(&root);
        let id = task.tree.lock().unwrap().get_ids_at_path(&script)[0];

        // Studio sends an edit before the removal event is handled.
        fs::remove_file(&script).unwrap();
        let mut properties = UstrMap::default();
        properties.insert(ustr("Source"), Some(Variant::String("return 2".into())));
        task.handle_tree_event(PatchSet {
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_properties: properties,
                changed_metadata: None,
            }],
            ..PatchSet::new()
        });
        assert!(!script.exists());
    }

    #[cfg(unix)]
    #[test]
    fn removal_of_a_recreated_symlink_keeps_its_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let target = root.join("target.luau");
        let link = root.join("link.luau");
        fs::write(&target, "return 1").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let vfs = Vfs::new_default_without_watch();
        assert_eq!(
            canonicalize_event_path(&vfs, &VfsEvent::Remove(link.clone()))
                .unwrap()
                .unwrap(),
            link
        );
    }

    #[test]
    fn event_path_resolves_missing_descendants_of_a_replaced_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let replaced = root.join("replaced");
        fs::write(&replaced, "now a file").unwrap();
        let vfs = Vfs::new_default_without_watch();
        let missing = replaced.join("child.luau");
        assert_eq!(
            canonicalize_event_path(&vfs, &VfsEvent::Remove(missing.clone()))
                .unwrap()
                .unwrap(),
            missing
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_events_through_symlinks_use_canonical_tree_paths() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let alias = dir.path().join("alias");
        let nested = real.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("script.luau"), "return 1").unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let task = context(&real);
        fs::remove_dir_all(&nested).unwrap();
        task.handle_vfs_event(VfsEvent::Remove(alias.join("nested/script.luau")));
        task.handle_vfs_event(VfsEvent::Remove(alias.join("nested")));
        assert!(task.tree.lock().unwrap().root().children().is_empty());
    }

    #[test]
    fn two_way_sync_updates_init_source_and_preserves_children() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let folder = root.join("module");
        fs::create_dir(&folder).unwrap();
        let init = folder.join("init.luau");
        fs::write(&init, "return 1").unwrap();
        fs::write(folder.join("child.luau"), "return 'child'").unwrap();
        let task = context(&root);
        let id = task.tree.lock().unwrap().get_ids_at_path(&folder)[0];
        let mut properties = UstrMap::default();
        properties.insert(ustr("Source"), Some(Variant::String("return 2".into())));
        task.handle_tree_event(PatchSet {
            updated_instances: vec![PatchUpdate {
                id,
                changed_name: None,
                changed_class_name: None,
                changed_properties: properties,
                changed_metadata: None,
            }],
            ..PatchSet::new()
        });
        assert_eq!(fs::read_to_string(&init).unwrap(), "return 2");
        assert!(folder.join("child.luau").is_file());
    }

    #[test]
    fn init_source_resolution_matches_snapshot_priority() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in [
            "init.lua",
            "init.luau",
            "init.server.luau",
            "init.plugin.luau",
        ] {
            fs::write(root.join(name), "return 1").unwrap();
        }
        let vfs = Vfs::new_default_without_watch();
        assert_eq!(
            resolve_source_path(&vfs, root).unwrap(),
            Some(root.join("init.luau"))
        );
        fs::remove_file(root.join("init.luau")).unwrap();
        assert_eq!(
            resolve_source_path(&vfs, root).unwrap(),
            Some(root.join("init.lua"))
        );
        fs::write(root.join("default.project.json"), "{}").unwrap();
        assert!(resolve_source_path(&vfs, root).is_err());
    }
}
