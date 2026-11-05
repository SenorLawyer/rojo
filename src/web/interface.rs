//! Defines all the structs needed to interact with the Rojo Serve API. This is
//! useful for tests to be able to use the same data structures as the
//! implementation.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use rbx_dom_weak::{
    types::{Ref, Variant, VariantType},
    Ustr, UstrMap,
};
use serde::{Deserialize, Serialize};
use strum::Display;

use crate::{
    session_id::SessionId,
    snapshot::{
        AppliedPatchSet, InstanceMetadata as RojoInstanceMetadata, InstanceWithMeta, RojoTree,
    },
};

/// Server version to report over the API, not exposed outside this crate.
pub(crate) const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Current protocol version, which is required to match.
pub const PROTOCOL_VERSION: u64 = 5;

/// Message returned by Rojo API when a change has occurred.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeMessage<'a> {
    pub removed: Vec<Ref>,
    pub added: HashMap<Ref, Instance<'a>>,
    pub updated: Vec<InstanceUpdate>,
}

impl<'a> SubscribeMessage<'a> {
    pub(crate) fn into_owned(self) -> SubscribeMessage<'static> {
        SubscribeMessage {
            removed: self.removed,
            added: self
                .added
                .into_iter()
                .map(|(id, instance)| {
                    (
                        id,
                        Instance {
                            id: instance.id,
                            parent: instance.parent,
                            name: Cow::Owned(instance.name.into_owned()),
                            class_name: instance.class_name,
                            properties: instance
                                .properties
                                .into_iter()
                                .map(|(name, value)| (name, Cow::Owned(value.into_owned())))
                                .collect(),
                            children: Cow::Owned(instance.children.into_owned()),
                            metadata: instance.metadata,
                        },
                    )
                })
                .collect(),
            updated: self.updated,
        }
    }

    /// Estimates the memory this message keeps alive. Only the parts that can
    /// grow large, such as script sources, are measured precisely.
    pub(crate) fn approximate_size(&self) -> usize {
        let removed = self.removed.len() * std::mem::size_of::<Ref>();
        let added: usize = self
            .added
            .values()
            .map(|instance| {
                std::mem::size_of_val(instance)
                    + instance.name.len()
                    + instance.children.len() * std::mem::size_of::<Ref>()
                    + instance
                        .properties
                        .values()
                        .map(|value| approximate_variant_size(value))
                        .sum::<usize>()
            })
            .sum();
        let updated: usize = self
            .updated
            .iter()
            .map(|update| {
                std::mem::size_of_val(update)
                    + update.changed_name.as_ref().map_or(0, String::len)
                    + update
                        .changed_properties
                        .values()
                        .map(|value| value.as_ref().map_or(0, approximate_variant_size))
                        .sum::<usize>()
            })
            .sum();
        std::mem::size_of_val(self) + removed + added + updated
    }

    /// Whether this message has no changes for the plugin to apply.
    pub(crate) fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty() && self.updated.is_empty()
    }

    /// Converts a single applied patch into a message using the current tree.
    ///
    /// The change processor calls this right after applying the patch while
    /// still holding the tree lock, so the tree normally matches the patch
    /// exactly. It still never fails: see [`Self::from_patch_updates`].
    pub(crate) fn from_patch_update(tree: &'a RojoTree, patch: AppliedPatchSet) -> Self {
        Self::from_patch_updates(tree, std::iter::once(patch))
            .pop()
            .unwrap()
    }

    /// Converts a batch of applied patches into messages using the current
    /// tree.
    ///
    /// The tree is only read once every patch in the batch has already been
    /// applied, so it can be ahead of any individual patch. Instances added by
    /// one patch may have been removed by a later one (a save followed by a
    /// delete, or a branch checkout), and those removals are always delivered
    /// later in the same or a subsequent batch. Updates carry their own values
    /// and never need the tree, so they are forwarded as-is even if the
    /// instance is gone. The plugin skips removals of instances it does not
    /// know about, but reports updates to them as unapplied, which can trigger
    /// its sync fallback for those IDs; `/api/serialize` then leaves out the
    /// IDs that no longer exist.
    ///
    /// The exception is an instance whose addition was skipped in this batch:
    /// the client never hears of it, so updates and removals of it are
    /// dropped too instead of making the plugin fall back for nothing.
    /// Instances the client received earlier are not affected.
    pub(crate) fn from_patch_updates(
        tree: &'a RojoTree,
        patches: impl IntoIterator<Item = AppliedPatchSet>,
    ) -> Vec<Self> {
        // Additions are serialized from the current tree including all of
        // their current descendants, so a descendant that was itself added by a
        // later patch in this batch has already been sent and can be skipped.
        let mut sent = HashSet::new();
        let mut skipped = HashSet::new();

        patches
            .into_iter()
            .map(|patch| Self::from_patch_update_inner(tree, patch, &mut sent, &mut skipped))
            .collect()
    }

    fn from_patch_update_inner(
        tree: &'a RojoTree,
        patch: AppliedPatchSet,
        sent: &mut HashSet<Ref>,
        skipped: &mut HashSet<Ref>,
    ) -> Self {
        let mut added = HashMap::new();
        for id in patch.added {
            if sent.contains(&id) {
                continue;
            }

            let Some(instance) = tree.get_instance(id) else {
                log::debug!(
                    "Skipping addition of instance {id} because it was removed by a later patch"
                );
                // Every descendant of a skipped addition is listed in the
                // patch too, and is gone as well, so each ends up here.
                skipped.insert(id);
                continue;
            };

            sent.insert(id);
            added.insert(id, Instance::from_rojo_instance(instance));

            for instance in tree.descendants(id) {
                if sent.insert(instance.id()) {
                    added.insert(instance.id(), Instance::from_rojo_instance(instance));
                }
            }
        }

        // An addition is always applied before any update or removal of the
        // same instance, so `skipped` is complete for those by now.
        let removed = patch
            .removed
            .into_iter()
            .filter(|id| !skipped.contains(id))
            .collect();

        let updated = patch
            .updated
            .into_iter()
            .filter(|update| !skipped.contains(&update.id))
            .map(|update| {
                let changed_metadata =
                    update
                        .changed_ignore_unknown_instances
                        .map(|ignore_unknown_instances| InstanceMetadata {
                            ignore_unknown_instances,
                        });

                let changed_properties: UstrMap<Option<Variant>> = update
                    .changed_properties
                    .into_iter()
                    .filter(|(_key, value)| property_filter(value.as_ref()))
                    .collect();

                InstanceUpdate {
                    id: update.id,
                    changed_name: update.changed_name,
                    changed_class_name: update.changed_class_name,
                    changed_properties,
                    changed_metadata,
                }
            })
            .filter(|update| {
                update.changed_name.is_some()
                    || update.changed_class_name.is_some()
                    || !update.changed_properties.is_empty()
                    || update.changed_metadata.is_some()
            })
            .collect();

        Self {
            removed,
            added,
            updated,
        }
    }
}

