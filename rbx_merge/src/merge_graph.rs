//! The three-way merge itself: combining matched instances into a single
//! merged graph, resolving child order, and lowering that graph back into a
//! `WeakDom` for encoding.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use rbx_types::{Attributes, Content, Ref, UniqueId, Variant};
use ustr::{Ustr, ustr};

use crate::Error;
use crate::conflict::{Conflict, ConflictKind, DisplayValue};
use crate::diagnostics::{Diagnostic, dropped_reference_diagnostic, unknown_property_diagnostic};
use crate::identity::{IdentitySet, MergeEntry, MergeNodeId};
use crate::render::display_variant;
use crate::resolve::{Resolutions, Side};
use crate::semantic::{NodeId, SemanticDom, SemanticInputs, ValueSource, variant_options_equal};

#[derive(Debug, Clone)]
pub(crate) struct MergedGraph {
    pub(crate) root: MergeNodeId,
    pub(crate) nodes: IndexMap<MergeNodeId, MergedInstance>,
}

#[derive(Debug, Clone)]
pub(crate) struct MergedInstance {
    pub(crate) class: Ustr,
    pub(crate) name: String,
    pub(crate) parent: Option<MergeNodeId>,
    pub(crate) children: Vec<MergeNodeId>,
    pub(crate) properties: BTreeMap<Ustr, MergedProperty>,
    pub(crate) referent: Ref,
}

#[derive(Debug, Clone)]
pub(crate) struct MergedProperty {
    pub(crate) value: Variant,
    pub(crate) source: ValueSource,
}

fn child_merge_ids(
    dom: &SemanticDom,
    parent: NodeId,
    source: ValueSource,
    identities: &IdentitySet,
    final_parent: MergeNodeId,
    graph: &MergedGraph,
) -> Vec<MergeNodeId> {
    dom.node(parent)
        .children
        .iter()
        .filter_map(|child| identities.lookup(source, *child))
        .filter(|child_id| {
            graph
                .nodes
                .get(child_id)
                .is_some_and(|node| node.parent == Some(final_parent))
        })
        .collect()
}

/// Pick one of three per-side values by resolved `Side`.
fn pick<T>(side: Side, base: T, ours: T, theirs: T) -> T {
    match side {
        Side::Base => base,
        Side::Ours => ours,
        Side::Theirs => theirs,
    }
}

fn side_source(side: Side) -> ValueSource {
    match side {
        Side::Base => ValueSource::Base,
        Side::Ours => ValueSource::Ours,
        Side::Theirs => ValueSource::Theirs,
    }
}

pub(crate) fn merge_semantic_graph(
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    identities: &IdentitySet,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) -> Result<MergedGraph, Error> {
    let doms = SemanticInputs { base, ours, theirs };
    let root = *identities
        .base_to_merge
        .get(&base.root)
        .ok_or_else(|| Error::Internal("base root was not assigned a merge identity".to_owned()))?;
    let mut graph = MergedGraph {
        root,
        nodes: IndexMap::new(),
    };
    let mut used_refs = HashSet::new();

    for (&merge_id, entry) in &identities.entries {
        match deletion_decision(
            entry,
            base,
            ours,
            theirs,
            identities,
            resolutions,
            conflicts,
        ) {
            NodeDecision::Drop => continue,
            NodeDecision::TakeSide(side) => {
                let instance =
                    materialize_from_side(entry, side, &doms, identities, &mut used_refs);
                graph.nodes.insert(merge_id, instance);
                continue;
            }
            NodeDecision::Merge => {}
        }

        let Some(class) = merge_class(entry, base, ours, theirs, resolutions, conflicts) else {
            continue;
        };
        let Some(name) = merge_name(entry, base, ours, theirs, resolutions, conflicts) else {
            continue;
        };
        let parent = merge_parent(
            entry,
            base,
            ours,
            theirs,
            identities,
            resolutions,
            conflicts,
        );
        let properties = merge_properties(entry, &doms, identities, resolutions, conflicts);
        let referent = choose_referent(entry, &doms, &mut used_refs);

        graph.nodes.insert(
            merge_id,
            MergedInstance {
                class,
                name,
                parent,
                children: Vec::new(),
                properties,
                referent,
            },
        );
    }

    Ok(graph)
}

/// What to do with one merge identity before the per-field merge runs.
enum NodeDecision {
    /// Proceed with the normal three-way merge.
    Merge,
    /// Omit the node (deleted, or a resolved delete/modify favoring deletion).
    Drop,
    /// A resolved delete/modify favoring a surviving side: materialize the node
    /// from that side's content alone.
    TakeSide(Side),
}

/// Build a merged instance from a single side's content, used when a
/// delete/modify conflict was resolved in favor of keeping that side.
fn materialize_from_side(
    entry: &MergeEntry,
    side: Side,
    doms: &SemanticInputs<'_>,
    identities: &IdentitySet,
    used_refs: &mut HashSet<Ref>,
) -> MergedInstance {
    let source = side_source(side);
    let (dom, node_id) = match side {
        Side::Base => (doms.base, entry.base),
        Side::Ours => (doms.ours, entry.ours),
        Side::Theirs => (doms.theirs, entry.theirs),
    };
    let node = dom.node(node_id.expect("resolved side has a node"));
    let parent = node
        .parent
        .and_then(|parent| identities.lookup(source, parent));
    let properties = node
        .properties
        .iter()
        .map(|(&key, value)| {
            (
                key,
                MergedProperty {
                    value: value.clone(),
                    source,
                },
            )
        })
        .collect();
    let referent = choose_referent(entry, doms, used_refs);
    MergedInstance {
        class: node.class,
        name: node.name.clone(),
        parent,
        children: Vec::new(),
        properties,
        referent,
    }
}

