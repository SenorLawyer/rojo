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
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeMessage<'a> {
    pub removed: Vec<Ref>,
    pub added: HashMap<Ref, Instance<'a>>,
    pub updated: Vec<InstanceUpdate>,
}

impl<'a> SubscribeMessage<'a> {
    /// Converts a single applied patch into a message using the current tree.
    #[cfg(test)]
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
    /// instance is gone; the plugin ignores updates and removals for instances
    /// it does not know about.
    pub(crate) fn from_patch_updates(
        tree: &'a RojoTree,
        patches: impl IntoIterator<Item = AppliedPatchSet>,
    ) -> Vec<Self> {
        // Additions are serialized from the current tree including all of
        // their current descendants, so a descendant that was itself added by a
        // later patch in this batch has already been sent and can be skipped.
        let mut sent = HashSet::new();

        patches
            .into_iter()
            .map(|patch| Self::from_patch_update_inner(tree, patch, &mut sent))
            .collect()
    }

    fn from_patch_update_inner(
        tree: &'a RojoTree,
        patch: AppliedPatchSet,
        sent: &mut HashSet<Ref>,
    ) -> Self {
        let removed = patch.removed;

        let mut added = HashMap::new();
        for id in patch.added {
            if sent.contains(&id) {
                continue;
            }

            let Some(instance) = tree.get_instance(id) else {
                log::debug!(
                    "Skipping addition of instance {id} because it was removed by a later patch"
                );
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

        let updated = patch
            .updated
            .into_iter()
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

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
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
    /// the tree cannot be serialized; it is skipped because its removal is
    /// delivered by a later message.
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
        assert_eq!(messages[1].removed, vec![gone]);
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
