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
    message_queue::{MessageQueue, MessageWeight},
    snapshot::{
        apply_patch_set, compute_patch_set, AppliedPatchSet, InstigatingSource, PatchSet, RojoTree,
    },
    snapshot_middleware::{
        canonicalize_existing_ancestor, get_dir_middleware, snapshot_from_vfs,
        snapshot_project_node, Middleware,
    },
};

/// An applied patch together with the subscription message describing it.
///
/// The message is built while the tree lock that applied the patch is still
/// held, because later patches can remove or replace the instances it adds.
#[derive(Debug, Clone)]
pub struct PublishedPatch {
    pub patch: AppliedPatchSet,
    pub message: crate::web::interface::SubscribeMessage<'static>,
    /// Estimated memory kept alive by this entry, used to bound the history
    /// of the message queue.
    weight: usize,
}

impl PublishedPatch {
    pub(crate) fn new(tree: &RojoTree, patch: AppliedPatchSet) -> Self {
        let message =
            crate::web::interface::SubscribeMessage::from_patch_update(tree, patch.clone())
                .into_owned();
        let weight = message.approximate_size() + approximate_patch_size(&patch);
        Self {
            patch,
            message,
            weight,
        }
    }
}

impl MessageWeight for PublishedPatch {
    fn weight(&self) -> usize {
        self.weight
    }
}

/// Estimates the memory an [`AppliedPatchSet`] keeps alive. Only the parts
/// that can grow large, such as script sources, are measured precisely.
fn approximate_patch_size(patch: &AppliedPatchSet) -> usize {
    let ids = (patch.removed.len() + patch.added.len()) * std::mem::size_of::<Ref>();
    let updates: usize = patch
        .updated
        .iter()
        .map(|update| {
            std::mem::size_of_val(update)
                + update.changed_name.as_ref().map_or(0, String::len)
                + update
                    .changed_properties
                    .values()
                    .map(|value| {
                        value
                            .as_ref()
                            .map_or(0, crate::web::interface::approximate_variant_size)
                    })
                    .sum::<usize>()
        })
        .sum();
    std::mem::size_of_val(patch) + ids + updates
}

/// How long the filesystem must be quiet before paths that may have been
/// snapshotted half-finished are reconciled again.
const SETTLE_DELAY: Duration = Duration::from_millis(250);