fn deletion_decision(
    entry: &MergeEntry,
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    identities: &IdentitySet,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) -> NodeDecision {
    let Some(base_id) = entry.base else {
        return NodeDecision::Merge;
    };

    match (entry.ours, entry.theirs) {
        (None, None) => NodeDecision::Drop,
        (None, Some(theirs_id)) => {
            if side_node_changed_from_base(
                base_id,
                theirs_id,
                ValueSource::Theirs,
                base,
                theirs,
                identities,
            ) {
                resolve_delete_modify(
                    entry,
                    base,
                    base_id,
                    "deleted",
                    "modified",
                    resolutions,
                    conflicts,
                )
            } else {
                NodeDecision::Drop
            }
        }
        (Some(ours_id), None) => {
            if side_node_changed_from_base(
                base_id,
                ours_id,
                ValueSource::Ours,
                base,
                ours,
                identities,
            ) {
                resolve_delete_modify(
                    entry,
                    base,
                    base_id,
                    "modified",
                    "deleted",
                    resolutions,
                    conflicts,
                )
            } else {
                NodeDecision::Drop
            }
        }
        (Some(_), Some(_)) => NodeDecision::Merge,
    }
}

/// Decide a delete/modify conflict: honor a resolution if one applies (drop when
/// it favors the deleting side, keep when it favors a surviving side), otherwise
/// report the conflict and drop the node as before.
fn resolve_delete_modify(
    entry: &MergeEntry,
    base: &SemanticDom,
    base_id: NodeId,
    ours_label: &str,
    theirs_label: &str,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) -> NodeDecision {
    let path = base.path(base_id);
    if let Some(side) = resolutions.lookup(&ConflictKind::DeleteModify, &path, None) {
        let present = match side {
            Side::Base => entry.base,
            Side::Ours => entry.ours,
            Side::Theirs => entry.theirs,
        };
        return match present {
            Some(_) => NodeDecision::TakeSide(side),
            None => NodeDecision::Drop,
        };
    }
    conflicts.push(node_conflict(
        ConflictKind::DeleteModify,
        base,
        base_id,
        None,
        Some("present in base"),
        Some(ours_label),
        Some(theirs_label),
    ));
    NodeDecision::Drop
}

fn side_node_changed_from_base(
    base_id: NodeId,
    side_id: NodeId,
    side_source: ValueSource,
    base: &SemanticDom,
    side: &SemanticDom,
    identities: &IdentitySet,
) -> bool {
    let base_node = base.node(base_id);
    let side_node = side.node(side_id);
    if base_node.class != side_node.class || base_node.name != side_node.name {
        return true;
    }

    let doms = match side_source {
        ValueSource::Ours => SemanticInputs {
            base,
            ours: side,
            theirs: side,
        },
        ValueSource::Theirs => SemanticInputs {
            base,
            ours: side,
            theirs: side,
        },
        _ => return true,
    };

    let property_keys: BTreeSet<_> = base_node
        .properties
        .keys()
        .chain(side_node.properties.keys())
        .copied()
        .collect();
    for key in property_keys {
        let base_value = base_node.properties.get(&key);
        let side_value = side_node.properties.get(&key);
        if !variant_options_equal(
            base_value,
            ValueSource::Base,
            side_value,
            side_source,
            identities,
            &doms,
        ) {
            return true;
        }
    }

    let base_parent = base_node
        .parent
        .and_then(|parent| identities.base_to_merge.get(&parent).copied());
    let side_parent = side_node
        .parent
        .and_then(|parent| identities.lookup(side_source, parent));
    if base_parent != side_parent {
        return true;
    }

    let base_children: Vec<_> = base_node
        .children
        .iter()
        .filter_map(|child| identities.base_to_merge.get(child).copied())
        .collect();
    let side_children: Vec<_> = side_node
        .children
        .iter()
        .filter_map(|child| identities.lookup(side_source, *child))
        .collect();
    base_children != side_children
}

fn merge_class(
    entry: &MergeEntry,
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) -> Option<Ustr> {
    let base_value = entry.base.map(|id| base.node(id).class);
    let ours_value = entry.ours.map(|id| ours.node(id).class);
    let theirs_value = entry.theirs.map(|id| theirs.node(id).class);
    merge_scalar(base_value, ours_value, theirs_value).or_else(|| {
        let (dom, id) = conflict_subject(entry, base, ours, theirs);
        let path = dom.path(id);
        if let Some(side) =
            resolutions.lookup(&ConflictKind::InstanceIdentity, &path, Some("ClassName"))
            && let Some(resolved) = pick(side, base_value, ours_value, theirs_value)
        {
            return Some(resolved);
        }
        conflicts.push(node_conflict(
            ConflictKind::InstanceIdentity,
            dom,
            id,
            Some("ClassName"),
            base_value.map(|value| value.to_string()),
            ours_value.map(|value| value.to_string()),
            theirs_value.map(|value| value.to_string()),
        ));
        None
    })
}

fn merge_name(
    entry: &MergeEntry,
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) -> Option<String> {
    let base_value = entry.base.map(|id| base.node(id).name.clone());
    let ours_value = entry.ours.map(|id| ours.node(id).name.clone());
    let theirs_value = entry.theirs.map(|id| theirs.node(id).name.clone());
    merge_scalar(base_value.clone(), ours_value.clone(), theirs_value.clone()).or_else(|| {
        let (dom, id) = conflict_subject(entry, base, ours, theirs);
        let path = dom.path(id);
        if let Some(side) = resolutions.lookup(&ConflictKind::InstanceIdentity, &path, Some("Name"))
            && let Some(resolved) = pick(
                side,
                base_value.clone(),
                ours_value.clone(),
                theirs_value.clone(),
            )
        {
            return Some(resolved);
        }
        conflicts.push(node_conflict(
            ConflictKind::InstanceIdentity,
            dom,
            id,
            Some("Name"),
            base_value,
            ours_value,
            theirs_value,
        ));
        None
    })
}

