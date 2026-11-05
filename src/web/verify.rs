//! Compares the scripts a connected Studio holds with the server's tree, for
//! `rojo studio verify`.
//!
//! Studio cannot be trusted to match the files on disk after a crash, a sync
//! that stopped halfway, an undo of a Rojo change or a change the plugin could
//! not apply. The plugin hashes the source of every script it tracks (see
//! `plugin/src/hashSource.lua`) and the server compares those hashes with its
//! own tree, so the full sources only travel when they need fixing.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    path::Path,
};

use rbx_dom_weak::{types::Variant, ustr, UstrMap};
use serde_json::{json, Value};

use super::interface::{InstanceUpdate, SubscribeMessage};
use crate::snapshot::{InstigatingSource, RojoTree};

/// Script classes whose `Source` the plugin can read.
const SCRIPT_CLASSES: [&str; 3] = ["Script", "LocalScript", "ModuleScript"];

/// Lists in a report are cut to this many entries, so the response stays
/// readable and small even when a whole place is out of date.
pub(crate) const MAX_LISTED: usize = 200;

/// How many bytes of source one resync command may carry. Each command must
/// fit in a single poll response (`MAX_POLL_BYTES` in studio.rs) with room
/// for JSON escaping, which can double the size of some sources.
pub(crate) const MAX_RESYNC_BYTES: usize = 2 * 1024 * 1024;

