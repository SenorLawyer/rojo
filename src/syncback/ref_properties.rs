//! Implements iterating through an entire WeakDom and linking all Ref
//! properties using attributes.

use std::collections::{HashMap, HashSet, VecDeque};

use rbx_dom_weak::{
    types::{Attributes, Ref, UniqueId, Variant},
    ustr, Instance, Ustr, WeakDom,
};

use crate::syncback::snapshot::inst_path;
use crate::{multimap::MultiMap, REF_ID_ATTRIBUTE_NAME, REF_POINTER_ATTRIBUTE_PREFIX};

pub struct RefLinks {
    /// A map of referents to each of their Ref properties.
    prop_links: MultiMap<Ref, RefLink>,
    /// A set of referents that need their ID rewritten. This includes
    /// Instances that have no existing ID.
    need_rewrite: HashSet<Ref>,
}

#[derive(PartialEq, Eq)]
struct RefLink {
    /// The name of a property
    name: Ustr,
    /// The value of the property.
    value: Ref,
}

/// Iterates through a WeakDom and collects referent properties.
///
/// They can be linked to a dom later using `link_referents`.
pub fn collect_referents(dom: &WeakDom) -> RefLinks {
    let mut ids = HashMap::new();
    let mut need_rewrite = HashSet::new();
    let mut links = MultiMap::new();

    // Note that this is back-in, front-out. This is important because
    // VecDeque::extend is the equivalent to using push_back.
    let mut queue = VecDeque::new();
    queue.push_back(dom.root_ref());
    while let Some(inst_ref) = queue.pop_front() {
        let pointer = dom.get_by_ref(inst_ref).unwrap();
        queue.extend(pointer.children().iter().copied());

        for (prop_name, prop_value) in &pointer.properties {
            let Variant::Ref(prop_value) = prop_value else {
                continue;
            };
            if prop_value.is_none() {
                continue;
            }

            let target = match dom.get_by_ref(*prop_value) {
                Some(inst) => inst,
                None => {
                    // Properties that are Some but point to nothing may as
                    // well be `nil`. Roblox and us never produce these values
                    // but syncback prunes trees without adjusting ref
                    // properties for performance reasons.
                    log::warn!(
                        "Property {}.{} will be `nil` on the disk because the actual value is not being included in syncback",
                        inst_path(dom, inst_ref),
                        prop_name
                    );
                    continue;
                }
            };

            links.insert(
                inst_ref,
                RefLink {
                    name: *prop_name,
                    value: *prop_value,
                },
            );

            // 1. Check if target has an ID
            if let Some(id) = get_existing_id(target) {
                // If it does, we need to check whether that ID is a duplicate
                if let Some(id_ref) = ids.get(id) {
                    // If the same ID points to a new Instance, rewrite it.
                    if id_ref != prop_value {
                        if log::log_enabled!(log::Level::Trace) {
                            log::trace!(
                                "{} needs an id rewritten because it has the same id as {}",
                                target.name,
                                dom.get_by_ref(*id_ref).unwrap().name
                            );
                        }
                        need_rewrite.insert(*prop_value);
                    }
                }
                ids.insert(id, *prop_value);
            } else {
                log::trace!("{} needs an id rewritten because it has no id but is referred to by {}.{prop_name}", target.name, pointer.name);
                // If it does not, it needs one.
                need_rewrite.insert(*prop_value);
            }
        }
    }

    RefLinks {
        need_rewrite,
        prop_links: links,
    }
}

/// See `unlinkable` on [`link_referents`].
pub type Unlinkable<'a> = dyn Fn(&WeakDom, Ref, Option<&str>, bool) -> bool + 'a;