fn merge_parent(
    entry: &MergeEntry,
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    identities: &IdentitySet,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) -> Option<MergeNodeId> {
    let base_parent = entry
        .base
        .and_then(|id| base.node(id).parent)
        .and_then(|id| identities.base_to_merge.get(&id).copied());
    let ours_parent = entry
        .ours
        .and_then(|id| ours.node(id).parent)
        .and_then(|id| identities.ours_to_merge.get(&id).copied());
    let theirs_parent = entry
        .theirs
        .and_then(|id| theirs.node(id).parent)
        .and_then(|id| identities.theirs_to_merge.get(&id).copied());

    match merge_optional_scalar(base_parent, ours_parent, theirs_parent) {
        Ok(parent) => parent,
        Err(()) => {
            let (dom, id) = conflict_subject(entry, base, ours, theirs);
            let path = dom.path(id);
            if let Some(side) = resolutions.lookup(&ConflictKind::ParentMove, &path, None) {
                return pick(side, base_parent, ours_parent, theirs_parent);
            }
            conflicts.push(node_conflict(
                ConflictKind::ParentMove,
                dom,
                id,
                None,
                base_parent.map(|value| parent_label(value, identities, base, ours, theirs)),
                ours_parent.map(|value| parent_label(value, identities, base, ours, theirs)),
                theirs_parent.map(|value| parent_label(value, identities, base, ours, theirs)),
            ));
            base_parent
        }
    }
}

fn merge_properties(
    entry: &MergeEntry,
    doms: &SemanticInputs<'_>,
    identities: &IdentitySet,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) -> BTreeMap<Ustr, MergedProperty> {
    let base_node = entry.base.map(|id| doms.base.node(id));
    let ours_node = entry.ours.map(|id| doms.ours.node(id));
    let theirs_node = entry.theirs.map(|id| doms.theirs.node(id));
    let keys: BTreeSet<_> = base_node
        .into_iter()
        .flat_map(|node| node.properties.keys())
        .chain(
            ours_node
                .into_iter()
                .flat_map(|node| node.properties.keys()),
        )
        .chain(
            theirs_node
                .into_iter()
                .flat_map(|node| node.properties.keys()),
        )
        .copied()
        .collect();

    let mut merged = BTreeMap::new();
    for key in keys {
        let base_value = entry
            .base
            .and_then(|id| doms.base.node(id).properties.get(&key));
        let ours_value = entry
            .ours
            .and_then(|id| doms.ours.node(id).properties.get(&key));
        let theirs_value = entry
            .theirs
            .and_then(|id| doms.theirs.node(id).properties.get(&key));

        match merge_property_value(key, base_value, ours_value, theirs_value, doms, identities) {
            PropertyMerge::Keep(property) => {
                merged.insert(key, property);
            }
            PropertyMerge::Delete => {}
            PropertyMerge::Conflict => {
                let (dom, id) = conflict_subject(entry, doms.base, doms.ours, doms.theirs);
                let path = dom.path(id);
                if let Some(side) =
                    resolutions.lookup(&ConflictKind::PropertyValue, &path, Some(key.as_str()))
                {
                    // The chosen side may not have the property, in which case
                    // resolving to that side means dropping it.
                    if let Some(value) = pick(side, base_value, ours_value, theirs_value) {
                        merged.insert(
                            key,
                            MergedProperty {
                                value: value.clone(),
                                source: side_source(side),
                            },
                        );
                    }
                    continue;
                }
                conflicts.push(node_conflict(
                    ConflictKind::PropertyValue,
                    dom,
                    id,
                    Some(key.as_str()),
                    base_value.map(|value| display_variant(value, ValueSource::Base, doms)),
                    ours_value.map(|value| display_variant(value, ValueSource::Ours, doms)),
                    theirs_value.map(|value| display_variant(value, ValueSource::Theirs, doms)),
                ));
            }
        }
    }

    merged
}

enum PropertyMerge {
    Keep(MergedProperty),
    Delete,
    Conflict,
}

fn merge_property_value(
    key: Ustr,
    base: Option<&Variant>,
    ours: Option<&Variant>,
    theirs: Option<&Variant>,
    doms: &SemanticInputs<'_>,
    identities: &IdentitySet,
) -> PropertyMerge {
    if key == ustr("Attributes") && attributes_merge_applicable(base, ours, theirs) {
        return merge_attributes(base, ours, theirs, doms, identities);
    }

    if variant_options_equal(
        ours,
        ValueSource::Ours,
        base,
        ValueSource::Base,
        identities,
        doms,
    ) && variant_options_equal(
        theirs,
        ValueSource::Theirs,
        base,
        ValueSource::Base,
        identities,
        doms,
    ) {
        return keep_or_delete(base, ValueSource::Base);
    }
    if variant_options_equal(
        ours,
        ValueSource::Ours,
        base,
        ValueSource::Base,
        identities,
        doms,
    ) {
        return keep_or_delete(theirs, ValueSource::Theirs);
    }
    if variant_options_equal(
        theirs,
        ValueSource::Theirs,
        base,
        ValueSource::Base,
        identities,
        doms,
    ) {
        return keep_or_delete(ours, ValueSource::Ours);
    }
    if variant_options_equal(
        ours,
        ValueSource::Ours,
        theirs,
        ValueSource::Theirs,
        identities,
        doms,
    ) {
        return keep_or_delete(ours, ValueSource::Ours);
    }

    // A `UniqueId` that diverges three ways on an instance we have already
    // matched is regenerated identity metadata, not a real edit: Studio rewrites
    // these on some instances (e.g. Welds) when a place is opened, so each side
    // can carry a different value for the same instance. The exact value is
    // meaningless as long as it stays unique across the tree, which
    // `detect_unique_id_collisions` enforces after the merge. Resolve it
    // deterministically toward the base value (minimizing churn) rather than
    // surfacing a conflict the user cannot meaningfully resolve.
    if key == ustr("UniqueId") {
        let (value, source) = if base.is_some() {
            (base, ValueSource::Base)
        } else if ours.is_some() {
            (ours, ValueSource::Ours)
        } else {
            (theirs, ValueSource::Theirs)
        };
        return keep_or_delete(value, source);
    }

    PropertyMerge::Conflict
}

