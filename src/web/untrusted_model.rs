//! Structural checks on untrusted models before they reach a decoder.
//!
//! `rbx_binary` sizes buffers from counts and lengths declared in the file
//! before reading the data they describe, and `rbx_xml` recurses once per
//! nested `<Item>`. These checks reject models whose declared sizes are not
//! backed by the bytes that follow them, which every well-formed model
//! satisfies, so most bad models fail with a specific reason. They do not
//! cover every nested format (attribute blobs, for example), so models are
//! still decoded in a separate process (see `model_check`), where a failed
//! allocation or stack overflow ends only that process.

use std::{borrow::Cow, collections::HashMap};

use anyhow::{bail, ensure, Context};

/// The largest decompressed chunk accepted. Chunks are decoded one at a time.
const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;
/// Bounds the work spent decompressing one model.
const MAX_TOTAL_CHUNK_BYTES: usize = 256 * 1024 * 1024;
/// `rbx_xml` recurses once per nesting level, so this bounds its stack use.
pub(crate) const MAX_XML_DEPTH: usize = 512;
/// Roblox writes one or two metadata entries. The decoder reserves a map for
/// the declared count before reading them.
const MAX_META_ENTRIES: usize = 1024;

const MAGIC: &[u8] = b"<roblox!";
const SIGNATURE: &[u8] = b"\x89\xff\x0d\x0a\x1a\x0a";
const ZSTD_MAGIC: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd];

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.data.len()
    }

    fn bytes(&mut self, len: usize) -> anyhow::Result<&'a [u8]> {
        ensure!(len <= self.data.len(), "Binary model is truncated");
        let (head, tail) = self.data.split_at(len);
        self.data = tail;
        Ok(head)
    }

    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    /// A count of elements that each take at least `size` bytes of what
    /// remains, which the decoder allocates for before reading them.
    fn count(&mut self, size: usize) -> anyhow::Result<usize> {
        let count = self.u32()? as usize;
        ensure!(
            count.saturating_mul(size) <= self.remaining(),
            "Binary model declares more data than it contains"
        );
        Ok(count)
    }

    /// A length-prefixed string, which the decoder allocates before reading.
    fn string(&mut self) -> anyhow::Result<&'a [u8]> {
        let len = self.count(1)?;
        self.bytes(len)
    }

    /// An array of `count` 4-byte values, allocated in full before reading.
    fn array(&mut self, count: usize) -> anyhow::Result<()> {
        self.bytes(count.saturating_mul(4)).map(|_| ())
    }
}

/// Checks a binary model's declared sizes against its contents, following
/// the layout `rbx_binary` 3 reads.
pub(crate) fn check_binary(bytes: &[u8], max_instances: usize) -> anyhow::Result<()> {
    let mut input = Reader { data: bytes };
    ensure!(
        input.bytes(MAGIC.len())? == MAGIC && input.bytes(SIGNATURE.len())? == SIGNATURE,
        "Binary model has an invalid header"
    );
    ensure!(
        input.bytes(2)? == [0, 0],
        "Binary model has an unknown version"
    );
    let types = input.u32()? as usize;
    let instances = input.u32()? as usize;
    // The decoder reserves room for both counts up front.
    ensure!(
        types <= max_instances && instances <= max_instances,
        "Binary model declares too many instances"
    );
    ensure!(
        input.bytes(8)? == [0; 8],
        "Binary model has an invalid header"
    );

    // Instance counts of each INST chunk, which PROP chunks read values for.
    let mut classes: HashMap<u32, usize> = HashMap::new();
    let mut total_instances = 0usize;
    let mut total_chunk_bytes = 0usize;
    loop {
        let name: [u8; 4] = input.bytes(4)?.try_into().unwrap();
        let compressed_len = input.u32()? as usize;
        let len = input.u32()? as usize;
        ensure!(input.u32()? == 0, "Binary model has an invalid chunk");
        ensure!(
            len <= MAX_CHUNK_BYTES,
            "Binary model has an oversized chunk"
        );
        total_chunk_bytes += len;
        ensure!(
            total_chunk_bytes <= MAX_TOTAL_CHUNK_BYTES,
            "Binary model decompresses to too much data"
        );
        let data = if compressed_len == 0 {
            Cow::Borrowed(input.bytes(len)?)
        } else {
            let compressed = input.bytes(compressed_len)?;
            let data = if compressed.starts_with(ZSTD_MAGIC) {
                zstd::bulk::decompress(compressed, len).ok()
            } else {
                ensure!(compressed.len() >= 4, "Binary model has an invalid chunk");
                lz4_flex::block::decompress(compressed, len).ok()
            };
            Cow::Owned(data.context("Binary model has a chunk that does not decompress")?)
        };
        ensure!(data.len() == len, "Binary model has an invalid chunk");
        let mut chunk = Reader { data: &data };
        match &name {
            b"META" => {
                // Each entry is two strings of at least a length prefix.
                let entries = chunk.count(8)?;
                ensure!(
                    entries <= MAX_META_ENTRIES,
                    "Binary model has too many metadata entries"
                );
                for _ in 0..entries {
                    chunk.string()?;
                    chunk.string()?;
                }
            }
            b"SSTR" => {
                chunk.u32()?;
                for _ in 0..chunk.u32()? {
                    chunk.bytes(16)?;
                    chunk.string()?;
                }
            }
            b"INST" => {
                let type_id = chunk.u32()?;
                chunk.string()?;
                chunk.u8()?;
                let count = chunk.count(4)?;
                total_instances += count;
                ensure!(
                    total_instances <= max_instances,
                    "Binary model declares too many instances"
                );
                classes.insert(type_id, count);
            }
            b"PROP" => check_prop(&mut chunk, &classes)?,
            b"PRNT" => {
                chunk.u8()?;
                let count = chunk.count(4)?;
                chunk.array(count)?;
                chunk.array(count)?;
            }
            b"END\0" => return Ok(()),
            _ => {}
        }
    }
}