/// The longest that reconciling those paths again can be postponed by
/// continuing filesystem activity.
const MAX_SETTLE_DELAY: Duration = Duration::from_secs(2);

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
        message_queue: Arc<MessageQueue<PublishedPatch>>,
        tree_mutation_receiver: Receiver<PatchSet>,
    ) -> Self {
        let (shutdown_sender, shutdown_receiver) = crossbeam_channel::bounded(1);
        let vfs_receiver = vfs.event_receiver();
        let task = JobThreadContext::new(tree, vfs, message_queue);

        let job_thread = jod_thread::Builder::new()
            .name("ChangeProcessor thread".to_owned())
            .spawn(move || {
                log::trace!("ChangeProcessor thread started");
                let mut unsettled_paths = std::collections::BTreeSet::new();
                let mut unsettled_since = None;
                let mut settle = crossbeam_channel::never();

                loop {
                    let resync_timer = match task.pending_resync_deadline() {
                        Some(deadline) => crossbeam_channel::at(deadline),
                        None => crossbeam_channel::never(),
                    };
                    select! {
                        recv(vfs_receiver) -> event => {
                            let mut events = vec![event?];
                            events.extend(vfs_receiver.try_iter().take(4095));
                            unsettled_paths.extend(task.handle_vfs_events(events));
                            if !unsettled_paths.is_empty() {
                                // Wait for activity to settle, but not forever: steady
                                // unrelated events would otherwise postpone recovery
                                // indefinitely while `unsettled_paths` keeps growing.
                                let now = Instant::now();
                                let first = *unsettled_since.get_or_insert(now);
                                let deadline = (now + SETTLE_DELAY).min(first + MAX_SETTLE_DELAY);
                                settle = crossbeam_channel::at(deadline);
                            }
                        },
                        recv(settle) -> _ => {
                            // Directory extraction can finish between the final OS event
                            // and our failed snapshot. Reconcile the settled paths once
                            // more so a partial package does not require a manual edit.
                            task.apply_patches(std::mem::take(&mut unsettled_paths).into_iter().collect());
                            unsettled_since = None;
                            settle = crossbeam_channel::never();
                        },
                        recv(resync_timer) -> _ => {
                            // A failed resync schedules its own retry.
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
    message_queue: Arc<MessageQueue<PublishedPatch>>,

    /// When the whole tree was last resynced because of a rescan.
    last_resync: Cell<Option<Instant>>,

    /// Whether a rescan arrived too soon after the last resync, or the last
    /// resync failed, and the tree still needs one.
    resync_pending: Cell<bool>,

    /// How many resyncs in a row failed, such as while a required `$path`
    /// was briefly missing or a project file was half written.
    resync_failures: Cell<u32>,
}

/// The shortest time between two resyncs of the whole tree.
///
/// Watchers can report a rescan for every burst of changes they could not
/// keep up with, such as a large `git checkout` or package install, and each
/// resync snapshots the entire project.
const RESYNC_INTERVAL: Duration = Duration::from_secs(1);

/// The longest time between retries of a resync that keeps failing.
const MAX_RESYNC_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// The tree could not be brought up to date with the filesystem, for example
/// because a snapshot failed, and still holds what was synced before.
struct StaleTree;

impl JobThreadContext {
    fn new(
        tree: Arc<Mutex<RojoTree>>,
        vfs: Arc<Vfs>,
        message_queue: Arc<MessageQueue<PublishedPatch>>,
    ) -> Self {
        Self {
            tree,
            vfs,
            message_queue,
            last_resync: Cell::new(None),
            resync_pending: Cell::new(false),
            resync_failures: Cell::new(0),
        }
    }

    /// The shortest time between the last resync and the next one. It grows
    /// while resyncs keep failing, so a project that stays broken is not
    /// snapshotted every second.
    fn resync_interval(&self) -> Duration {
        let doublings = self.resync_failures.get().saturating_sub(1).min(16);
        (RESYNC_INTERVAL * 2u32.pow(doublings)).min(MAX_RESYNC_RETRY_INTERVAL)
    }

    /// Computes and applies patches to the DOM for a batch of file paths.
    ///
    /// For each path, this finds the nearest ancestor that has associated
    /// instances in the tree. Overlapping instances are coalesced so each DOM
    /// subtree is reconciled once. It then computes and applies changes for
    /// each remaining instance ID and publishes them before releasing the tree.
    fn apply_patches(&self, paths: Vec<PathBuf>) {
        let mut tree = self.tree.lock().unwrap();
        let mut applied_patches = Vec::new();

        // Find the nearest ancestor to each path that has
        // associated instances in the tree. This helps make sure
        // that we handle additions correctly, especially if we
        // receive events for descendants of a large tree being
        // created all at once.
        let mut affected_ids = std::collections::HashSet::new();
        for path in &paths {
            for ancestor in path.ancestors() {
                let ids = tree.get_ids_at_path(ancestor);
                if !ids.is_empty() {
                    affected_ids.extend(ids.iter().copied());
                    break;
                }
            }
        }
        // Coalesce DOM subtrees, not filesystem paths: two project nodes can
        // map the same file independently, and both must be reconciled.
        //
        // When a root cannot be reconciled, for example because a sibling file
        // in the same batch is half written, the tree keeps its old subtree.
        // The affected instances below it are then reconciled on their own in
        // the next round, so a valid change next to a broken file is not lost.
        let mut pending = affected_ids;
        while !pending.is_empty() {
            let roots: Vec<_> = pending
                .iter()
                .copied()
                .filter(|id| {
                    let Some(instance) = tree.get_instance(*id) else {
                        return true;
                    };
                    let mut parent = instance.parent();
                    while let Some(instance) = tree.get_instance(parent) {
                        if pending.contains(&parent) {
                            return false;
                        }
                        parent = instance.parent();
                    }
                    true
                })
                .collect();

            let mut reconciled = std::collections::HashSet::new();
            for id in roots {
                pending.remove(&id);
                // Reconciling an earlier root can remove a later one, for
                // example when a missing optional project path re-snapshots
                // its parent.
                if tree.get_instance(id).is_none() {
                    continue;
                }
                if let Ok(patch) = compute_and_apply_changes(&mut tree, &self.vfs, id) {
                    reconciled.insert(id);
                    if let Some(patch) = patch.filter(|patch| !patch.is_empty()) {
                        // Freeze additions before another filesystem event
                        // can delete or replace them. Subscription history
                        // must not read the future tree.
                        applied_patches.push(PublishedPatch::new(&tree, patch));
                    }
                }
            }

            // Everything below a reconciled root was reconciled along with
            // it. Instances it removed are gone from the tree.
            pending.retain(|id| {
                let Some(instance) = tree.get_instance(*id) else {
                    return false;
                };
                let mut parent = instance.parent();
                while let Some(instance) = tree.get_instance(parent) {
                    if reconciled.contains(&parent) {
                        return false;
                    }
                    parent = instance.parent();
                }
                true
            });
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
    ///
    /// Returns whether the tree was resynced now. It was not when the resync
    /// was deferred or failed, and then a retry is scheduled.
    fn request_resync(&self) -> bool {
        let recently_resynced = self
            .last_resync
            .get()
            .is_some_and(|last| last.elapsed() < self.resync_interval());
        if recently_resynced {
            self.resync_pending.set(true);
            false
        } else {
            self.resync_tree()
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
                .map_or_else(Instant::now, |last| last + self.resync_interval()),
        )
    }

    /// Snapshots the whole tree again after the watcher reported that it may
    /// have dropped events.
    ///
    /// A resync that fails is retried, since the events it was meant to make
    /// up for are gone, and nothing else would bring the tree up to date.
    ///
    /// Returns whether the tree was resynced.
    fn resync_tree(&self) -> bool {
        self.resync_pending.set(false);
        let failures = self.resync_failures.get();
        if failures == 0 {
            log::warn!(
                "The filesystem watcher may have missed changes; resyncing the whole project"
            );
        } else {
            log::debug!("Retrying the resync of the whole project");
        }
        // Watches can be lost along with the events, so recover them first.
        if let Err(err) = self.vfs.commit_event(&VfsEvent::Rescan) {
            log::error!("Error recovering filesystem watches: {:?}", err);
        }
        let mut tree = self.tree.lock().unwrap();
        let root_id = tree.get_root_id();
        let result = compute_and_apply_changes(&mut tree, &self.vfs, root_id);
        let resynced = match result {
            Ok(patch) => {
                if let Some(patch) = patch.filter(|patch| !patch.is_empty()) {
                    // Publish under the tree lock, like `apply_patches`, so the
                    // message is built from the tree this patch produced.
                    let message = PublishedPatch::new(&tree, patch);
                    self.message_queue.push_messages(&[message]);
                }
                true
            }
            Err(StaleTree) => false,
        };
        drop(tree);
        self.last_resync.set(Some(Instant::now()));
        if resynced {
            if failures > 0 {
                log::info!("Resynced the whole project");
            }
            self.resync_failures.set(0);
        } else {
            self.resync_failures.set(failures + 1);
            self.resync_pending.set(true);
            if failures == 0 {
                log::warn!(
                    "Could not resync the whole project, so it may be out of date. Retrying until it succeeds."
                );
            }
        }
        resynced
    }

    #[cfg(test)]
    fn handle_vfs_event(&self, event: VfsEvent) {
        self.handle_vfs_events(vec![event]);
    }

    /// Commits a batch of VFS events and reconciles every path they touch.
    ///
    /// Returns the canonical paths that may have been observed half-finished,
    /// so the caller can reconcile them once more after the filesystem
    /// settles.
    fn handle_vfs_events(&self, events: Vec<VfsEvent>) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut settle_paths = Vec::new();
        let mut rescan = false;
        for event in events {
            log::trace!("Vfs event: {:?}", event);

            // Recovering every watch is deferred along with the resync.
            if let VfsEvent::Rescan = event {
                rescan = true;
                continue;
            }

            // Update the VFS immediately with the event.
            if let Err(err) = self.vfs.commit_event(&event) {
                log::error!("Error applying VFS change: {:?}", err);
                continue;
            }

            match canonicalize_event_path(&self.vfs, &event) {
                Some(Ok(path)) => {
                    // Only creations, removals and directory changes can be
                    // observed half-finished, as when a package manager extracts
                    // a folder. A later write to a file raises its own event, so
                    // reconciling plain file writes again would be redundant.
                    let may_be_partial = !matches!(event, VfsEvent::Write(_))
                        || self
                            .vfs
                            .metadata(&path)
                            .map(|metadata| metadata.is_dir())
                            .unwrap_or(false);
                    if may_be_partial {
                        settle_paths.push(path.clone());
                    }
                    paths.push(path);
                }
                Some(Err(err)) => {
                    log::error!("Error resolving filesystem event {:?}: {:?}", event, err);
                }
                None => log::warn!("Unhandled VFS event: {:?}", event),
            }
        }
        paths.sort();
        paths.dedup();
        // A whole-tree resync already covers every path in this batch, but a
        // deferred one does not run until later, and a failed one kept the
        // tree as it was, so those paths still need reconciling now.
        if !(rescan && self.request_resync()) {
            self.apply_patches(paths);
        }
        settle_paths.sort();
        settle_paths.dedup();
        settle_paths
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
                        if key == "Source"
                            && matches!(
                                instance.metadata().middleware,
                                Some(Middleware::DataCsv | Middleware::DataCsvDir)
                            )
                        {
                            // Writing the edited Luau over the spreadsheet
                            // would destroy it.
                            log::warn!(
                                "Cannot write Source of {:?}, it is generated from a data CSV file",
                                id
                            );
                        } else if key == "Source" {
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
                let message = PublishedPatch::new(&tree, applied_patch);
                self.message_queue.push_messages(&[message]);
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

/// Snapshots instance `id` again and applies the changes to the tree.
///
/// Fails when the snapshot could not be taken, and the instance was kept as it
/// was. Returns `None` when there was nothing to update.
fn compute_and_apply_changes(
    tree: &mut RojoTree,
    vfs: &Vfs,
    id: Ref,
) -> Result<Option<AppliedPatchSet>, StaleTree> {
    let Some(metadata) = tree.get_metadata(id) else {
        // Panicking here would stop the processor thread, and every later
        // change with it.
        log::warn!("Instance {:?} was removed before it could be updated", id);
        return Ok(None);
    };

    let instigating_source = match &metadata.instigating_source {
        Some(path) => path,
        None => {
            log::error!(
                "Instance {:?} did not have an instigating source, but was considered for an update.",
                id
            );
            log::error!("This is a bug. Please file an issue!");
            return Ok(None);
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
                        return Err(StaleTree);
                    }
                };
                if snapshot.is_none() && id == tree.get_root_id() {
                    log::warn!(
                        "{} no longer produces an instance. Keeping the last synced project.",
                        path.display()
                    );
                    return Err(StaleTree);
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
                return Err(StaleTree);
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
                return Err(StaleTree);
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
                    return Err(StaleTree);
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
                    return Err(StaleTree);
                }
            }

            let patch_set = compute_patch_set(snapshot, tree, id);
            apply_patch_set(tree, patch_set)
        }
    };

    Ok(Some(applied_patch_set))
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

    /// Steady filesystem activity must not postpone reconciling a folder that
    /// was snapshotted while it was still being written.
    #[test]
    fn settle_pass_runs_despite_continuous_events() {
        use memofs::{InMemoryFs, VfsSnapshot};

        let mut fs = InMemoryFs::new();
        fs.load_snapshot(
            "/root",
            VfsSnapshot::dir([("other.luau", VfsSnapshot::file("return 0"))]),
        )
        .unwrap();
        let vfs = Vfs::new(fs.clone());
        let snapshot = snapshot_from_vfs(&InstanceContext::default(), &vfs, Path::new("/root"))
            .unwrap()
            .unwrap();
        let mut tree = RojoTree::new(InstanceSnapshot::new());
        let patch = compute_patch_set(Some(snapshot), &tree, tree.get_root_id());
        apply_patch_set(&mut tree, patch);
        let tree = Arc::new(Mutex::new(tree));
        let queue = Arc::new(MessageQueue::new());
        let (_mutation_sender, mutation_receiver) = crossbeam_channel::unbounded();
        let _processor = ChangeProcessor::start(
            Arc::clone(&tree),
            Arc::new(vfs),
            Arc::clone(&queue),
            mutation_receiver,
        );

        // The package folder is reported while only its first file exists.
        fs.load_snapshot(
            "/root/pkg",
            VfsSnapshot::dir([("a.luau", VfsSnapshot::file("return 1"))]),
        )
        .unwrap();
        fs.raise_event(VfsEvent::Create(PathBuf::from("/root/pkg")));
        let started = Instant::now();
        while queue.cursor() == 0 {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }

        // The rest of the package lands without an event of its own.
        fs.load_snapshot("/root/pkg/b.luau", VfsSnapshot::file("return 2"))
            .unwrap();
        let late = Path::new("/root/pkg/b.luau");
        while tree.lock().unwrap().get_ids_at_path(late).is_empty() {
            assert!(
                started.elapsed() < MAX_SETTLE_DELAY + Duration::from_secs(3),
                "continuous events postponed the settle pass"
            );
            fs.raise_event(VfsEvent::Write(PathBuf::from("/root/other.luau")));
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn plain_file_writes_are_not_reconciled_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let file = root.join("file.luau");
        fs::write(&file, "return 1").unwrap();
        let task = context(&root);
        fs::write(&file, "return 2").unwrap();
        assert!(task
            .handle_vfs_events(vec![VfsEvent::Write(file.clone())])
            .is_empty());
        assert_eq!(
            task.handle_vfs_events(vec![VfsEvent::Write(root.clone())]),
            vec![root]
        );
    }

    #[test]
    fn published_patch_weight_counts_script_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let task = context(&root);
        let source = "x".repeat(100_000);
        fs::write(root.join("big.luau"), &source).unwrap();
        task.handle_vfs_event(VfsEvent::Create(root.join("big.luau")));

        let (_, messages) = task
            .message_queue
            .subscribe(0)
            .unwrap()
            .try_recv()
            .unwrap()
            .unwrap();
        assert_eq!(messages.len(), 1);
        // Both the message and the applied patch may hold a copy, so the
        // estimate must cover at least one full source.
        assert!(messages[0].weight() >= source.len());
    }

    #[test]
    fn settled_package_paths_recover_children_missing_during_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let package = root.join("_Index");
        fs::create_dir(&package).unwrap();
        let task = context(&root);
        fs::remove_dir(&package).unwrap();
        let paths = task.handle_vfs_events(vec![VfsEvent::Remove(package.clone())]);
        fs::create_dir(&package).unwrap();
        let module = package.join("dependency.luau");
        fs::write(&module, "return 'complete'").unwrap();
        task.apply_patches(paths);
        assert!(!task
            .tree
            .lock()
            .unwrap()
            .get_ids_at_path(&module)
            .is_empty());
    }

    #[test]
    fn batched_parent_and_child_events_update_independent_project_mappings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let src = root.join("src");
        fs::create_dir(&src).unwrap();
        let shared = src.join("shared.luau");
        fs::write(&shared, "return 'before'").unwrap();
        let project = root.join("default.project.json");
        fs::write(&project, r#"{"name":"overlap","tree":{"$className":"Folder","A":{"$path":"src"},"B":{"$path":"src/shared.luau"}}}"#).unwrap();
        let task = context(&project);
        fs::write(&shared, "return 'after'").unwrap();
        task.handle_vfs_events(vec![VfsEvent::Write(src), VfsEvent::Write(shared.clone())]);
        let tree = task.tree.lock().unwrap();
        let ids = tree.get_ids_at_path(&shared);
        assert_eq!(ids.len(), 2);
        for id in ids {
            assert_eq!(
                tree.get_instance(*id).unwrap().properties()[&ustr("Source")],
                Variant::String("return 'after'".into())
            );
        }
    }

    #[test]
    fn package_event_batch_reconciles_the_parent_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let task = context(&root);
        let package = root.join("package.luau");
        fs::write(&package, "return 'installed'").unwrap();
        task.handle_vfs_events(vec![
            VfsEvent::Write(root),
            VfsEvent::Create(package.clone()),
            VfsEvent::Write(package.clone()),
        ]);
        assert_eq!(task.message_queue.cursor(), 1);
        assert!(!task
            .tree
            .lock()
            .unwrap()
            .get_ids_at_path(&package)
            .is_empty());
    }

    #[test]
    fn queued_package_addition_keeps_its_contents_after_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let task = context(&root);
        let package = root.join("package.luau");
        fs::write(&package, "return 'original'").unwrap();
        task.handle_vfs_event(VfsEvent::Write(root.clone()));
        fs::remove_file(&package).unwrap();
        task.handle_vfs_event(VfsEvent::Write(root.clone()));
        fs::write(&package, "return 'replacement'").unwrap();
        task.handle_vfs_event(VfsEvent::Write(root));

        let (_, messages) = task
            .message_queue
            .subscribe(0)
            .unwrap()
            .try_recv()
            .unwrap()
            .unwrap();
        assert_eq!(messages.len(), 3);
        let original = messages[0]
            .message
            .added
            .values()
            .find(|instance| instance.name == "package")
            .unwrap();
        assert_eq!(
            original.properties[&ustr("Source")].as_ref(),
            &Variant::String("return 'original'".into())
        );
        assert!(messages[1].message.removed.contains(&original.id));
        let replacement = messages[2]
            .message
            .added
            .values()
            .find(|instance| instance.name == "package")
            .unwrap();
        assert_ne!(original.id, replacement.id);
        assert_eq!(
            replacement.properties[&ustr("Source")].as_ref(),
            &Variant::String("return 'replacement'".into())
        );
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
    fn broken_sibling_in_a_batch_does_not_drop_a_valid_child_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let child = root.join("a.luau");
        fs::write(&child, "return 'before'").unwrap();
        let task = context(&root);

        fs::write(&child, "return 'after'").unwrap();
        // A new, half-written sibling makes the parent fail to snapshot.
        let broken = root.join("broken.model.json");
        fs::write(&broken, "{").unwrap();
        task.handle_vfs_events(vec![
            VfsEvent::Write(child.clone()),
            VfsEvent::Create(broken.clone()),
        ]);

        let tree = task.tree.lock().unwrap();
        let ids = tree.get_ids_at_path(&child);
        assert_eq!(ids.len(), 1);
        assert_eq!(
            tree.get_instance(ids[0]).unwrap().properties()[&ustr("Source")],
            Variant::String("return 'after'".into())
        );
        assert!(tree.get_ids_at_path(&broken).is_empty());
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
        let mut pending = task.message_queue.subscribe(0).unwrap();
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
    fn removing_a_path_shared_by_optional_nodes_keeps_processing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let shared = root.join("Shared");
        fs::create_dir(&shared).unwrap();
        fs::write(shared.join("module.luau"), "return 1").unwrap();
        let project = write_project(
            &root,
            r#"{"$className":"Folder","A":{"$path":{"optional":"Shared"}},"B":{"$path":{"optional":"Shared"}}}"#,
        );
        let task = context(&project);
        assert_eq!(task.tree.lock().unwrap().get_ids_at_path(&shared).len(), 2);

        // Resyncing the first node resyncs the root, which removes the second
        // node before it is handled.
        fs::remove_dir_all(&shared).unwrap();
        task.handle_vfs_event(VfsEvent::Remove(shared.clone()));
        assert!(task.tree.lock().unwrap().root().children().is_empty());

        fs::create_dir(&shared).unwrap();
        task.handle_vfs_event(VfsEvent::Create(shared.clone()));
        assert_eq!(task.tree.lock().unwrap().get_ids_at_path(&shared).len(), 2);
    }

    #[test]
    fn updating_a_removed_instance_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let task = context(&root);
        let mut tree = task.tree.lock().unwrap();
        assert!(matches!(
            compute_and_apply_changes(&mut tree, &task.vfs, Ref::new()),
            Ok(None)
        ));
    }

    // On macOS, where FSEvents cannot watch a path before it exists, the
    // backend polls for it instead.
    #[test]
    fn creating_a_project_path_whose_parent_was_missing_syncs_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let project = write_project(
            &root,
            r#"{"$className":"Folder","Packages":{"$path":{"optional":"lib/Packages"}}}"#,
        );
        let task = context_with_vfs(Vfs::new_default().unwrap(), &project);
        let receiver = task.vfs.event_receiver();

        // A package manager creates `lib` and then `lib/Packages`, neither of
        // which existed when the project was first snapshotted.
        let added = root.join("lib").join("Packages").join("module.luau");
        fs::create_dir(root.join("lib")).unwrap();
        fs::create_dir(root.join("lib").join("Packages")).unwrap();
        fs::write(&added, "return 1").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while task.tree.lock().unwrap().get_ids_at_path(&added).is_empty() {
            let event = receiver
                .recv_deadline(deadline)
                .expect("the created project path was never synced");
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
    fn paths_batched_with_a_deferred_rescan_are_still_applied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let task = context(&root);
        task.handle_vfs_event(VfsEvent::Rescan);

        // The deferred resync only runs later, so the other events in the
        // batch must not wait for it.
        let added = root.join("added.luau");
        fs::write(&added, "return 1").unwrap();
        task.handle_vfs_events(vec![VfsEvent::Rescan, VfsEvent::Create(added.clone())]);
        assert!(task.pending_resync_deadline().is_some());
        assert!(!task.tree.lock().unwrap().get_ids_at_path(&added).is_empty());
    }

    #[test]
    fn paths_batched_with_a_failed_rescan_are_still_applied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let packages = root.join("Packages");
        let src = root.join("src");
        fs::create_dir(&packages).unwrap();
        fs::create_dir(&src).unwrap();
        let project = write_project(
            &root,
            r#"{"$className":"Folder","Packages":{"$path":"Packages"},"src":{"$path":"src"}}"#,
        );
        let task = context(&project);

        // The resync fails while the required folder is missing, which leaves
        // the tree as it was, so the other events in the batch must still be
        // applied.
        fs::remove_dir_all(&packages).unwrap();
        let added = src.join("added.luau");
        fs::write(&added, "return 1").unwrap();
        task.handle_vfs_events(vec![VfsEvent::Rescan, VfsEvent::Create(added.clone())]);
        assert!(task.pending_resync_deadline().is_some());
        assert!(!task.tree.lock().unwrap().get_ids_at_path(&added).is_empty());
    }

    #[test]
    fn failed_resyncs_are_retried_with_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let packages = root.join("Packages");
        fs::create_dir(&packages).unwrap();
        fs::write(packages.join("old.luau"), "return 1").unwrap();
        let project = write_project(
            &root,
            r#"{"$className":"Folder","Packages":{"$path":"Packages"}}"#,
        );
        let task = context(&project);

        // The watcher drops events during a package reinstall, and the resync
        // runs while the required folder is missing.
        fs::remove_dir_all(&packages).unwrap();
        task.handle_vfs_event(VfsEvent::Rescan);
        let first_retry = task.pending_resync_deadline().unwrap();
        assert!(first_retry <= Instant::now() + RESYNC_INTERVAL);
        task.resync_tree();
        let second_retry = task.pending_resync_deadline().unwrap();
        assert!(second_retry > Instant::now() + RESYNC_INTERVAL);
        assert!(second_retry <= Instant::now() + 2 * RESYNC_INTERVAL);
        for _ in 0..10 {
            task.resync_tree();
        }
        assert_eq!(task.resync_interval(), MAX_RESYNC_RETRY_INTERVAL);

        // Its creation was never reported, so only the retry picks it up.
        fs::create_dir(&packages).unwrap();
        let added = packages.join("new.luau");
        fs::write(&added, "return 2").unwrap();
        task.resync_tree();
        assert!(task.pending_resync_deadline().is_none());
        assert_eq!(task.resync_interval(), RESYNC_INTERVAL);
        let tree = task.tree.lock().unwrap();
        assert!(!tree.get_ids_at_path(&added).is_empty());
        assert!(tree.get_ids_at_path(&packages.join("old.luau")).is_empty());
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
    fn two_way_sync_does_not_write_luau_over_data_csv_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let items = root.join("Items.data.csv");
        fs::write(&items, "id,cost\nsword,100\n").unwrap();
        let levels = root.join("Levels");
        fs::create_dir(&levels).unwrap();
        let levels_init = levels.join("init.data.csv");
        fs::write(&levels_init, "id,music\n1,calm\n").unwrap();
        fs::write(levels.join("Helper.luau"), "return 1").unwrap();
        let task = context(&root);

        for (path, file) in [(&items, &items), (&levels, &levels_init)] {
            let before = fs::read(file).unwrap();
            let id = task.tree.lock().unwrap().get_ids_at_path(path)[0];
            let mut properties = UstrMap::default();
            properties.insert(
                ustr("Source"),
                Some(Variant::String("return 'edited in Studio'".into())),
            );
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
            assert_eq!(fs::read(file).unwrap(), before, "{}", file.display());
        }

        // Nothing else was written next to them, such as an init.luau.
        assert!(!levels.join("init.luau").exists());
        assert_eq!(fs::read_dir(&levels).unwrap().count(), 2);
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