fn keep_or_delete(value: Option<&Variant>, source: ValueSource) -> PropertyMerge {
    match value {
        Some(value) => PropertyMerge::Keep(MergedProperty {
            value: value.clone(),
            source,
        }),
        None => PropertyMerge::Delete,
    }
}

fn attributes_merge_applicable(
    base: Option<&Variant>,
    ours: Option<&Variant>,
    theirs: Option<&Variant>,
) -> bool {
    let present = [base, ours, theirs]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    present.len() >= 2
        && present
            .iter()
            .all(|value| matches!(value, Variant::Attributes(_)))
}

fn merge_attributes(
    base: Option<&Variant>,
    ours: Option<&Variant>,
    theirs: Option<&Variant>,
    doms: &SemanticInputs<'_>,
    identities: &IdentitySet,
) -> PropertyMerge {
    let base = match base {
        Some(Variant::Attributes(value)) => Some(value),
        None => None,
        _ => return PropertyMerge::Conflict,
    };
    let ours = match ours {
        Some(Variant::Attributes(value)) => Some(value),
        None => None,
        _ => return PropertyMerge::Conflict,
    };
    let theirs = match theirs {
        Some(Variant::Attributes(value)) => Some(value),
        None => None,
        _ => return PropertyMerge::Conflict,
    };

    let keys: BTreeSet<_> = base
        .into_iter()
        .flat_map(|attrs| attrs.iter().map(|(key, _)| key.clone()))
        .chain(
            ours.into_iter()
                .flat_map(|attrs| attrs.iter().map(|(key, _)| key.clone())),
        )
        .chain(
            theirs
                .into_iter()
                .flat_map(|attrs| attrs.iter().map(|(key, _)| key.clone())),
        )
        .collect();

    let mut merged = Attributes::new();
    for key in keys {
        let base_value = base.and_then(|attrs| attrs.get(key.as_str()));
        let ours_value = ours.and_then(|attrs| attrs.get(key.as_str()));
        let theirs_value = theirs.and_then(|attrs| attrs.get(key.as_str()));
        match merge_property_value(
            ustr("Attributes"),
            base_value,
            ours_value,
            theirs_value,
            doms,
            identities,
        ) {
            PropertyMerge::Keep(property) => {
                merged.insert(key, property.value);
            }
            PropertyMerge::Delete => {}
            PropertyMerge::Conflict => return PropertyMerge::Conflict,
        }
    }

    // Preserve the `Attributes` property even when the merge empties it: every
    // side that reaches here carried the property, and an empty map is a
    // meaningful value to keep rather than silently drop.
    PropertyMerge::Keep(MergedProperty {
        value: Variant::Attributes(merged),
        source: ValueSource::Merged,
    })
}

fn merge_scalar<T>(base: Option<T>, ours: Option<T>, theirs: Option<T>) -> Option<T>
where
    T: Clone + PartialEq,
{
    if ours == base && theirs == base {
        return base;
    }
    if ours == base {
        return theirs;
    }
    if theirs == base {
        return ours;
    }
    if ours == theirs {
        return ours;
    }
    None
}

fn merge_optional_scalar<T>(
    base: Option<T>,
    ours: Option<T>,
    theirs: Option<T>,
) -> Result<Option<T>, ()>
where
    T: Clone + PartialEq,
{
    if ours == base && theirs == base {
        return Ok(base);
    }
    if ours == base {
        return Ok(theirs);
    }
    if theirs == base {
        return Ok(ours);
    }
    if ours == theirs {
        return Ok(ours);
    }
    Err(())
}

fn conflict_subject<'a>(
    entry: &MergeEntry,
    base: &'a SemanticDom,
    ours: &'a SemanticDom,
    theirs: &'a SemanticDom,
) -> (&'a SemanticDom, NodeId) {
    if let Some(id) = entry.ours {
        (ours, id)
    } else if let Some(id) = entry.theirs {
        (theirs, id)
    } else {
        (base, entry.base.expect("conflict entry has no source node"))
    }
}

/// Render a parent merge identity as a human-readable path for conflict output.
fn parent_label(
    parent: MergeNodeId,
    identities: &IdentitySet,
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
) -> String {
    let entry = match identities.entries.get(&parent) {
        Some(entry) => entry,
        None => return format!("{parent:?}"),
    };
    let (dom, id) = conflict_subject(entry, base, ours, theirs);
    dom.path(id)
}

