use rbx_dom_weak::{types::Variant, Instance, Ustr, UstrMap};
use rbx_reflection::{PropertyKind, PropertySerialization, Scriptability};

use crate::{variant_eq::variant_eq, Project};

/// Returns a map of properties from `inst` that are both allowed under the
/// user-provided settings, are not their default value, and serialize.
pub fn filter_properties<'inst>(
    project: &Project,
    inst: &'inst Instance,
) -> UstrMap<&'inst Variant> {
    let mut map: Vec<(Ustr, &Variant)> = Vec::with_capacity(inst.properties.len());
    filter_properties_preallocated(project, inst, &mut map);

    map.into_iter().collect()
}

/// Fills `allocation` with a list of properties from `inst` that are
/// user-provided settings, are not their default value, and serialize.
pub fn filter_properties_preallocated<'inst>(
    project: &Project,
    inst: &'inst Instance,
    allocation: &mut Vec<(Ustr, &'inst Variant)>,
) {
    let class_data = rbx_reflection_database::get()
        .unwrap()
        .classes
        .get(inst.class.as_str());

    let predicate = |prop_name: &Ustr, prop_value: &Variant| {
        is_excluded_by_rules(project, &inst.class, prop_name, prop_value)
    };

    if let Some(class_data) = class_data {
        let defaults = &class_data.default_properties;
        for (name, value) in &inst.properties {
            if predicate(name, value) {
                continue;
            }
            if let Some(default) = defaults.get(name.as_str()) {
                if !variant_eq(value, default) {
                    allocation.push((*name, value));
                }
            } else {
                allocation.push((*name, value));
            }
        }
    } else {
        for (name, value) in &inst.properties {
            if predicate(name, value) {
                continue;
            }
            allocation.push((*name, value));
        }
    }
}

/// Returns whether `filter_properties` leaves this property out for a reason
/// other than its value being the default: it is a Ref or UniqueId, does not
/// serialize, or is unscriptable while `syncUnscriptable` is off. Syncback has
/// no value for such a property, so an entry for it in a file must be kept
/// rather than taken for a property reset to its default.
pub fn is_excluded_by_rules(
    project: &Project,
    class_name: &str,
    prop_name: &str,
    prop_value: &Variant,
) -> bool {
    // We don't want to serialize Ref or UniqueId properties in JSON files
    if matches!(prop_value, Variant::Ref(_) | Variant::UniqueId(_)) {
        return true;
    }
    if !should_property_serialize(class_name, prop_name) {
        return true;
    }
    let sync_unscriptable = project
        .syncback_rules
        .as_ref()
        .and_then(|s| s.sync_unscriptable)
        .unwrap_or(true);
    if !sync_unscriptable {
        let class_data = rbx_reflection_database::get()
            .ok()
            .and_then(|db| db.classes.get(class_name));
        if let Some(prop_data) = class_data.and_then(|data| data.properties.get(prop_name)) {
            if matches!(prop_data.scriptability, Scriptability::None) {
                return true;
            }
        }
    }
    false
}

fn should_property_serialize(class_name: &str, prop_name: &str) -> bool {
    let database = rbx_reflection_database::get().unwrap();
    let mut current_class_name = class_name;

    loop {
        let class_data = match database.classes.get(current_class_name) {
            Some(data) => data,
            None => return true,
        };
        if let Some(data) = class_data.properties.get(prop_name) {
            log::trace!("found {class_name}.{prop_name} on {current_class_name}");
            return match &data.kind {
                // It's not really clear if this can ever happen but I want to
                // support it just in case!
                PropertyKind::Alias { alias_for } => {
                    should_property_serialize(current_class_name, alias_for)
                }
                // Migrations and aliases are happily handled for us by parsers
                // so we don't really need to handle them.
                PropertyKind::Canonical { serialization } => {
                    !matches!(serialization, PropertySerialization::DoesNotSerialize)
                }
                kind => unimplemented!("unknown property kind {kind:?}"),
            };
        } else if let Some(super_class) = class_data.superclass {
            current_class_name = super_class;
        } else {
            break;
        }
    }
    true
}