/// Estimates the memory a property value keeps alive, counting the contents
/// of variable-length values.
pub(crate) fn approximate_variant_size(value: &Variant) -> usize {
    let contents = match value {
        Variant::String(value) => value.len(),
        Variant::BinaryString(value) => AsRef::<[u8]>::as_ref(value).len(),
        Variant::SharedString(value) => value.data().len(),
        Variant::Tags(tags) => tags.iter().map(str::len).sum(),
        Variant::Attributes(attributes) => attributes
            .iter()
            .map(|(name, value)| name.len() + approximate_variant_size(value))
            .sum(),
        _ => 0,
    };
    std::mem::size_of::<Variant>() + contents
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceUpdate {
    pub id: Ref,
    pub changed_name: Option<String>,
    pub changed_class_name: Option<Ustr>,

    // TODO: Transform from UstrMap<String, Option<_>> to something else, since
    // null will get lost when decoding from JSON in some languages.
    #[serde(default)]
    pub changed_properties: UstrMap<Option<Variant>>,
    pub changed_metadata: Option<InstanceMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceMetadata {
    pub ignore_unknown_instances: bool,
}

impl InstanceMetadata {
    pub(crate) fn from_rojo_metadata(meta: &RojoInstanceMetadata) -> Self {
        Self {
            ignore_unknown_instances: meta.ignore_unknown_instances,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Instance<'a> {
    pub id: Ref,
    pub parent: Ref,
    pub name: Cow<'a, str>,
    pub class_name: Ustr,
    pub properties: UstrMap<Cow<'a, Variant>>,
    pub children: Cow<'a, [Ref]>,
    pub metadata: Option<InstanceMetadata>,
}

impl Instance<'_> {
    pub(crate) fn from_rojo_instance(source: InstanceWithMeta<'_>) -> Instance<'_> {
        let properties = source
            .properties()
            .iter()
            .filter(|(_key, value)| property_filter(Some(value)))
            .map(|(key, value)| (*key, Cow::Borrowed(value)))
            .collect();

        Instance {
            id: source.id(),
            parent: source.parent(),
            name: Cow::Borrowed(source.name()),
            class_name: source.class_name(),
            properties,
            children: Cow::Borrowed(source.children()),
            metadata: Some(InstanceMetadata::from_rojo_metadata(source.metadata())),
        }
    }
}

fn property_filter(value: Option<&Variant>) -> bool {
    let ty = value.map(|value| value.ty());

    // Lua can't do anything with SharedString values. They also can't be
    // serialized directly by Serde!
    ty != Some(VariantType::SharedString)
}

/// Response body from /api/rojo
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfoResponse {
    pub session_id: SessionId,
    pub server_version: String,
    pub protocol_version: u64,
    pub project_name: String,
    pub expected_place_ids: Option<HashSet<u64>>,
    pub unexpected_place_ids: Option<HashSet<u64>>,
    pub game_id: Option<u64>,
    pub place_id: Option<u64>,
    pub root_instance_id: Ref,
}

/// Response body from /api/read/{id}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResponse<'a> {
    pub session_id: SessionId,
    pub message_cursor: u32,
    pub instances: HashMap<Ref, Instance<'a>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRequest {
    pub session_id: SessionId,
    pub removed: Vec<Ref>,

    #[serde(default)]
    pub added: HashMap<Ref, ()>,
    pub updated: Vec<InstanceUpdate>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResponse {
    pub session_id: SessionId,
}

/// Packet type enum for different websocket message types
#[derive(Debug, Serialize, Deserialize, Display, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum SocketPacketType {
    Messages,
    // TODO: Can we cleanly use the socket for all communication?
    // Serialize,
    // RefPatch,
}

/// Body content for messages packet type
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagesPacket<'a> {
    pub message_cursor: u32,
    pub messages: Vec<SubscribeMessage<'a>>,
}

/// Body content for different packet types
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SocketPacketBody<'a> {
    Messages(MessagesPacket<'a>),
    // TODO: Can we cleanly use the socket for all communication?
    // Serialize(SerializePacket),
    // RefPatch(RefPatchPacket<'a>),
}

/// Message content from /api/socket
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketPacket<'a> {
    pub session_id: SessionId,
    pub packet_type: SocketPacketType,
    pub body: SocketPacketBody<'a>,
}

/// Response body from /api/open/{id}
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResponse {
    pub session_id: SessionId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializeRequest {
    pub session_id: SessionId,
    pub ids: Vec<Ref>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializeResponse {
    pub session_id: SessionId,
    #[serde(with = "serde_bytes")]
    pub model_contents: Vec<u8>,
    /// Requested IDs that are no longer in the tree and so are not part of
    /// `model_contents`.
    #[serde(default)]
    pub missing_ids: Vec<Ref>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefPatchRequest {
    pub session_id: SessionId,
    pub ids: HashSet<Ref>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefPatchResponse<'a> {
    pub session_id: SessionId,
    pub patch: SubscribeMessage<'a>,
}

/// General response type returned from all Rojo routes
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    kind: ErrorResponseKind,
    details: String,
}

impl ErrorResponse {
    pub fn not_found<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::NotFound,
            details: details.into(),
        }
    }

    pub fn bad_request<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::BadRequest,
            details: details.into(),
        }
    }

    pub fn forbidden<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::Forbidden,
            details: details.into(),
        }
    }

    pub fn internal_error<S: Into<String>>(details: S) -> Self {
        Self {
            kind: ErrorResponseKind::InternalError,
            details: details.into(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ErrorResponseKind {
    NotFound,
    BadRequest,
    Forbidden,
    InternalError,
}

#[cfg(test)]
mod tests {
    use super::*;

    use rbx_dom_weak::ustr;

    use crate::snapshot::{AppliedPatchUpdate, InstanceSnapshot};

    fn empty_tree() -> RojoTree {
        RojoTree::new(InstanceSnapshot::new().name("ROOT").class_name("ROOT"))
    }

    fn update_for(mut update: AppliedPatchUpdate) -> Vec<InstanceUpdate> {
        let tree = empty_tree();
        update.id = tree.get_root_id();
        let patch = AppliedPatchSet {
            removed: Vec::new(),
            added: Vec::new(),
            updated: vec![update],
        };

        SubscribeMessage::from_patch_update(&tree, patch).updated
    }

    fn folder(name: &str) -> InstanceSnapshot {
        InstanceSnapshot::new().name(name).class_name("Folder")
    }

    /// A save followed by a delete produces two patches that the websocket
    /// task can receive in one batch, after both have been applied to the
    /// tree. The update must still be forwarded instead of failing the batch
    /// and dropping the plugin's connection.
    #[test]
    fn update_then_remove_in_one_batch_is_forwarded() {
        let mut tree = empty_tree();
        let child = tree.insert_instance(tree.get_root_id(), folder("Child"));
        tree.remove(child);

        let mut update = AppliedPatchUpdate::new(child);
        update.changed_name = Some("Renamed".to_owned());

        let messages = SubscribeMessage::from_patch_updates(
            &tree,
            [
                AppliedPatchSet {
                    removed: Vec::new(),
                    added: Vec::new(),
                    updated: vec![update],
                },
                AppliedPatchSet {
                    removed: vec![child],
                    added: Vec::new(),
                    updated: Vec::new(),
                },
            ],
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].updated.len(), 1);
        assert_eq!(messages[0].updated[0].id, child);
        assert_eq!(messages[1].removed, vec![child]);
    }

    /// An instance created and then deleted before the websocket task reads
    /// the tree cannot be serialized, so it is skipped, and so is its removal
    /// since the client never heard of it.
    #[test]
    fn add_then_remove_in_one_batch_skips_addition() {
        let mut tree = empty_tree();
        let kept = tree.insert_instance(tree.get_root_id(), folder("Kept"));
        let gone = tree.insert_instance(tree.get_root_id(), folder("Gone"));
        tree.remove(gone);

        let messages = SubscribeMessage::from_patch_updates(
            &tree,
            [
                AppliedPatchSet {
                    removed: Vec::new(),
                    added: vec![gone, kept],
                    updated: Vec::new(),
                },
                AppliedPatchSet {
                    removed: vec![gone],
                    added: Vec::new(),
                    updated: Vec::new(),
                },
            ],
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].added.len(), 1);
        assert!(messages[0].added.contains_key(&kept));
        assert!(messages[1].is_empty());
    }

    /// Updates and removals of an instance whose addition was skipped would
    /// only make the plugin report them as unapplied and fall back, so they
    /// are dropped. Changes to instances the client already has still go out.
    #[test]
    fn add_update_remove_in_one_batch_sends_nothing_for_instance() {
        let mut tree = empty_tree();
        let known = tree.insert_instance(tree.get_root_id(), folder("Known"));
        let gone = tree.insert_instance(tree.get_root_id(), folder("Gone"));
        let gone_child = tree.insert_instance(gone, folder("GoneChild"));
        tree.remove(gone);
        tree.remove(known);

        let rename = |id| {
            let mut update = AppliedPatchUpdate::new(id);
            update.changed_name = Some("Renamed".to_owned());
            update
        };

        let messages = SubscribeMessage::from_patch_updates(
            &tree,
            [
                AppliedPatchSet {
                    removed: Vec::new(),
                    added: vec![gone, gone_child],
                    updated: Vec::new(),
                },
                AppliedPatchSet {
                    removed: Vec::new(),
                    added: Vec::new(),
                    updated: vec![rename(gone), rename(gone_child), rename(known)],
                },
                AppliedPatchSet {
                    removed: vec![gone, known],
                    added: Vec::new(),
                    updated: Vec::new(),
                },
            ],
        );

        assert_eq!(messages.len(), 3);
        assert!(messages[0].is_empty());
        // `known` was sent to the client by an earlier batch, so its update
        // and removal are forwarded even though it is gone from the tree.
        assert_eq!(messages[1].updated.len(), 1);
        assert_eq!(messages[1].updated[0].id, known);
        assert_eq!(messages[2].removed, vec![known]);
    }

    /// Additions are serialized with their current descendants, so a
    /// descendant added by a later patch in the same batch is only sent once.
    #[test]
    fn descendant_added_later_in_batch_is_sent_once() {
        let mut tree = empty_tree();
        let parent = tree.insert_instance(tree.get_root_id(), folder("Parent"));
        let child = tree.insert_instance(parent, folder("Child"));

        let messages = SubscribeMessage::from_patch_updates(
            &tree,
            [
                AppliedPatchSet {
                    removed: Vec::new(),
                    added: vec![parent],
                    updated: Vec::new(),
                },
                AppliedPatchSet {
                    removed: Vec::new(),
                    added: vec![child],
                    updated: Vec::new(),
                },
            ],
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].added.len(), 2);
        assert!(messages[0].added.contains_key(&parent));
        assert!(messages[0].added.contains_key(&child));
        assert!(messages[1].added.is_empty());
    }

    #[test]
    fn name_only_update_is_kept() {
        let mut update = AppliedPatchUpdate::new(Ref::none());
        update.changed_name = Some("Renamed".to_owned());

        let updated = update_for(update);
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].changed_name.as_deref(), Some("Renamed"));
    }

    #[test]
    fn class_name_only_update_is_kept() {
        let mut update = AppliedPatchUpdate::new(Ref::none());
        update.changed_class_name = Some(ustr("Folder"));

        let updated = update_for(update);
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].changed_class_name, Some(ustr("Folder")));
    }

    #[test]
    fn ignore_unknown_instances_change_is_reported() {
        let mut update = AppliedPatchUpdate::new(Ref::none());
        update.changed_metadata = Some(RojoInstanceMetadata::new().ignore_unknown_instances(true));
        update.changed_ignore_unknown_instances = Some(true);

        let updated = update_for(update);
        assert_eq!(updated.len(), 1);
        assert_eq!(
            updated[0]
                .changed_metadata
                .as_ref()
                .map(|metadata| metadata.ignore_unknown_instances),
            Some(true)
        );
    }

    #[test]
    fn internal_metadata_only_update_is_dropped() {
        let mut update = AppliedPatchUpdate::new(Ref::none());
        update.changed_metadata = Some(RojoInstanceMetadata::new());

        assert!(update_for(update).is_empty());
    }
}