fn node_conflict(
    kind: ConflictKind,
    dom: &SemanticDom,
    id: NodeId,
    property: Option<&str>,
    base: Option<impl Into<String>>,
    ours: Option<impl Into<String>>,
    theirs: Option<impl Into<String>>,
) -> Conflict {
    let node = dom.node(id);
    Conflict {
        kind,
        path: dom.path(id),
        class: node.class.to_string(),
        name: node.name.clone(),
        property: property.map(str::to_owned),
        base: base.map(|value| DisplayValue::new(value.into())),
        ours: ours.map(|value| DisplayValue::new(value.into())),
        theirs: theirs.map(|value| DisplayValue::new(value.into())),
    }
}

fn choose_referent(entry: &MergeEntry, doms: &SemanticInputs<'_>, used: &mut HashSet<Ref>) -> Ref {
    let preferred = entry
        .ours
        .map(|id| doms.ours.node(id).source_ref)
        .or_else(|| entry.theirs.map(|id| doms.theirs.node(id).source_ref))
        .or_else(|| entry.base.map(|id| doms.base.node(id).source_ref));

    let mut referent = preferred.unwrap_or_else(Ref::new);
    while referent.is_none() || used.contains(&referent) {
        referent = Ref::new();
    }
    used.insert(referent);
    referent
}

pub(crate) fn assign_child_order(
    graph: &mut MergedGraph,
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    identities: &IdentitySet,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) {
    let ids: Vec<_> = graph.nodes.keys().copied().collect();
    for id in ids {
        let Some(node) = graph.nodes.get(&id).cloned() else {
            continue;
        };
        let entry = identities.entries.get(&id).unwrap();
        let base_seq = entry
            .base
            .map(|parent| child_merge_ids(base, parent, ValueSource::Base, identities, id, graph))
            .unwrap_or_default();
        let ours_seq = entry
            .ours
            .map(|parent| child_merge_ids(ours, parent, ValueSource::Ours, identities, id, graph))
            .unwrap_or_default();
        let theirs_seq = entry
            .theirs
            .map(|parent| {
                child_merge_ids(theirs, parent, ValueSource::Theirs, identities, id, graph)
            })
            .unwrap_or_default();

        let merged = merge_child_sequence(&base_seq, &ours_seq, &theirs_seq);
        let mut children = match merged {
            ChildOrderMerge::Clean(children) => children,
            ChildOrderMerge::Conflict => {
                let (dom, node_id) = conflict_subject(entry, base, ours, theirs);
                let path = dom.path(node_id);
                match resolutions.lookup(&ConflictKind::ChildOrder, &path, None) {
                    Some(side) => pick(side, &base_seq, &ours_seq, &theirs_seq).clone(),
                    None => {
                        conflicts.push(node_conflict(
                            ConflictKind::ChildOrder,
                            dom,
                            node_id,
                            None,
                            Some(child_order_label(&base_seq, graph)),
                            Some(child_order_label(&ours_seq, graph)),
                            Some(child_order_label(&theirs_seq, graph)),
                        ));
                        continue;
                    }
                }
            }
        };

        // A node kept by a resolved delete/modify survives in the graph but no
        // side's sequence places it; append any such surviving children so they
        // are not orphaned out of the output.
        append_surviving_children(graph, id, &mut children);
        if let Some(node) = graph.nodes.get_mut(&id) {
            node.children = children;
        }
        let _ = node;
    }
}

/// Append graph children of `parent` that the merged sequence omitted, keeping
/// them in graph (identity) order for determinism.
fn append_surviving_children(
    graph: &MergedGraph,
    parent: MergeNodeId,
    children: &mut Vec<MergeNodeId>,
) {
    let present: HashSet<MergeNodeId> = children.iter().copied().collect();
    for (&id, node) in &graph.nodes {
        if node.parent == Some(parent) && !present.contains(&id) {
            children.push(id);
        }
    }
}

/// Render a child sequence as a readable list of child names for conflict output.
fn child_order_label(seq: &[MergeNodeId], graph: &MergedGraph) -> String {
    let names: Vec<&str> = seq
        .iter()
        .map(|id| {
            graph
                .nodes
                .get(id)
                .map(|node| node.name.as_str())
                .unwrap_or("<unknown>")
        })
        .collect();
    format!("[{}]", names.join(", "))
}

enum ChildOrderMerge {
    Clean(Vec<MergeNodeId>),
    Conflict,
}

fn merge_child_sequence(
    base: &[MergeNodeId],
    ours: &[MergeNodeId],
    theirs: &[MergeNodeId],
) -> ChildOrderMerge {
    if ours == base && theirs == base {
        return ChildOrderMerge::Clean(base.to_vec());
    }
    if ours == base {
        return ChildOrderMerge::Clean(theirs.to_vec());
    }
    if theirs == base || ours == theirs {
        return ChildOrderMerge::Clean(ours.to_vec());
    }
    if preserves_base_order(base, ours) && preserves_base_order(base, theirs) {
        return ChildOrderMerge::Clean(merge_insertions(base, ours, theirs));
    }
    ChildOrderMerge::Conflict
}

fn preserves_base_order(base: &[MergeNodeId], side: &[MergeNodeId]) -> bool {
    let base_set: HashSet<_> = base.iter().copied().collect();
    let projected: Vec<_> = side
        .iter()
        .copied()
        .filter(|id| base_set.contains(id))
        .collect();
    let expected: Vec<_> = base
        .iter()
        .copied()
        .filter(|id| side.contains(id))
        .collect();
    projected == expected
}

