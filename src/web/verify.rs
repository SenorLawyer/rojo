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
    path::{Path, PathBuf},
};

use rbx_dom_weak::{types::Variant, ustr, UstrMap};
use serde_json::{json, Value};

use super::interface::{InstanceUpdate, SubscribeMessage};
use crate::snapshot::{InstigatingSource, RojoTree};

/// Script classes whose `Source` the plugin can read.
const SCRIPT_CLASSES: [&str; 3] = ["Script", "LocalScript", "ModuleScript"];

/// Lists in a report are cut to this many entries by default, so the response
/// stays readable and small even when a whole place is out of date.
pub(crate) const DEFAULT_LISTED: usize = 200;
/// The most entries a caller may ask for per list. A few hundred bytes each,
/// so three full lists stay well under the bridge's 8 MiB response limit.
pub(crate) const MAX_LISTED: usize = 10_000;

/// How many bytes of source one resync command may carry. Each command must
/// fit in a single poll response (`MAX_POLL_BYTES` in studio.rs) with room
/// for JSON escaping, which can double the size of some sources.
pub(crate) const MAX_RESYNC_BYTES: usize = 2 * 1024 * 1024;

/// How many bytes of Studio source one backup taken before `--fix` may hold.
/// The plugin refuses a model over 5 MiB, and a script's copy is a little
/// larger than its source, so this leaves room for that and for its
/// properties.
pub(crate) const MAX_BACKUP_SOURCE_BYTES: usize = 4 * 1024 * 1024;
/// How many scripts one backup before `--fix` may hold, well under the
/// 10,000 instances the plugin exports and a backup can restore.
pub(crate) const MAX_BACKUP_SCRIPTS: usize = 2_000;

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

/// 32-bit djb2 (xor variant) of `source` with CRLF turned into LF, a second
/// hash independent of [`hash_source`], so that two versions of a script
/// match only when both hashes and the length do. Must match
/// `plugin/src/hashSource.lua`. Neither hash resists sources made to collide on
/// purpose; `verify --fix` also checks the exact source the plugin saw.
pub(crate) fn second_hash(source: &str) -> u32 {
    let normalized: Cow<str> = if source.contains("\r\n") {
        Cow::Owned(source.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(source)
    };
    let mut hash: u32 = 5381;
    for byte in normalized.bytes() {
        hash = hash.wrapping_mul(33) ^ u32::from(byte);
    }
    hash
}

#[derive(Debug, Clone)]
pub(crate) struct TreeScript {
    pub id: String,
    pub ref_id: rbx_dom_weak::types::Ref,
    pub path: String,
    pub file: Option<String>,
    pub hash: u32,
    pub hash2: u32,
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

fn is_script_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("lua" | "luau")
    ) && path.is_file()
}

/// The file a script's source comes from. A script inside a model file
/// (`.model.json`, `.rbxm`, `.rbxmx`) has no file of its own, so the model
/// file of its nearest ancestor that has one is reported instead.
fn source_file(tree: &RojoTree, id: rbx_dom_weak::types::Ref, root_dir: &Path) -> Option<String> {
    let root = tree.get_root_id();
    let mut current = id;
    loop {
        if let Some(file) = own_file(tree, current, root_dir) {
            return Some(file);
        }
        if current == root {
            return None;
        }
        current = tree.get_instance(current)?.parent();
    }
}

fn own_file(tree: &RojoTree, id: rbx_dom_weak::types::Ref, root_dir: &Path) -> Option<String> {
    let metadata = tree.get_metadata(id)?;
    let instigating = match &metadata.instigating_source {
        Some(InstigatingSource::Path(path)) => Some(path.as_path()),
        _ => None,
    };
    // A script made from a directory's init file has the directory as its
    // instigating source, and a script declared in a project file gets its
    // source from `$path`. Either way the file itself is among the relevant
    // paths, next to candidates such as `init.lua` that do not exist.
    let path = instigating
        .filter(|path| is_script_file(path))
        .or_else(|| {
            metadata
                .relevant_paths
                .iter()
                .map(PathBuf::as_path)
                .find(|path| is_script_file(path))
        })
        .or_else(|| instigating.filter(|path| path.is_file()))
        .or_else(|| {
            metadata
                .relevant_paths
                .iter()
                .map(PathBuf::as_path)
                .find(|path| path.is_file())
        })?;
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
        let hash2 = second_hash(source);
        let id = instance.id();
        scripts.push(TreeScript {
            id: ref_string(id),
            ref_id: id,
            path: instance_path(tree, id),
            file: source_file(tree, id, root_dir),
            hash,
            hash2,
            length,
        });
    }
    scripts.sort_by(|a, b| a.path.cmp(&b.path));
    scripts
}