/// Links Ref properties with attributes: gives every instance that is
/// pointed at an ID where it needs one, and every pointer a
/// `Rojo_Target_*` attribute naming it.
///
/// `unlinkable(dom, target, id, needs_new_id)` says a target's ID must not
/// be given or changed, because the change would never be written, such as
/// one in a partition syncback leaves alone. `needs_new_id` is set when
/// linking would have to write one: the target has none, or shares it with
/// another target. A pointer at such a target gets the `Rojo_Target_*`
/// attribute its files have, `on_disk(dom, holder, attribute)`, or none,
/// with a warning, rather than one naming an ID that no file holds.
///
/// `reserved(dom, target, id)` says `id` belongs to that very target in the
/// files and must stay its own: another target with the same ID, such as a
/// copy Studio made, is the one that counts as a duplicate, whatever order
/// they were found in.
pub fn link_referents(
    mut links: RefLinks,
    dom: &mut WeakDom,
    unlinkable: &Unlinkable,
    reserved: &dyn Fn(&WeakDom, Ref, &str) -> bool,
    on_disk: &dyn Fn(&WeakDom, Ref, &str) -> Option<Variant>,
) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    let mut targets: Vec<(Ref, String)> = Vec::new();
    for (_, properties) in links.prop_links.iter() {
        for link in properties {
            if !seen.insert(link.value) {
                continue;
            }
            if let Some(id) = dom.get_by_ref(link.value).and_then(get_existing_id) {
                targets.push((link.value, id.to_owned()));
            }
        }
    }
    let mut owners: HashMap<&str, Ref> = HashMap::new();
    for (target, id) in &targets {
        if reserved(dom, *target, id) {
            owners.entry(id.as_str()).or_insert(*target);
        }
    }
    for (target, id) in &targets {
        if let Some(owner) = owners.get(id.as_str()) {
            if owner == target {
                links.need_rewrite.remove(target);
            } else {
                links.need_rewrite.insert(*target);
            }
        }
    }

    let mut skipped = HashSet::new();
    let mut checked = HashSet::new();
    for (_, properties) in links.prop_links.iter() {
        for link in properties {
            if !checked.insert(link.value) {
                continue;
            }
            if let Some(target) = dom.get_by_ref(link.value) {
                let needs_new_id = links.need_rewrite.contains(&link.value);
                if unlinkable(dom, link.value, get_existing_id(target), needs_new_id) {
                    skipped.insert(link.value);
                }
            }
        }
    }
    write_id_attributes(&links, dom, &skipped)?;

    let mut prop_list = Vec::new();

    for (inst_id, properties) in links.prop_links {
        let mut kept = Vec::new();
        for ref_link in properties {
            if skipped.contains(&ref_link.value) {
                log::warn!(
                    "{}.{} points at {}, which has no ID of its own in the files and is in a \
                     partition with $globIgnorePaths that syncback leaves alone, so the pointer \
                     is left as it was. Give {} a unique ID in its files to sync this ref back.",
                    inst_path(dom, inst_id),
                    ref_link.name,
                    inst_path(dom, ref_link.value),
                    inst_path(dom, ref_link.value),
                );
                let attribute = format!("{REF_POINTER_ATTRIBUTE_PREFIX}{}", ref_link.name);
                if let Some(value) = on_disk(dom, inst_id, &attribute) {
                    kept.push((attribute, value));
                }
                continue;
            }
            let prop_inst = match dom.get_by_ref(ref_link.value) {
                Some(inst) => inst,
                None => continue,
            };
            let id = get_existing_id(prop_inst)
                .expect("all Instances that are pointed to should have an ID");
            prop_list.push((ref_link.name, Variant::String(id.to_owned())));
        }
        let inst = match dom.get_by_ref_mut(inst_id) {
            Some(inst) => inst,
            None => continue,
        };

        let mut attributes: Attributes = match inst.properties.remove(&ustr("Attributes")) {
            Some(Variant::Attributes(attrs)) => attrs,
            None => Attributes::new(),
            Some(value) => {
                anyhow::bail!(
                    "expected Attributes to be of type 'Attributes' but it was of type '{:?}'",
                    value.ty()
                );
            }
        }
        .into_iter()
        .filter(|(name, _)| !name.starts_with(REF_POINTER_ATTRIBUTE_PREFIX))
        .collect();
        for (name, value) in kept {
            attributes.insert(name, value);
        }

        for (prop_name, prop_value) in prop_list.drain(..) {
            attributes.insert(
                format!("{REF_POINTER_ATTRIBUTE_PREFIX}{prop_name}"),
                prop_value,
            );
        }

        inst.properties
            .insert("Attributes".into(), attributes.into());
    }

    Ok(())
}