fn merge_insertions(
    base: &[MergeNodeId],
    ours: &[MergeNodeId],
    theirs: &[MergeNodeId],
) -> Vec<MergeNodeId> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut ours_pos = 0;
    let mut theirs_pos = 0;

    for base_child in base {
        append_until_anchor(
            ours,
            &mut ours_pos,
            Some(*base_child),
            &mut result,
            &mut seen,
        );
        append_until_anchor(
            theirs,
            &mut theirs_pos,
            Some(*base_child),
            &mut result,
            &mut seen,
        );
        if seen.insert(*base_child) {
            result.push(*base_child);
        }
        if ours.get(ours_pos) == Some(base_child) {
            ours_pos += 1;
        }
        if theirs.get(theirs_pos) == Some(base_child) {
            theirs_pos += 1;
        }
    }

    append_until_anchor(ours, &mut ours_pos, None, &mut result, &mut seen);
    append_until_anchor(theirs, &mut theirs_pos, None, &mut result, &mut seen);
    result
}

fn append_until_anchor(
    side: &[MergeNodeId],
    pos: &mut usize,
    anchor: Option<MergeNodeId>,
    result: &mut Vec<MergeNodeId>,
    seen: &mut HashSet<MergeNodeId>,
) {
    while *pos < side.len() && Some(side[*pos]) != anchor {
        let value = side[*pos];
        if seen.insert(value) {
            result.push(value);
        }
        *pos += 1;
    }
}

/// Defensive guard against two merged instances sharing a `UniqueId`. In
/// practice `WeakDom` regenerates colliding ids on every decode and build, and
/// same-`UniqueId` additions across sides are unified by identity matching, so
/// this rarely fires — but the merged graph is assembled independently of those
/// guarantees, so the check is kept.
pub(crate) fn detect_unique_id_collisions(
    graph: &mut MergedGraph,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) {
    let mut seen: HashMap<UniqueId, MergeNodeId> = HashMap::new();
    let mut clear: Vec<MergeNodeId> = Vec::new();
    for id in graph.nodes.keys().copied().collect::<Vec<_>>() {
        let Some(Variant::UniqueId(unique_id)) = graph.nodes[&id]
            .properties
            .get(&ustr("UniqueId"))
            .map(|property| &property.value)
        else {
            continue;
        };
        let unique_id = *unique_id;
        if unique_id.is_nil() {
            continue;
        }
        let Some(&first_id) = seen.get(&unique_id) else {
            seen.insert(unique_id, id);
            continue;
        };
        let path = graph_path(graph, id);
        // Resolving a UniqueId collision drops the duplicate id (the side choice
        // is not meaningful for this kind); the first holder keeps it.
        if resolutions
            .lookup(&ConflictKind::UniqueIdCollision, &path, Some("UniqueId"))
            .is_some()
        {
            clear.push(id);
            continue;
        }
        let first_path = graph_path(graph, first_id);
        let node = &graph.nodes[&id];
        conflicts.push(Conflict {
            kind: ConflictKind::UniqueIdCollision,
            path: path.clone(),
            class: node.class.to_string(),
            name: node.name.clone(),
            property: Some("UniqueId".to_owned()),
            base: Some(DisplayValue::new(format!("also assigned to {first_path}"))),
            ours: Some(DisplayValue::new(unique_id.to_string())),
            theirs: None,
        });
    }
    for id in clear {
        if let Some(node) = graph.nodes.get_mut(&id) {
            node.properties.remove(&ustr("UniqueId"));
        }
    }
}

/// Reconstruct a Roblox-style dotted path for a node in the merged graph.
fn graph_path(graph: &MergedGraph, id: MergeNodeId) -> String {
    if id == graph.root {
        return "<root>".to_owned();
    }
    let mut parts = Vec::new();
    let mut current = Some(id);
    while let Some(node_id) = current {
        if node_id == graph.root {
            break;
        }
        let Some(node) = graph.nodes.get(&node_id) else {
            break;
        };
        parts.push(node.name.clone());
        current = node.parent;
    }
    parts.reverse();
    parts.join(".")
}

/// Detect references that survive into the merged graph but point at an
/// instance that did not — i.e. the target was deleted on the winning side.
/// Writing such a reference would produce a dangling referent, so it is
/// surfaced as a [`ConflictKind::RefTarget`] conflict instead.
///
/// Detection is keyed on the source referents of dropped identities. When the
/// three sides derive from a common base (the version-control case) referents
/// are stable across files, so a property still holding a dropped instance's
/// referent — even one written by the side that performed the deletion — is
/// recognized as dangling.
pub(crate) fn detect_ref_targets(
    graph: &mut MergedGraph,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) {
    let deleted = dropped_referents(graph, identities, doms);
    if deleted.is_empty() {
        return;
    }

    let mut drop_properties: Vec<(MergeNodeId, Ustr)> = Vec::new();
    for (&id, node) in &graph.nodes {
        for (&key, property) in &node.properties {
            let mut referents = Vec::new();
            collect_ref_targets(&property.value, &mut referents);
            for referent in referents {
                let Some(&target) = deleted.get(&referent) else {
                    // Either a live reference or an external one; preserved as-is.
                    continue;
                };
                let path = graph_path(graph, id);
                // Resolving a RefTarget drops the dangling reference (the side
                // choice is not meaningful for this kind).
                if resolutions
                    .lookup(&ConflictKind::RefTarget, &path, Some(key.as_str()))
                    .is_some()
                {
                    drop_properties.push((id, key));
                    break;
                }
                let target_path = identities
                    .entries
                    .get(&target)
                    .map(|entry| deleted_identity_path(entry, doms))
                    .unwrap_or_else(|| format!("{referent}"));
                conflicts.push(Conflict {
                    kind: ConflictKind::RefTarget,
                    path,
                    class: node.class.to_string(),
                    name: node.name.clone(),
                    property: Some(key.to_string()),
                    base: Some(DisplayValue::new(format!(
                        "references {target_path}, which was deleted in the merge"
                    ))),
                    ours: None,
                    theirs: None,
                });
            }
        }
    }
    for (id, key) in drop_properties {
        if let Some(node) = graph.nodes.get_mut(&id) {
            node.properties.remove(&key);
        }
    }
}