/// A script whose source in Studio differs from the server's.
#[derive(Debug, Clone)]
pub(crate) struct Different {
    pub script: TreeScript,
    /// Studio's hash and length when it was checked. A fix only applies while
    /// Studio still has this source, so it cannot undo a newer change.
    pub studio_hash: u32,
    pub studio_length: usize,
}

#[derive(Debug, Default)]
pub(crate) struct Comparison {
    pub checked: usize,
    pub matching: usize,
    pub different: Vec<Different>,
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
    let mut in_studio: HashMap<&str, (u32, usize, Option<u32>)> =
        HashMap::with_capacity(entries.len());
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
        // Plugins from before the second hash leave it out.
        let hash2 = match entry.get("hash2") {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Studio's answer has a malformed second hash")
                    })?,
            ),
        };
        in_studio.insert(id, (hash, length as usize, hash2));
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
            Some(&(hash, length, hash2))
                if hash == script.hash
                    && length == script.length
                    && hash2.is_none_or(|hash2| hash2 == script.hash2) =>
            {
                comparison.matching += 1;
            }
            Some(&(hash, length, _)) => comparison.different.push(Different {
                script,
                studio_hash: hash,
                studio_length: length,
            }),
            None if detached.contains(&script.id) => comparison.detached.push(script),
            None => comparison.missing.push(script),
        }
    }
    Ok(comparison)
}

/// Paths longer than this are shortened in reports. Instances inside model
/// files can have very long paths, and a few hundred of them must still fit
/// the caller's output limit.
const MAX_REPORTED_PATH: usize = 1024;

fn shorten(text: &str) -> String {
    if text.len() <= MAX_REPORTED_PATH {
        return text.to_owned();
    }
    let mut cut = MAX_REPORTED_PATH;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &text[..cut])
}

fn describe(script: &TreeScript) -> Value {
    json!({
        "id": script.id,
        "path": shorten(&script.path),
        "file": script.file.as_deref().map(shorten),
        "diskLength": script.length,
    })
}

/// The default byte budget for listed scripts, well under the CLI's 8 MiB
/// response limit.
pub(crate) const DEFAULT_LIST_BYTES: usize = 4 * 1024 * 1024;

/// Takes up to `limit` items while their serialized size fits in `budget`,
/// which they then use up. Returns them and how many there were in all.
fn listed<'a>(
    items: impl ExactSizeIterator<Item = Value> + 'a,
    limit: usize,
    budget: &mut usize,
) -> (Vec<Value>, usize) {
    let total = items.len();
    let mut taken = Vec::new();
    for item in items.take(limit) {
        let size = item.to_string().len() + 1;
        if size > *budget {
            break;
        }
        *budget -= size;
        taken.push(item);
    }
    (taken, total)
}

impl Comparison {
    pub(crate) fn in_sync(&self) -> bool {
        self.different.is_empty() && self.missing.is_empty() && self.detached.is_empty()
    }

    /// The report, listing at most `limit` scripts of each kind, in at most
    /// `max_bytes` of JSON for the lists together. The counts cover every
    /// script either way.
    pub(crate) fn report(&self, limit: usize, max_bytes: usize) -> Value {
        let mut budget = max_bytes;
        let (different, different_count) = listed(
            self.different.iter().map(|different| {
                let mut value = describe(&different.script);
                value["studioLength"] = json!(different.studio_length);
                value["held"] = json!(self.held.contains(&different.script.id));
                value
            }),
            limit,
            &mut budget,
        );
        let (missing, missing_count) =
            listed(self.missing.iter().map(describe), limit, &mut budget);
        let (detached, detached_count) =
            listed(self.detached.iter().map(describe), limit, &mut budget);
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
            "truncated": different.len() < different_count
                || missing.len() < missing_count
                || detached.len() < detached_count,
        })
    }

    /// The scripts `--fix` sets back to their files' contents. Held wipes stay
    /// out unless asked for, because fixing them empties the script in Studio.
    pub(crate) fn fixable(&self, include_held: bool) -> Vec<&Different> {
        self.different
            .iter()
            .filter(|different| include_held || !self.held.contains(&different.script.id))
            .collect()
    }

    pub(crate) fn held_count(&self) -> usize {
        self.different
            .iter()
            .filter(|different| self.held.contains(&different.script.id))
            .count()
    }
}