/// 32-bit FNV-1a of `source` with CRLF line endings turned into LF, and the
/// length of that normalized text. Must match `plugin/src/hashSource.lua`.
pub(crate) fn hash_source(source: &str) -> (u32, usize) {
    let normalized: Cow<str> = if source.contains("\r\n") {
        Cow::Owned(source.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(source)
    };
    let mut hash: u32 = 0x811c_9dc5;
    for byte in normalized.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    (hash, normalized.len())
}

#[derive(Debug, Clone)]
pub(crate) struct TreeScript {
    pub id: String,
    pub ref_id: rbx_dom_weak::types::Ref,
    pub path: String,
    pub file: Option<String>,
    pub hash: u32,
    pub length: usize,
}

fn escape_name(name: &str) -> String {
    name.replace('\\', "\\\\").replace('/', "\\/")
}

/// The instance's path in the form the bridge accepts, such as
/// `game/ReplicatedStorage/Shared`.
fn instance_path(tree: &RojoTree, id: rbx_dom_weak::types::Ref) -> String {
    let root = tree.get_root_id();
    let mut names = Vec::new();
    let mut current = id;
    while current != root {
        let Some(instance) = tree.get_instance(current) else {
            break;
        };
        names.push(escape_name(instance.name()));
        current = instance.parent();
    }
    names.push("game".to_owned());
    names.reverse();
    names.join("/")
}

fn source_file(tree: &RojoTree, id: rbx_dom_weak::types::Ref, root_dir: &Path) -> Option<String> {
    let metadata = tree.get_metadata(id)?;
    let path = match &metadata.instigating_source {
        Some(InstigatingSource::Path(path)) => path.as_path(),
        // A script declared in a project file gets its source from `$path`,
        // which is listed among the relevant paths.
        _ => metadata.relevant_paths.first()?.as_path(),
    };
    let relative = path.strip_prefix(root_dir).unwrap_or(path);
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn ref_string(id: rbx_dom_weak::types::Ref) -> String {
    // The protocol's own spelling of a Ref, which is what the plugin reports.
    serde_json::to_value(id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| id.to_string())
}

/// Every script in the tree whose source the server knows.
pub(crate) fn tree_scripts(tree: &RojoTree, root_dir: &Path) -> Vec<TreeScript> {
    let root = tree.get_root_id();
    let mut scripts = Vec::new();
    for instance in tree.descendants(root) {
        if !SCRIPT_CLASSES.contains(&instance.class_name().as_str()) {
            continue;
        }
        let Some(Variant::String(source)) = instance.properties().get(&ustr("Source")) else {
            continue;
        };
        let (hash, length) = hash_source(source);
        let id = instance.id();
        scripts.push(TreeScript {
            id: ref_string(id),
            ref_id: id,
            path: instance_path(tree, id),
            file: source_file(tree, id, root_dir),
            hash,
            length,
        });
    }
    scripts.sort_by(|a, b| a.path.cmp(&b.path));
    scripts
}

#[derive(Debug, Default)]
pub(crate) struct Comparison {
    pub checked: usize,
    pub matching: usize,
    /// Scripts whose source in Studio differs from the server's, with
    /// Studio's length.
    pub different: Vec<(TreeScript, usize)>,
    pub missing: Vec<TreeScript>,
    pub detached: Vec<TreeScript>,
    pub held: HashSet<String>,
}

fn string_list(value: Option<&Value>) -> HashSet<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Compares the plugin's `sources` answer with the server's scripts.
pub(crate) fn compare(scripts: Vec<TreeScript>, studio: &Value) -> anyhow::Result<Comparison> {
    let entries = studio
        .get("scripts")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Studio's answer has no script list"))?;
    let mut in_studio: HashMap<&str, (u32, usize)> = HashMap::with_capacity(entries.len());
    for entry in entries {
        let (Some(id), Some(hash), Some(length)) = (
            entry.get("id").and_then(Value::as_str),
            entry.get("hash").and_then(Value::as_u64),
            entry.get("length").and_then(Value::as_u64),
        ) else {
            anyhow::bail!("Studio's answer has a malformed script entry");
        };
        let hash = u32::try_from(hash)
            .map_err(|_| anyhow::anyhow!("Studio's answer has a hash outside 32 bits"))?;
        in_studio.insert(id, (hash, length as usize));
    }
    let detached = string_list(studio.get("detached"));
    let held = string_list(studio.get("held"));
    let mut comparison = Comparison {
        checked: scripts.len(),
        held,
        ..Comparison::default()
    };
    for script in scripts {
        match in_studio.get(script.id.as_str()) {
            Some(&(hash, length)) if hash == script.hash && length == script.length => {
                comparison.matching += 1;
            }
            Some(&(_, length)) => comparison.different.push((script, length)),
            None if detached.contains(&script.id) => comparison.detached.push(script),
            None => comparison.missing.push(script),
        }
    }
    Ok(comparison)
}

fn describe(script: &TreeScript) -> Value {
    json!({"id": script.id, "path": script.path, "file": script.file, "diskLength": script.length})
}

fn listed<'a>(items: impl ExactSizeIterator<Item = Value> + 'a) -> (Vec<Value>, usize) {
    let total = items.len();
    (items.take(MAX_LISTED).collect(), total)
}

impl Comparison {
    pub(crate) fn in_sync(&self) -> bool {
        self.different.is_empty() && self.missing.is_empty() && self.detached.is_empty()
    }

    pub(crate) fn report(&self) -> Value {
        let (different, different_count) = listed(self.different.iter().map(|(script, studio)| {
            let mut value = describe(script);
            value["studioLength"] = json!(studio);
            value["held"] = json!(self.held.contains(&script.id));
            value
        }));
        let (missing, missing_count) = listed(self.missing.iter().map(describe));
        let (detached, detached_count) = listed(self.detached.iter().map(describe));
        json!({
            "inSync": self.in_sync(),
            "checked": self.checked,
            "matching": self.matching,
            "differentCount": different_count,
            "different": different,
            "missingCount": missing_count,
            "missing": missing,
            "detachedCount": detached_count,
            "detached": detached,
            "heldCount": self.held.len(),
            "truncated": different_count > MAX_LISTED || missing_count > MAX_LISTED || detached_count > MAX_LISTED,
        })
    }

    /// The scripts `--fix` sets back to their files' contents. Held wipes stay
    /// out unless asked for, because fixing them empties the script in Studio.
    pub(crate) fn fixable(&self, include_held: bool) -> Vec<rbx_dom_weak::types::Ref> {
        self.different
            .iter()
            .filter(|(script, _)| include_held || !self.held.contains(&script.id))
            .map(|(script, _)| script.ref_id)
            .collect()
    }
}

/// Builds sync messages that set each script's `Source` to the server's,
/// split so that each carries at most [`MAX_RESYNC_BYTES`] of source. A
/// script larger than that gets a message of its own.
pub(crate) fn resync_messages(tree: &RojoTree, ids: &[rbx_dom_weak::types::Ref]) -> Vec<Value> {
    let mut messages = Vec::new();
    let mut updated = Vec::new();
    let mut bytes = 0;
    for &id in ids {
        let Some(instance) = tree.get_instance(id) else {
            continue;
        };
        let Some(source @ Variant::String(text)) = instance.properties().get(&ustr("Source"))
        else {
            continue;
        };
        if !updated.is_empty() && bytes + text.len() > MAX_RESYNC_BYTES {
            messages.push(std::mem::take(&mut updated));
            bytes = 0;
        }
        bytes += text.len();
        let mut changed_properties = UstrMap::default();
        changed_properties.insert(ustr("Source"), Some(source.clone()));
        updated.push(InstanceUpdate {
            id,
            changed_name: None,
            changed_class_name: None,
            changed_properties,
            changed_metadata: None,
        });
    }
    if !updated.is_empty() {
        messages.push(updated);
    }
    messages
        .into_iter()
        .map(|updated| {
            serde_json::to_value(SubscribeMessage {
                removed: Vec::new(),
                added: HashMap::new(),
                updated,
            })
            .expect("sync messages always serialize")
        })
        .collect()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::snapshot::InstanceSnapshot;

    #[test]
    fn hashes_match_the_plugin() {
        // Same vectors as plugin/src/hashSource.spec.lua.
        assert_eq!(hash_source(""), (0x811c_9dc5, 0));
        assert_eq!(hash_source("a"), (0xe40c_292c, 1));
        assert_eq!(hash_source("foobar"), (0xbf9c_f968, 6));
        assert_eq!(hash_source("print(\"Hello, Rojo!\")\n"), (0x8d5c_0b3a, 22));
    }

    #[test]
    fn line_endings_do_not_change_the_hash() {
        assert_eq!(
            hash_source("local a = 1\r\nreturn a\r\n"),
            hash_source("local a = 1\nreturn a\n")
        );
    }

    fn script(name: &str, source: &str) -> InstanceSnapshot {
        InstanceSnapshot::new()
            .name(name)
            .class_name("ModuleScript")
            .property(ustr("Source"), source)
    }

    fn tree() -> RojoTree {
        RojoTree::new(
            InstanceSnapshot::new()
                .name("game")
                .class_name("DataModel")
                .children(vec![InstanceSnapshot::new()
                    .name("ReplicatedStorage")
                    .class_name("ReplicatedStorage")
                    .children(vec![
                        script("Same", "return 1\n"),
                        script("Changed", "return 2\n"),
                        script("Gone", "return 3\n"),
                        script("Detached", "return 4\n"),
                        script("Odd/Name", "return 5\n"),
                    ])]),
        )
    }

    fn studio_answer(scripts: &[TreeScript]) -> Value {
        let by_name = |name: &str| scripts.iter().find(|s| s.path.ends_with(name)).unwrap();
        let same = by_name("/Same");
        let changed = by_name("/Changed");
        let odd = by_name("/Odd\\/Name");
        let (other_hash, other_len) = hash_source("return 'edited in Studio'\n");
        json!({
            "scripts": [
                {"id": same.id, "hash": same.hash, "length": same.length},
                {"id": changed.id, "hash": other_hash, "length": other_len},
                {"id": odd.id, "hash": odd.hash, "length": odd.length},
            ],
            "detached": [by_name("/Detached").id],
            "held": [],
        })
    }

    #[test]
    fn reports_each_kind_of_difference() {
        let tree = tree();
        let scripts = tree_scripts(&tree, Path::new("/"));
        assert_eq!(scripts.len(), 5);
        let answer = studio_answer(&scripts);
        let comparison = compare(scripts, &answer).unwrap();
        assert!(!comparison.in_sync());
        assert_eq!(comparison.checked, 5);
        assert_eq!(comparison.matching, 2);
        let report = comparison.report();
        assert_eq!(report["differentCount"], 1);
        assert_eq!(
            report["different"][0]["path"],
            "game/ReplicatedStorage/Changed"
        );
        assert_eq!(report["missing"][0]["path"], "game/ReplicatedStorage/Gone");
        assert_eq!(
            report["detached"][0]["path"],
            "game/ReplicatedStorage/Detached"
        );
        assert_eq!(report["truncated"], false);
    }

    #[test]
    fn a_matching_place_is_in_sync() {
        let tree = tree();
        let scripts = tree_scripts(&tree, Path::new("/"));
        let entries: Vec<Value> = scripts
            .iter()
            .map(|s| json!({"id": s.id, "hash": s.hash, "length": s.length}))
            .collect();
        let comparison = compare(scripts, &json!({"scripts": entries})).unwrap();
        assert!(comparison.in_sync());
        assert_eq!(comparison.matching, 5);
    }

    #[test]
    fn held_wipes_are_only_fixed_when_asked() {
        let tree = tree();
        let scripts = tree_scripts(&tree, Path::new("/"));
        let mut answer = studio_answer(&scripts);
        let changed = scripts
            .iter()
            .find(|s| s.path.ends_with("/Changed"))
            .unwrap();
        answer["held"] = json!([changed.id]);
        let comparison = compare(scripts, &answer).unwrap();
        assert!(comparison.fixable(false).is_empty());
        assert_eq!(comparison.fixable(true).len(), 1);
        assert_eq!(comparison.report()["different"][0]["held"], true);
    }

    #[test]
    fn malformed_answers_are_errors() {
        let tree = tree();
        let scripts = tree_scripts(&tree, Path::new("/"));
        assert!(compare(scripts.clone(), &json!({})).is_err());
        assert!(compare(scripts.clone(), &json!({"scripts": [{"id": "x"}]})).is_err());
        assert!(compare(
            scripts,
            &json!({"scripts": [{"id": "x", "hash": 4_294_967_296_u64, "length": 1}]})
        )
        .is_err());
    }

    #[test]
    fn resync_messages_carry_the_server_source_and_split_large_sets() {
        let big = "x".repeat(MAX_RESYNC_BYTES / 2 + 1);
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .name("game")
                .class_name("DataModel")
                .children(vec![
                    script("A", &big),
                    script("B", &big),
                    script("C", "return 1"),
                ]),
        );
        let ids: Vec<_> = tree_scripts(&tree, Path::new("/"))
            .into_iter()
            .map(|s| s.ref_id)
            .collect();
        let messages = resync_messages(&tree, &ids);
        assert_eq!(messages.len(), 2);
        let first = &messages[0]["updated"][0];
        assert!(first["changedProperties"]["Source"]["String"].is_string());
        assert_eq!(messages[0]["added"], json!({}));
        assert_eq!(messages[0]["removed"], json!([]));
    }
}