fn collect_ref_targets(value: &Variant, out: &mut Vec<Ref>) {
    match value {
        Variant::Ref(referent) => out.push(*referent),
        Variant::Content(content) => {
            if let Some(referent) = content.as_object() {
                out.push(referent);
            }
        }
        Variant::Attributes(attributes) => {
            for (_, value) in attributes {
                collect_ref_targets(value, out);
            }
        }
        _ => {}
    }
}

/// Path of a dropped identity, taken from whichever side still described it.
fn deleted_identity_path(entry: &MergeEntry, doms: &SemanticInputs<'_>) -> String {
    if let Some(id) = entry.base {
        doms.base.path(id)
    } else if let Some(id) = entry.ours {
        doms.ours.path(id)
    } else if let Some(id) = entry.theirs {
        doms.theirs.path(id)
    } else {
        "<unknown>".to_owned()
    }
}

/// Map every source referent of a dropped identity (one absent from the merged
/// graph) back to that identity, across all three sides.
fn dropped_referents(
    graph: &MergedGraph,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> HashMap<Ref, MergeNodeId> {
    let mut deleted = HashMap::new();
    for (&merge_id, entry) in &identities.entries {
        if graph.nodes.contains_key(&merge_id) {
            continue;
        }
        if let Some(base_id) = entry.base {
            deleted.insert(doms.base.node(base_id).source_ref, merge_id);
        }
        if let Some(ours_id) = entry.ours {
            deleted.insert(doms.ours.node(ours_id).source_ref, merge_id);
        }
        if let Some(theirs_id) = entry.theirs {
            deleted.insert(doms.theirs.node(theirs_id).source_ref, merge_id);
        }
    }
    deleted
}

/// The single referent a property points at, if it is a non-nil `Ref` or an
/// object-valued `Content`.
fn single_ref_target(value: &Variant) -> Option<Ref> {
    match value {
        Variant::Ref(referent) if referent.is_some() => Some(*referent),
        Variant::Content(content) => content.as_object(),
        _ => None,
    }
}

/// Report references that pointed at an instance in the base but resolve to
/// nothing in the merged output because the target was deleted — the
/// complement of [`detect_ref_targets`]. There the surviving (non-deleting)
/// side's reference wins and dangles; here the deleting side's nilled reference
/// wins and the link is silently lost. Intentional repoints (the merged value
/// points at a live instance) are not reported.
pub(crate) fn detect_dropped_references(
    graph: &MergedGraph,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let deleted = dropped_referents(graph, identities, doms);
    if deleted.is_empty() {
        return;
    }

    for (&merge_id, node) in &graph.nodes {
        let Some(entry) = identities.entries.get(&merge_id) else {
            continue;
        };
        let Some(base_id) = entry.base else {
            continue;
        };
        for (&key, base_value) in &doms.base.node(base_id).properties {
            let Some(base_ref) = single_ref_target(base_value) else {
                continue;
            };
            let Some(&target) = deleted.get(&base_ref) else {
                continue;
            };
            // The base referenced a now-dropped instance through `key`. In a
            // clean merge the merged value cannot still point at the dropped
            // target (that path is a RefTarget conflict), so a value present
            // here is an intentional repoint; only a missing/nil value is a
            // silent drop.
            if node
                .properties
                .get(&key)
                .and_then(|property| single_ref_target(&property.value))
                .is_none()
            {
                let target_path = identities
                    .entries
                    .get(&target)
                    .map(|target_entry| deleted_identity_path(target_entry, doms))
                    .unwrap_or_else(|| "<unknown>".to_owned());
                diagnostics.push(dropped_reference_diagnostic(
                    graph_path(graph, merge_id),
                    key.as_str(),
                    &target_path,
                ));
            }
        }
    }
}

/// Record merged properties that the reflection database does not recognize.
/// They round-trip unchanged, but are reported so callers can audit lossy or
/// format-specific behavior at a concrete path.
pub(crate) fn scan_unknown_properties(graph: &MergedGraph, diagnostics: &mut Vec<Diagnostic>) {
    let Ok(database) = rbx_reflection_database::get() else {
        return;
    };
    for (&id, node) in &graph.nodes {
        for &key in node.properties.keys() {
            if is_known_property(database, node.class.as_str(), key.as_str()) {
                continue;
            }
            diagnostics.push(unknown_property_diagnostic(
                graph_path(graph, id),
                node.class.as_str(),
                key.as_str(),
            ));
        }
    }
}

fn is_known_property(
    database: &rbx_reflection::ReflectionDatabase<'_>,
    class: &str,
    property: &str,
) -> bool {
    // These are modeled specially by the binary/XML codecs rather than as
    // reflected properties, so they are always considered known.
    if matches!(property, "Attributes" | "Tags" | "UniqueId") {
        return true;
    }

    let mut current = Some(class);
    while let Some(class_name) = current {
        let Some(descriptor) = database.classes.get(class_name) else {
            return false;
        };
        if descriptor.properties.contains_key(property) {
            return true;
        }
        current = descriptor.superclass.as_deref();
    }
    false
}

pub(crate) fn build_weak_dom(
    graph: &MergedGraph,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> Result<WeakDom, Error> {
    let refs = BuildRefMaps::new(graph, identities, doms);
    let root_builder = build_instance_builder(graph, &refs, graph.root)?;
    Ok(WeakDom::new(root_builder))
}

struct BuildRefMaps {
    base_refs: HashMap<Ref, Ref>,
    ours_refs: HashMap<Ref, Ref>,
    theirs_refs: HashMap<Ref, Ref>,
}

impl BuildRefMaps {
    fn new(graph: &MergedGraph, identities: &IdentitySet, doms: &SemanticInputs<'_>) -> Self {
        let mut base_refs = HashMap::new();
        let mut ours_refs = HashMap::new();
        let mut theirs_refs = HashMap::new();
        for (&merge_id, entry) in &identities.entries {
            if !graph.nodes.contains_key(&merge_id) {
                continue;
            }
            let final_ref = graph.nodes[&merge_id].referent;
            if let Some(base_id) = entry.base {
                base_refs.insert(doms.base.node(base_id).source_ref, final_ref);
            }
            if let Some(ours_id) = entry.ours {
                ours_refs.insert(doms.ours.node(ours_id).source_ref, final_ref);
            }
            if let Some(theirs_id) = entry.theirs {
                theirs_refs.insert(doms.theirs.node(theirs_id).source_ref, final_ref);
            }
        }
        Self {
            base_refs,
            ours_refs,
            theirs_refs,
        }
    }
}

fn build_instance_builder(
    graph: &MergedGraph,
    refs: &BuildRefMaps,
    id: MergeNodeId,
) -> Result<InstanceBuilder, Error> {
    let node = graph
        .nodes
        .get(&id)
        .ok_or_else(|| Error::Internal(format!("merged node {id:?} does not exist")))?;
    let mut builder = InstanceBuilder::new(node.class)
        .with_name(node.name.clone())
        .with_referent(node.referent);
    for (&key, property) in &node.properties {
        let value = rewrite_variant_refs(property.value.clone(), property.source, refs);
        builder.add_property(key, value);
    }
    for &child in &node.children {
        if graph.nodes.contains_key(&child) {
            builder.add_child(build_instance_builder(graph, refs, child)?);
        }
    }
    Ok(builder)
}

fn rewrite_variant_refs(value: Variant, source: ValueSource, refs: &BuildRefMaps) -> Variant {
    match value {
        Variant::Ref(referent) => Variant::Ref(rewrite_ref(referent, source, refs)),
        Variant::Content(content) => {
            if let Some(referent) = content.as_object() {
                Variant::Content(Content::from_referent(rewrite_ref(referent, source, refs)))
            } else {
                Variant::Content(content)
            }
        }
        Variant::Attributes(attributes) => {
            let mut rewritten = Attributes::new();
            for (key, value) in attributes {
                rewritten.insert(key, rewrite_variant_refs(value, source, refs));
            }
            Variant::Attributes(rewritten)
        }
        other => other,
    }
}

fn rewrite_ref(referent: Ref, source: ValueSource, refs: &BuildRefMaps) -> Ref {
    if referent.is_none() {
        return referent;
    }

    match source {
        ValueSource::Base => refs.base_refs.get(&referent).copied().unwrap_or(referent),
        ValueSource::Ours => refs.ours_refs.get(&referent).copied().unwrap_or(referent),
        ValueSource::Theirs => refs.theirs_refs.get(&referent).copied().unwrap_or(referent),
        ValueSource::Merged => referent,
    }
}

#[cfg(test)]
mod unique_id_tests {
    use super::*;
    use crate::resolve::{Resolutions, Side};
    use rbx_types::UniqueId;

    /// A root with two children that share one UniqueId — the collision the
    /// public API can't produce (WeakDom regenerates colliding ids on decode).
    fn graph_with_duplicate_uid() -> MergedGraph {
        let uid = Variant::UniqueId(UniqueId::new(1, 1, 1));
        let mut nodes = IndexMap::new();
        nodes.insert(
            MergeNodeId(0),
            MergedInstance {
                class: ustr("DataModel"),
                name: "DataModel".to_owned(),
                parent: None,
                children: vec![MergeNodeId(1), MergeNodeId(2)],
                properties: BTreeMap::new(),
                referent: Ref::new(),
            },
        );
        for index in [1usize, 2] {
            let mut properties = BTreeMap::new();
            properties.insert(
                ustr("UniqueId"),
                MergedProperty {
                    value: uid.clone(),
                    source: ValueSource::Base,
                },
            );
            nodes.insert(
                MergeNodeId(index),
                MergedInstance {
                    class: ustr("Folder"),
                    name: format!("F{index}"),
                    parent: Some(MergeNodeId(0)),
                    children: vec![],
                    properties,
                    referent: Ref::new(),
                },
            );
        }
        MergedGraph {
            root: MergeNodeId(0),
            nodes,
        }
    }

    #[test]
    fn collision_reported_without_resolution() {
        let mut graph = graph_with_duplicate_uid();
        let mut conflicts = Vec::new();
        detect_unique_id_collisions(&mut graph, &Resolutions::none(), &mut conflicts);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, ConflictKind::UniqueIdCollision);
    }

    #[test]
    fn collision_resolved_drops_the_duplicate_id() {
        let mut graph = graph_with_duplicate_uid();
        let mut conflicts = Vec::new();
        detect_unique_id_collisions(&mut graph, &Resolutions::take(Side::Ours), &mut conflicts);
        assert!(conflicts.is_empty());
        // The first holder keeps its UniqueId; the duplicate loses it.
        assert!(
            graph.nodes[&MergeNodeId(1)]
                .properties
                .contains_key(&ustr("UniqueId"))
        );
        assert!(
            !graph.nodes[&MergeNodeId(2)]
                .properties
                .contains_key(&ustr("UniqueId"))
        );
    }
}