/// Splits the scripts `--fix` overwrites into groups of at most
/// [`MAX_BACKUP_SCRIPTS`] whose Studio sources add up to at most
/// [`MAX_BACKUP_SOURCE_BYTES`], one backup each. A script
/// larger than that gets a group of its own, which the plugin refuses if its
/// copy does not fit the model limit.
pub(crate) fn backup_groups<'a>(fixes: &[&'a Different]) -> Vec<Vec<&'a Different>> {
    let mut groups = Vec::new();
    let mut group = Vec::new();
    let mut bytes = 0;
    for &fix in fixes {
        if !group.is_empty()
            && (bytes + fix.studio_length > MAX_BACKUP_SOURCE_BYTES
                || group.len() >= MAX_BACKUP_SCRIPTS)
        {
            groups.push(std::mem::take(&mut group));
            bytes = 0;
        }
        bytes += fix.studio_length;
        group.push(fix);
    }
    if !group.is_empty() {
        groups.push(group);
    }
    groups
}

/// One batch of `resync`: a sync message that sets each script's `Source` to
/// the server's, and for each script the hash Studio must still have for the
/// update to apply (`from`) and the hash it must have afterwards (`to`).
pub(crate) struct ResyncBatch {
    pub patch: Value,
    pub expect: Value,
    pub count: usize,
}