fn check_prop(chunk: &mut Reader, classes: &HashMap<u32, usize>) -> anyhow::Result<()> {
    let type_id = chunk.u32()?;
    let name = chunk.string()?;
    // The decoder rejects properties of unknown classes before reading them.
    let Some(&instances) = classes.get(&type_id) else {
        return Ok(());
    };
    // Chunks without a type byte, or with one the decoder does not know,
    // are skipped.
    let Ok(ty) = chunk.u8() else {
        return Ok(());
    };
    if !matches!(ty, 0x01..=0x0E | 0x10 | 0x12..=0x1C | 0x1E..=0x22) {
        return Ok(());
    }
    // Every known type that is not listed below reads fixed-size values for
    // each instance, which the instance limit already bounds.
    if name == b"Name" || ty == 0x01 {
        // `Name` is read as strings whatever its declared type.
        for _ in 0..instances {
            chunk.string()?;
        }
        return Ok(());
    }
    match ty {
        // Font
        0x20 => {
            for _ in 0..instances {
                chunk.string()?;
                chunk.bytes(3)?;
                chunk.string()?;
            }
        }
        // NumberSequence: time, value and envelope per keypoint.
        0x15 => {
            for _ in 0..instances {
                let keypoints = chunk.count(12)?;
                chunk.bytes(keypoints * 12)?;
            }
        }
        // ColorSequence: time, three color channels and envelope.
        0x16 => {
            for _ in 0..instances {
                let keypoints = chunk.count(20)?;
                chunk.bytes(keypoints * 20)?;
            }
        }
        // Content: kinds, then URIs, object referents and external referents.
        0x22 => {
            chunk.array(instances)?;
            // Each value is one kind of content, so no list is longer than
            // the number of instances. The decoder reserves each in full.
            let uris = chunk.count(4)?;
            ensure!(
                uris <= instances,
                "Binary model has an invalid Content property"
            );
            for _ in 0..uris {
                chunk.string()?;
            }
            let objects = chunk.count(4)?;
            ensure!(
                objects <= instances,
                "Binary model has an invalid Content property"
            );
            chunk.array(objects)?;
            ensure!(
                chunk.count(4)? <= instances,
                "Binary model has an invalid Content property"
            );
        }
        _ => {}
    }
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Checks that an XML model does not nest `<Item>` elements deeper than
/// [`MAX_XML_DEPTH`]. Comments, CDATA sections, processing instructions and
/// quoted attribute values are skipped, so text inside them is not counted.
///
/// The XML parser also decodes other encodings and expands entities into
/// markup, which this scan would not see, so only UTF-8 without a document
/// type declaration is accepted. Roblox writes models that way.
pub(crate) fn check_xml_depth(bytes: &[u8]) -> anyhow::Result<()> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    ensure!(
        std::str::from_utf8(bytes).is_ok() && !bytes.contains(&0),
        "Import XML must be UTF-8 text"
    );
    ensure!(
        !contains(bytes, b"<!DOCTYPE") && !contains(bytes, b"<!ENTITY"),
        "Import XML cannot declare a document type or entities"
    );
    if bytes.starts_with(b"<?xml") {
        let declaration = &bytes[..bytes.len().min(
            bytes
                .windows(2)
                .position(|window| window == b"?>")
                .unwrap_or(bytes.len()),
        )];
        let declaration = String::from_utf8_lossy(declaration).to_ascii_lowercase();
        ensure!(
            !declaration.contains("encoding")
                || declaration.contains("\"utf-8\"")
                || declaration.contains("'utf-8'"),
            "Import XML must be UTF-8 text"
        );
    }
    let mut depth = 0usize;
    let mut index = 0;
    let find = |from: usize, needle: &[u8]| {
        bytes[from..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|position| from + position + needle.len())
    };
    while let Some(offset) = bytes[index..].iter().position(|&b| b == b'<') {
        let start = index + offset;
        let rest = &bytes[start..];
        if rest.starts_with(b"<!--") {
            index = find(start + 4, b"-->").unwrap_or(bytes.len());
        } else if rest.starts_with(b"<![CDATA[") {
            index = find(start + 9, b"]]>").unwrap_or(bytes.len());
        } else if rest.starts_with(b"<?") {
            index = find(start + 2, b"?>").unwrap_or(bytes.len());
        } else {
            // Find the end of the tag, skipping quoted attribute values.
            let mut end = start + 1;
            let mut quote = None;
            while end < bytes.len() {
                match (quote, bytes[end]) {
                    (None, b'"' | b'\'') => quote = Some(bytes[end]),
                    (Some(open), byte) if byte == open => quote = None,
                    (None, b'>') => break,
                    _ => {}
                }
                end += 1;
            }
            let tag = &bytes[start + 1..end.min(bytes.len())];
            // The decoder matches local names, so `<a:Item>` counts too.
            let is_item = |tag: &[u8]| {
                let name_end = tag
                    .iter()
                    .position(|&b| b.is_ascii_whitespace() || b == b'/')
                    .unwrap_or(tag.len());
                let name = &tag[..name_end];
                let local = name
                    .iter()
                    .rposition(|&b| b == b':')
                    .map_or(name, |colon| &name[colon + 1..]);
                local == b"Item"
            };
            if let Some(closing) = tag.strip_prefix(b"/") {
                if is_item(closing) {
                    depth = depth.saturating_sub(1);
                }
            } else if is_item(tag) && !tag.ends_with(b"/") {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    bail!("Import model nests instances more than {MAX_XML_DEPTH} levels deep");
                }
            }
            index = end + 1;
        }
        if index >= bytes.len() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(types: u32, instances: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(SIGNATURE);
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&types.to_le_bytes());
        bytes.extend_from_slice(&instances.to_le_bytes());
        bytes.extend_from_slice(&[0; 8]);
        bytes
    }

    fn chunk(bytes: &mut Vec<u8>, name: &[u8; 4], compressed: u32, data: &[u8], len: u32) {
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&compressed.to_le_bytes());
        bytes.extend_from_slice(&len.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(data);
    }

    #[test]
    fn models_written_by_rbx_binary_pass() {
        use rbx_dom_weak::{
            types::{
                Attributes, Color3, ColorSequence, ColorSequenceKeypoint, Content, Font,
                NumberSequence, NumberSequenceKeypoint, Tags, Variant,
            },
            InstanceBuilder, WeakDom,
        };

        let mut tree = WeakDom::new(InstanceBuilder::new("DataModel"));
        let mut attributes = Attributes::new();
        attributes.insert("Key".into(), Variant::String("value".into()));
        let mut tags = Tags::new();
        tags.push("Tagged");
        let folder = tree.insert(
            tree.root_ref(),
            InstanceBuilder::new("Folder")
                .with_name("Folder")
                .with_property("Attributes", attributes)
                .with_property("Tags", tags),
        );
        tree.insert(
            folder,
            InstanceBuilder::new("ParticleEmitter")
                .with_property(
                    "Size",
                    NumberSequence {
                        keypoints: vec![
                            NumberSequenceKeypoint::new(0.0, 1.0, 0.0),
                            NumberSequenceKeypoint::new(1.0, 2.0, 0.0),
                        ],
                    },
                )
                .with_property(
                    "Color",
                    ColorSequence {
                        keypoints: vec![
                            ColorSequenceKeypoint::new(0.0, Color3::new(1.0, 0.0, 0.0)),
                            ColorSequenceKeypoint::new(1.0, Color3::new(0.0, 0.0, 1.0)),
                        ],
                    },
                ),
        );
        tree.insert(
            folder,
            InstanceBuilder::new("TextLabel")
                .with_property("FontFace", Font::default())
                .with_property("Text", "x".repeat(10_000)),
        );
        tree.insert(
            folder,
            InstanceBuilder::new("ImageLabel")
                .with_property("ImageContent", Content::from_uri("rbxassetid://1")),
        );
        for compression in [
            rbx_binary::CompressionType::Lz4,
            rbx_binary::CompressionType::Zstd,
            rbx_binary::CompressionType::None,
        ] {
            let mut bytes = Vec::new();
            rbx_binary::Serializer::new()
                .compression_type(compression)
                .serialize(&mut bytes, &tree, &[folder])
                .unwrap();
            check_binary(&bytes, 10_000).unwrap();
            rbx_binary::from_reader(bytes.as_slice()).unwrap();
            // Every truncation is rejected without reaching the decoder.
            for len in (0..bytes.len()).step_by(7) {
                assert!(check_binary(&bytes[..len], 10_000).is_err());
            }
        }
    }

    #[test]
    fn huge_header_counts_are_rejected() {
        let error = check_binary(&header(u32::MAX, u32::MAX), 10_000).unwrap_err();
        assert!(error.to_string().contains("too many instances"));
    }

    #[test]
    fn chunk_lengths_must_be_backed_by_input() {
        let mut bytes = header(1, 1);
        chunk(&mut bytes, b"INST", 0, &[], u32::MAX);
        assert!(check_binary(&bytes, 10_000).is_err());

        let mut bytes = header(1, 1);
        chunk(&mut bytes, b"INST", u32::MAX, &[], 16);
        assert!(check_binary(&bytes, 10_000).is_err());

        let mut bytes = header(1, 1);
        chunk(&mut bytes, b"INST", 0, &[0; 16], 16);
        // Shorter than its declared length.
        bytes.truncate(bytes.len() - 1);
        assert!(check_binary(&bytes, 10_000).is_err());
    }

    #[test]
    fn counts_inside_chunks_must_be_backed_by_input() {
        // META declares four billion entries.
        let mut bytes = header(0, 0);
        chunk(&mut bytes, b"META", 0, &u32::MAX.to_le_bytes(), 4);
        assert!(check_binary(&bytes, 10_000).is_err());

        // An instance name claims to be 4GiB long.
        let mut inst = Vec::new();
        inst.extend_from_slice(&0u32.to_le_bytes());
        inst.extend_from_slice(&6u32.to_le_bytes());
        inst.extend_from_slice(b"Folder");
        inst.push(0);
        inst.extend_from_slice(&1u32.to_le_bytes());
        inst.extend_from_slice(&[0; 4]);
        let named = |name_len: u32| {
            let mut prop = Vec::new();
            prop.extend_from_slice(&0u32.to_le_bytes());
            prop.extend_from_slice(&4u32.to_le_bytes());
            prop.extend_from_slice(b"Name");
            prop.push(0x01);
            prop.extend_from_slice(&name_len.to_le_bytes());
            let mut bytes = header(1, 1);
            chunk(&mut bytes, b"INST", 0, &inst, inst.len() as u32);
            chunk(&mut bytes, b"PROP", 0, &prop, prop.len() as u32);
            chunk(&mut bytes, b"END\0", 0, &[], 0);
            bytes
        };
        check_binary(&named(0), 10_000).unwrap();
        assert!(check_binary(&named(u32::MAX), 10_000).is_err());
    }

    #[test]
    fn deep_xml_is_rejected_and_shallow_xml_passes() {
        let nested = |depth: usize| {
            let mut xml = String::from("<roblox version=\"4\">");
            for _ in 0..depth {
                xml.push_str("<Item class=\"Folder\" referent=\"a>b\"><Properties/>");
            }
            for _ in 0..depth {
                xml.push_str("</Item>");
            }
            xml.push_str("<!-- <Item class=\"Folder\"> --></roblox>");
            xml
        };
        assert!(check_xml_depth(nested(MAX_XML_DEPTH).as_bytes()).is_ok());
        assert!(check_xml_depth(nested(MAX_XML_DEPTH + 1).as_bytes()).is_err());
        // Siblings do not add depth.
        let siblings = "<Item class=\"Folder\"></Item>".repeat(MAX_XML_DEPTH * 2);
        assert!(check_xml_depth(siblings.as_bytes()).is_ok());
        // Prefixed names are matched by their local name.
        let prefixed = nested(MAX_XML_DEPTH + 1).replace("Item", "a:Item");
        assert!(check_xml_depth(prefixed.as_bytes()).is_err());
    }

    #[test]
    fn xml_the_depth_scan_cannot_see_into_is_rejected() {
        let entities = "<!DOCTYPE r [<!ENTITY o \"<Item class='Folder'>\">]>\
                        <roblox version=\"4\">&o;</roblox>";
        assert!(check_xml_depth(entities.as_bytes()).is_err());
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "<roblox version=\"4\"></roblox>".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert!(check_xml_depth(&utf16).is_err());
        let declared = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><roblox version=\"4\"></roblox>";
        assert!(check_xml_depth(declared.as_bytes()).is_err());
        let plain =
            "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><roblox version=\"4\"></roblox>";
        assert!(check_xml_depth(plain.as_bytes()).is_ok());
    }
}