fn write_id_attributes(
    links: &RefLinks,
    dom: &mut WeakDom,
    skipped: &HashSet<Ref>,
) -> anyhow::Result<()> {
    for referent in links.need_rewrite.difference(skipped) {
        let inst = match dom.get_by_ref_mut(*referent) {
            Some(inst) => inst,
            None => continue,
        };
        let unique_id = match inst.properties.get(&ustr("UniqueId")) {
            Some(Variant::UniqueId(id)) => Some(*id),
            _ => None,
        }
        .unwrap_or_else(|| UniqueId::now().unwrap());

        let attributes = match inst.properties.get_mut(&ustr("Attributes")) {
            Some(Variant::Attributes(attrs)) => attrs,
            None => {
                inst.properties
                    .insert("Attributes".into(), Attributes::new().into());
                match inst.properties.get_mut(&ustr("Attributes")) {
                    Some(Variant::Attributes(attrs)) => attrs,
                    _ => unreachable!(),
                }
            }
            Some(value) => {
                anyhow::bail!(
                    "expected Attributes to be of type 'Attributes' but it was of type '{:?}'",
                    value.ty()
                );
            }
        };
        attributes.insert(
            REF_ID_ATTRIBUTE_NAME.into(),
            Variant::String(unique_id.to_string()),
        );
    }
    Ok(())
}

fn get_existing_id(inst: &Instance) -> Option<&str> {
    if let Variant::Attributes(attrs) = inst.properties.get(&ustr("Attributes"))? {
        let id = attrs.get(REF_ID_ATTRIBUTE_NAME)?;
        match id {
            Variant::String(str) => Some(str),
            Variant::BinaryString(bstr) => std::str::from_utf8(bstr.as_ref()).ok(),
            _ => None,
        }
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use std::{cell::Cell, time::Instant};

    use rbx_dom_weak::InstanceBuilder;

    use super::*;

    /// A large map has many pointers, several at each target. Each target is
    /// checked once, and the whole run stays linear.
    #[test]
    fn each_target_is_checked_once_and_linking_scales() {
        const TARGETS: usize = 20_000;
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let root = dom.root_ref();
        let targets: Vec<Ref> = (0..TARGETS)
            .map(|index| {
                dom.insert(
                    root,
                    InstanceBuilder::new("Folder")
                        .with_name(format!("Target{index}"))
                        .with_property(
                            "Attributes",
                            Attributes::new().with(REF_ID_ATTRIBUTE_NAME, format!("id{index}")),
                        ),
                )
            })
            .collect();
        for (index, target) in targets.iter().enumerate() {
            for copy in 0..2 {
                dom.insert(
                    root,
                    InstanceBuilder::new("ObjectValue")
                        .with_name(format!("Pointer{index}_{copy}"))
                        .with_property("Value", *target),
                );
            }
        }

        let started = Instant::now();
        let links = collect_referents(&dom);
        let (unlinkable_calls, reserved_calls) = (Cell::new(0), Cell::new(0));
        link_referents(
            links,
            &mut dom,
            &|_, _, _, _| {
                unlinkable_calls.set(unlinkable_calls.get() + 1);
                false
            },
            &|_, _, _| {
                reserved_calls.set(reserved_calls.get() + 1);
                false
            },
            &|_, _, _| None,
        )
        .unwrap();
        assert!(
            started.elapsed().as_secs() < 10,
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(unlinkable_calls.get(), TARGETS);
        assert_eq!(reserved_calls.get(), TARGETS);
        let pointer = dom
            .descendants()
            .find(|inst| inst.name == "Pointer7_1")
            .unwrap();
        let Some(Variant::Attributes(attributes)) = pointer.properties.get(&ustr("Attributes"))
        else {
            panic!("no attributes");
        };
        assert_eq!(
            attributes.get("Rojo_Target_Value"),
            Some(&Variant::String("id7".to_owned()))
        );
    }
}