/// Builds resync batches for `fixes`, split so that each carries at most
/// [`MAX_RESYNC_BYTES`] of source. A script larger than that gets a batch of
/// its own. The sources are read from the tree now, which may be newer than
/// the comparison that chose the scripts.
pub(crate) fn resync_batches(tree: &RojoTree, fixes: &[&Different]) -> Vec<ResyncBatch> {
    let mut batches = Vec::new();
    let mut updated = Vec::new();
    let mut expect = serde_json::Map::new();
    let mut bytes = 0;
    for fix in fixes {
        let id = fix.script.ref_id;
        let Some(instance) = tree.get_instance(id) else {
            continue;
        };
        let Some(source @ Variant::String(text)) = instance.properties().get(&ustr("Source"))
        else {
            continue;
        };
        if !updated.is_empty() && bytes + text.len() > MAX_RESYNC_BYTES {
            batches.push((std::mem::take(&mut updated), std::mem::take(&mut expect)));
            bytes = 0;
        }
        bytes += text.len();
        let (target, _) = hash_source(text);
        expect.insert(
            fix.script.id.clone(),
            json!({"from": fix.studio_hash, "to": target}),
        );
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
        batches.push((updated, expect));
    }
    batches
        .into_iter()
        .map(|(updated, expect)| ResyncBatch {
            count: updated.len(),
            patch: serde_json::to_value(SubscribeMessage {
                removed: Vec::new(),
                added: HashMap::new(),
                updated,
            })
            .expect("sync messages always serialize"),
            expect: Value::Object(expect),
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
        assert_eq!(second_hash(""), 5381);
        assert_eq!(second_hash("a"), 177_604);
        assert_eq!(second_hash("print(\"Hello, Rojo!\")\n"), 911_966_760);
    }

    #[test]
    fn the_second_hash_tells_apart_a_first_hash_collision() {
        // Two sources found to collide on FNV-1a and length.
        let (a, b) = ("return '07ilh49'", "return '0v3kbm8'");
        assert_eq!(hash_source(a), hash_source(b));
        assert_ne!(second_hash(a), second_hash(b));
        let tree = RojoTree::new(
            InstanceSnapshot::new()
                .name("game")
                .class_name("DataModel")
                .children(vec![script("S", a)]),
        );
        let scripts = tree_scripts(&tree, Path::new("/"));
        let (hash, length) = hash_source(b);
        let answer = json!({"scripts":[{"id": scripts[0].id, "hash": hash, "hash2": second_hash(b), "length": length}]});
        assert!(!compare(scripts, &answer).unwrap().in_sync());
    }

    #[test]
    fn init_scripts_report_their_init_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/Map")).unwrap();
        std::fs::write(
            root.join("default.project.json"),
            r#"{"name":"t","tree":{"$className":"DataModel","ReplicatedStorage":{"Shared":{"$path":"src"}}}}"#,
        )
        .unwrap();
        std::fs::write(root.join("src/Map/init.luau"), "return 1").unwrap();
        std::fs::write(root.join("src/Plain.luau"), "return 2").unwrap();
        std::fs::write(
            root.join("src/Widget.model.json"),
            r#"{"className":"Folder","children":[{"name":"Inner","className":"ModuleScript","properties":{"Source":"return 3"}}]}"#,
        )
        .unwrap();
        let session =
            crate::serve_session::ServeSession::new(memofs::Vfs::new_default().unwrap(), root)
                .unwrap();
        let root_dir = dunce::canonicalize(session.root_dir()).unwrap();
        let scripts = tree_scripts(&session.tree(), &root_dir);
        let file = |name: &str| {
            scripts
                .iter()
                .find(|s| s.path.ends_with(name))
                .and_then(|s| s.file.clone())
        };
        assert_eq!(file("/Map").as_deref(), Some("src/Map/init.luau"));
        assert_eq!(file("/Plain").as_deref(), Some("src/Plain.luau"));
        assert_eq!(file("/Inner").as_deref(), Some("src/Widget.model.json"));
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
        let report = comparison.report(DEFAULT_LISTED, DEFAULT_LIST_BYTES);
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
    fn reports_list_as_many_scripts_as_asked() {
        let tree = tree();
        let scripts = tree_scripts(&tree, Path::new("/"));
        let comparison = compare(scripts, &json!({"scripts": []})).unwrap();
        let short = comparison.report(2, DEFAULT_LIST_BYTES);
        assert_eq!(short["missingCount"], 5);
        assert_eq!(short["missing"].as_array().unwrap().len(), 2);
        assert_eq!(short["truncated"], true);
        let full = comparison.report(MAX_LISTED, DEFAULT_LIST_BYTES);
        assert_eq!(full["missing"].as_array().unwrap().len(), 5);
        assert_eq!(full["truncated"], false);
        // A small byte budget lists fewer, but still counts all.
        let tight = comparison.report(MAX_LISTED, 300);
        assert!(tight["missing"].as_array().unwrap().len() < 5);
        assert_eq!(tight["missingCount"], 5);
        assert_eq!(tight["truncated"], true);
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
        assert_eq!(comparison.held_count(), 1);
        assert_eq!(
            comparison.report(DEFAULT_LISTED, DEFAULT_LIST_BYTES)["different"][0]["held"],
            true
        );
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
    fn backups_before_a_fix_are_split_by_studio_source_size() {
        let tree = tree();
        let scripts = tree_scripts(&tree, Path::new("/"));
        let sized = |lengths: &[usize]| -> Vec<Different> {
            lengths
                .iter()
                .zip(scripts.iter().cycle())
                .map(|(&studio_length, script)| Different {
                    script: script.clone(),
                    studio_hash: 1,
                    studio_length,
                })
                .collect()
        };
        assert!(backup_groups(&[]).is_empty());
        let half = MAX_BACKUP_SOURCE_BYTES / 2;
        let fixes = sized(&[half, half, 1, MAX_BACKUP_SOURCE_BYTES * 2, 10]);
        let refs: Vec<&Different> = fixes.iter().collect();
        let groups = backup_groups(&refs);
        let lengths: Vec<Vec<usize>> = groups
            .iter()
            .map(|group| group.iter().map(|fix| fix.studio_length).collect())
            .collect();
        assert_eq!(
            lengths,
            vec![
                vec![half, half],
                vec![1],
                vec![MAX_BACKUP_SOURCE_BYTES * 2],
                vec![10]
            ],
            "every script is in exactly one group, in order, and an oversized one alone"
        );
    }

    #[test]
    fn backups_before_a_fix_hold_a_bounded_number_of_scripts() {
        let tree = tree();
        let script = tree_scripts(&tree, Path::new("/")).remove(0);
        let fixes: Vec<Different> = (0..MAX_BACKUP_SCRIPTS * 5 + 1)
            .map(|_| Different {
                script: script.clone(),
                studio_hash: 1,
                studio_length: 10,
            })
            .collect();
        let refs: Vec<&Different> = fixes.iter().collect();
        let sizes: Vec<usize> = backup_groups(&refs).iter().map(Vec::len).collect();
        assert_eq!(
            sizes,
            vec![
                MAX_BACKUP_SCRIPTS,
                MAX_BACKUP_SCRIPTS,
                MAX_BACKUP_SCRIPTS,
                MAX_BACKUP_SCRIPTS,
                MAX_BACKUP_SCRIPTS,
                1
            ]
        );
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
        let fixes: Vec<Different> = tree_scripts(&tree, Path::new("/"))
            .into_iter()
            .map(|script| Different {
                script,
                studio_hash: 7,
                studio_length: 1,
            })
            .collect();
        let refs: Vec<&Different> = fixes.iter().collect();
        let batches = resync_batches(&tree, &refs);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].count + batches[1].count, 3);
        let first = &batches[0].patch["updated"][0];
        assert!(first["changedProperties"]["Source"]["String"].is_string());
        assert_eq!(batches[0].patch["added"], json!({}));
        assert_eq!(batches[0].patch["removed"], json!([]));
        let id = first["id"].as_str().unwrap();
        assert_eq!(batches[0].expect[id]["from"], 7);
        assert_eq!(
            batches[0].expect[id]["to"],
            json!(hash_source(&big).0),
            "the target is the hash of the source being sent"
        );
    }
}
