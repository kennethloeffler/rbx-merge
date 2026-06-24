//! The three-way merge itself: combining matched instances into a single
//! merged graph, resolving child order, and lowering that graph back into a
//! `WeakDom` for encoding.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

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
use crate::semantic::{
    NodeId, SemanticDom, SemanticInputs, SemanticInstance, ValueSource, variant_key,
    variant_options_equal,
};

#[derive(Debug, Clone)]
struct MergedGraph {
    root: MergeNodeId,
    nodes: IndexMap<MergeNodeId, MergedInstance>,
}

#[derive(Debug, Clone)]
struct MergedInstance {
    class: Ustr,
    name: String,
    parent: Option<MergeNodeId>,
    children: Vec<MergeNodeId>,
    properties: BTreeMap<Ustr, MergedProperty>,
    referent: Ref,
}

#[derive(Debug, Clone)]
struct MergedProperty {
    value: Variant,
    source: ValueSource,
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

/// A value held once per side. The recurring shape of the merge: most rules read
/// `base`/`ours`/`theirs` of the same type, combine them, and `pick` one by a
/// resolved `Side`. Bundling them lets those operations travel as a single value
/// instead of three parallel arguments.
#[derive(Clone, Copy)]
struct Sides<T> {
    base: T,
    ours: T,
    theirs: T,
}

impl<T> Sides<T> {
    /// Select the value for a resolved `Side`.
    fn pick(self, side: Side) -> T {
        match side {
            Side::Base => self.base,
            Side::Ours => self.ours,
            Side::Theirs => self.theirs,
        }
    }

    /// Apply `f` to each side, producing a `Sides` of the results.
    fn map<U>(self, mut f: impl FnMut(T) -> U) -> Sides<U> {
        Sides {
            base: f(self.base),
            ours: f(self.ours),
            theirs: f(self.theirs),
        }
    }

    /// Borrow each side, so the values can be `pick`ed or `map`ped without moving.
    fn as_ref(&self) -> Sides<&T> {
        Sides {
            base: &self.base,
            ours: &self.ours,
            theirs: &self.theirs,
        }
    }
}

impl<T: PartialEq> Sides<T> {
    /// Which side the three-way rule resolves to. If both sides equal base, take
    /// base; if only one side changed, take it; if both changed alike, take ours;
    /// if both changed differently, it is a conflict (`Err`).
    fn three_way_side(&self) -> Result<Side, ()> {
        if self.ours == self.base && self.theirs == self.base {
            Ok(Side::Base)
        } else if self.ours == self.base {
            Ok(Side::Theirs)
        } else if self.theirs == self.base || self.ours == self.theirs {
            Ok(Side::Ours)
        } else {
            Err(())
        }
    }

    /// The three-way rule resolved to the winning side's value.
    fn three_way_merge(self) -> Result<T, ()> {
        let side = self.three_way_side()?;
        Ok(self.pick(side))
    }
}

impl<T: PartialEq> Sides<Option<T>> {
    /// The three-way rule where an absent value is itself a legitimate outcome
    /// and a disagreement collapses to `None` rather than a distinguishable
    /// conflict. Used for class and name, where each side either has the node or
    /// does not.
    fn three_way_option_merge(self) -> Option<T> {
        self.three_way_merge().unwrap_or(None)
    }
}

fn side_source(side: Side) -> ValueSource {
    match side {
        Side::Base => ValueSource::Base,
        Side::Ours => ValueSource::Ours,
        Side::Theirs => ValueSource::Theirs,
    }
}

/// The immutable inputs every conflict-resolving stage of the merge reads: the
/// three semantic DOMs, the identity matching between them, and the caller's
/// conflict resolutions. Bundled so they travel as one argument; the mutable
/// `conflicts` sink is threaded separately to avoid borrow conflicts with the
/// identity iteration that drives the merge.
#[derive(Clone, Copy)]
struct MergeCtx<'a> {
    doms: SemanticInputs<'a>,
    identities: &'a IdentitySet,
    resolutions: &'a Resolutions,
}

impl<'a> MergeCtx<'a> {
    /// The per-side source node for a merge entry, absent where that side lacks
    /// the node.
    fn nodes(&self, entry: &MergeEntry) -> Sides<Option<&'a SemanticInstance>> {
        Sides {
            base: entry.base.map(|id| self.doms.base.node(id)),
            ours: entry.ours.map(|id| self.doms.ours.node(id)),
            theirs: entry.theirs.map(|id| self.doms.theirs.node(id)),
        }
    }
}

/// The result of a full three-way merge: either a lowered `WeakDom` ready to
/// encode, or the conflicts that prevented one. Diagnostics are accumulated
/// separately through the caller's sink.
pub(crate) enum MergeOutcome {
    Merged(WeakDom),
    Conflicts(Vec<Conflict>),
}

/// Run the entire three-way merge: build the merged graph, resolve structure
/// and child order, run every conflict/diagnostic pass in order, and lower a
/// clean result back to a `WeakDom`. The pass ordering and the points at which
/// accumulated conflicts abort the merge are internal to this function; callers
/// supply the three sides and a diagnostics sink and receive a [`MergeOutcome`].
pub(crate) fn merge(
    doms: SemanticInputs<'_>,
    identities: &IdentitySet,
    resolutions: &Resolutions,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<MergeOutcome, Error> {
    let ctx = MergeCtx {
        doms,
        identities,
        resolutions,
    };

    let mut conflicts = Vec::new();
    let mut graph = merge_semantic_graph(&ctx, &mut conflicts)?;

    detect_parent_cycles(&ctx, &mut graph, &mut conflicts);
    detect_unique_id_collisions(&mut graph, resolutions, &mut conflicts);
    detect_ref_targets(&ctx, &mut graph, &mut conflicts);
    if !conflicts.is_empty() {
        return Ok(MergeOutcome::Conflicts(conflicts));
    }

    assign_child_order(&ctx, &mut graph, &mut conflicts);
    detect_unique_id_collisions(&mut graph, resolutions, &mut conflicts);
    if !conflicts.is_empty() {
        return Ok(MergeOutcome::Conflicts(conflicts));
    }

    detect_dropped_references(&graph, identities, &doms, diagnostics);
    scan_unknown_properties(&graph, diagnostics);

    let dom = build_weak_dom(&graph, identities, &doms)?;
    Ok(MergeOutcome::Merged(dom))
}

fn merge_semantic_graph(
    ctx: &MergeCtx<'_>,
    conflicts: &mut Vec<Conflict>,
) -> Result<MergedGraph, Error> {
    let root = ctx
        .identities
        .lookup(ValueSource::Base, ctx.doms.base.root())
        .ok_or_else(|| Error::Internal("base root was not assigned a merge identity".to_owned()))?;
    let mut graph = MergedGraph {
        root,
        nodes: IndexMap::new(),
    };
    let mut used_refs = HashSet::new();

    for (merge_id, entry) in ctx.identities.entries() {
        match deletion_decision(ctx, entry, conflicts) {
            NodeDecision::Drop => continue,
            NodeDecision::TakeSide(side) => {
                let instance = materialize_from_side(ctx, entry, side, &mut used_refs);
                graph.nodes.insert(merge_id, instance);
                continue;
            }
            NodeDecision::Merge => {}
        }

        let Some(class) = merge_class(ctx, entry, conflicts) else {
            continue;
        };
        let Some(name) = merge_name(ctx, entry, conflicts) else {
            continue;
        };
        let parent = merge_parent(ctx, entry, conflicts);
        let properties = merge_properties(ctx, entry, conflicts);
        let referent = choose_referent(entry, &ctx.doms, &mut used_refs);

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
    ctx: &MergeCtx<'_>,
    entry: &MergeEntry,
    side: Side,
    used_refs: &mut HashSet<Ref>,
) -> MergedInstance {
    let doms = &ctx.doms;
    let source = side_source(side);
    let node = ctx
        .nodes(entry)
        .pick(side)
        .expect("resolved side has a node");
    let parent = node
        .parent
        .and_then(|parent| ctx.identities.lookup(source, parent));
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
    ctx: &MergeCtx<'_>,
    entry: &MergeEntry,
    conflicts: &mut Vec<Conflict>,
) -> NodeDecision {
    let Some(base_id) = entry.base else {
        return NodeDecision::Merge;
    };

    match (entry.ours, entry.theirs) {
        (None, None) => NodeDecision::Drop,
        (None, Some(theirs_id)) => {
            if side_node_changed_from_base(ctx, base_id, theirs_id, ValueSource::Theirs) {
                resolve_delete_modify(ctx, entry, base_id, "deleted", "modified", conflicts)
            } else {
                NodeDecision::Drop
            }
        }
        (Some(ours_id), None) => {
            if side_node_changed_from_base(ctx, base_id, ours_id, ValueSource::Ours) {
                resolve_delete_modify(ctx, entry, base_id, "modified", "deleted", conflicts)
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
    ctx: &MergeCtx<'_>,
    entry: &MergeEntry,
    base_id: NodeId,
    ours_label: &str,
    theirs_label: &str,
    conflicts: &mut Vec<Conflict>,
) -> NodeDecision {
    match resolve_or_report(
        ConflictKind::DeleteModify,
        ctx.doms.base,
        base_id,
        None,
        Sides {
            base: Some("present in base"),
            ours: Some(ours_label),
            theirs: Some(theirs_label),
        },
        ctx.resolutions,
        conflicts,
    ) {
        Some(side) => match ctx.nodes(entry).pick(side) {
            Some(_) => NodeDecision::TakeSide(side),
            None => NodeDecision::Drop,
        },
        None => NodeDecision::Drop,
    }
}

fn side_node_changed_from_base(
    ctx: &MergeCtx<'_>,
    base_id: NodeId,
    side_id: NodeId,
    side_source: ValueSource,
) -> bool {
    let base = ctx.doms.base;
    let side = match side_source {
        ValueSource::Ours => ctx.doms.ours,
        ValueSource::Theirs => ctx.doms.theirs,
        _ => return true,
    };
    let identities = ctx.identities;

    let base_node = base.node(base_id);
    let side_node = side.node(side_id);
    if base_node.class != side_node.class || base_node.name != side_node.name {
        return true;
    }

    let doms = SemanticInputs {
        base,
        ours: side,
        theirs: side,
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
        .and_then(|parent| identities.lookup(ValueSource::Base, parent));
    let side_parent = side_node
        .parent
        .and_then(|parent| identities.lookup(side_source, parent));
    if base_parent != side_parent {
        return true;
    }

    let base_children: Vec<_> = base_node
        .children
        .iter()
        .filter_map(|child| identities.lookup(ValueSource::Base, *child))
        .collect();
    let side_children: Vec<_> = side_node
        .children
        .iter()
        .filter_map(|child| identities.lookup(side_source, *child))
        .collect();
    base_children != side_children
}

fn merge_class(
    ctx: &MergeCtx<'_>,
    entry: &MergeEntry,
    conflicts: &mut Vec<Conflict>,
) -> Option<Ustr> {
    let classes = ctx.nodes(entry).map(|node| node.map(|node| node.class));
    classes.three_way_option_merge().or_else(|| {
        let (dom, id) = conflict_subject(entry, &ctx.doms);
        let path = dom.path(id);
        if let Some(side) =
            ctx.resolutions
                .lookup(&ConflictKind::InstanceIdentity, &path, Some("ClassName"))
            && let Some(resolved) = classes.pick(side)
        {
            return Some(resolved);
        }
        conflicts.push(node_conflict(
            ConflictKind::InstanceIdentity,
            dom,
            id,
            Some("ClassName"),
            classes.map(|class| class.map(|class| class.to_string())),
        ));
        None
    })
}

fn merge_name(
    ctx: &MergeCtx<'_>,
    entry: &MergeEntry,
    conflicts: &mut Vec<Conflict>,
) -> Option<String> {
    let names = ctx
        .nodes(entry)
        .map(|node| node.map(|node| node.name.clone()));
    names.clone().three_way_option_merge().or_else(|| {
        let (dom, id) = conflict_subject(entry, &ctx.doms);
        let path = dom.path(id);
        if let Some(side) =
            ctx.resolutions
                .lookup(&ConflictKind::InstanceIdentity, &path, Some("Name"))
            && let Some(resolved) = names.clone().pick(side)
        {
            return Some(resolved);
        }
        conflicts.push(node_conflict(
            ConflictKind::InstanceIdentity,
            dom,
            id,
            Some("Name"),
            names,
        ));
        None
    })
}

fn merge_parent(
    ctx: &MergeCtx<'_>,
    entry: &MergeEntry,
    conflicts: &mut Vec<Conflict>,
) -> Option<MergeNodeId> {
    let parents = side_parents(ctx, entry);

    match parents.three_way_merge() {
        Ok(parent) => parent,
        Err(()) => {
            let (dom, id) = conflict_subject(entry, &ctx.doms);
            match resolve_or_report(
                ConflictKind::ParentMove,
                dom,
                id,
                None,
                parents.map(|parent| parent.map(|value| parent_label(ctx, value))),
                ctx.resolutions,
                conflicts,
            ) {
                Some(side) => parents.pick(side),
                None => parents.base,
            }
        }
    }
}

/// Each side's merged-id parent for a node, or `None` where that side lacks the
/// node or its parent. Shared by the per-node parent merge and cycle detection.
fn side_parents(ctx: &MergeCtx<'_>, entry: &MergeEntry) -> Sides<Option<MergeNodeId>> {
    let (doms, identities) = (&ctx.doms, ctx.identities);
    Sides {
        base: entry
            .base
            .and_then(|id| doms.base.node(id).parent)
            .and_then(|id| identities.lookup(ValueSource::Base, id)),
        ours: entry
            .ours
            .and_then(|id| doms.ours.node(id).parent)
            .and_then(|id| identities.lookup(ValueSource::Ours, id)),
        theirs: entry
            .theirs
            .and_then(|id| doms.theirs.node(id).parent)
            .and_then(|id| identities.lookup(ValueSource::Theirs, id)),
    }
}

/// Report (and where possible resolve) instances whose merged parent links form
/// a cycle.
///
/// `merge_parent` resolves each node's parent independently, so two instances
/// each cleanly reparented under the other yield `A -> B -> A` with no per-node
/// conflict — yet that component is unreachable from the root and would be
/// silently dropped from the output (an instance both sides kept, lost without a
/// word). A cycle admits no valid tree, so it is a genuine conflict.
///
/// A resolution breaks it: re-pointing every cycle member's parent to one chosen
/// side reproduces that side's own (acyclic) tree for them, so the loop is gone.
/// Members left on a cycle afterwards — no resolution, or a partial one that did
/// not break the loop — are reported as conflicts.
fn detect_parent_cycles(
    ctx: &MergeCtx<'_>,
    graph: &mut MergedGraph,
    conflicts: &mut Vec<Conflict>,
) {
    let members: Vec<MergeNodeId> = graph
        .nodes
        .keys()
        .copied()
        .filter(|&id| node_is_on_parent_cycle(graph, id))
        .collect();

    // Apply resolutions first. Each member's parent is re-pointed from the
    // original per-side parents (not the graph's current links), so resolving
    // every member to the same side yields that side's acyclic structure.
    for &id in &members {
        let Some(entry) = ctx.identities.entry(id) else {
            continue;
        };
        let (dom, node_id) = conflict_subject(entry, &ctx.doms);
        let path = dom.path(node_id);
        if let Some(side) = ctx
            .resolutions
            .lookup(&ConflictKind::ParentCycle, &path, None)
        {
            let parent = side_parents(ctx, entry).pick(side);
            if let Some(node) = graph.nodes.get_mut(&id) {
                node.parent = parent;
            }
        }
    }

    // Whatever still lies on a cycle is an unresolved conflict.
    for id in members {
        if !node_is_on_parent_cycle(graph, id) {
            continue;
        }
        let Some(entry) = ctx.identities.entry(id) else {
            continue;
        };
        let (dom, node_id) = conflict_subject(entry, &ctx.doms);
        conflicts.push(node_conflict(
            ConflictKind::ParentCycle,
            dom,
            node_id,
            None,
            side_parents(ctx, entry).map(|parent| parent.map(|value| parent_label(ctx, value))),
        ));
    }
}

/// Whether `start`'s parent chain loops back to `start` — i.e. `start` itself
/// lies on a cycle. A node that merely hangs below a cycle (its chain enters a
/// loop that does not include it) returns `false`; it is reachable again once
/// the cycle members are resolved, so it is not itself the conflict.
fn node_is_on_parent_cycle(graph: &MergedGraph, start: MergeNodeId) -> bool {
    let mut visited = HashSet::new();
    let mut current = graph.nodes.get(&start).and_then(|node| node.parent);
    while let Some(id) = current {
        if id == start {
            return true;
        }
        if id == graph.root || !visited.insert(id) {
            return false;
        }
        current = graph.nodes.get(&id).and_then(|node| node.parent);
    }
    false
}

fn merge_properties(
    ctx: &MergeCtx<'_>,
    entry: &MergeEntry,
    conflicts: &mut Vec<Conflict>,
) -> BTreeMap<Ustr, MergedProperty> {
    let doms = &ctx.doms;
    let nodes = ctx.nodes(entry);
    let keys: BTreeSet<_> = [nodes.base, nodes.ours, nodes.theirs]
        .into_iter()
        .flatten()
        .flat_map(|node| node.properties.keys())
        .copied()
        .collect();

    let mut merged = BTreeMap::new();
    for key in keys {
        let values = nodes.map(|node| node.and_then(|node| node.properties.get(&key)));

        match merge_property_value(ctx, key, values) {
            PropertyMerge::Keep(property) => {
                merged.insert(key, property);
            }
            PropertyMerge::Delete => {}
            PropertyMerge::Conflict => {
                let (dom, id) = conflict_subject(entry, doms);
                if let Some(side) = resolve_or_report(
                    ConflictKind::PropertyValue,
                    dom,
                    id,
                    Some(key.as_str()),
                    Sides {
                        base: values
                            .base
                            .map(|value| display_variant(value, ValueSource::Base, doms)),
                        ours: values
                            .ours
                            .map(|value| display_variant(value, ValueSource::Ours, doms)),
                        theirs: values
                            .theirs
                            .map(|value| display_variant(value, ValueSource::Theirs, doms)),
                    },
                    ctx.resolutions,
                    conflicts,
                ) {
                    // The chosen side may not have the property, in which case
                    // resolving to that side means dropping it.
                    if let Some(value) = values.pick(side) {
                        merged.insert(
                            key,
                            MergedProperty {
                                value: value.clone(),
                                source: side_source(side),
                            },
                        );
                    }
                }
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
    ctx: &MergeCtx<'_>,
    key: Ustr,
    values: Sides<Option<&Variant>>,
) -> PropertyMerge {
    let (doms, identities) = (&ctx.doms, ctx.identities);
    let Sides { base, ours, theirs } = values;
    if key == ustr("Attributes") && attributes_merge_applicable(base, ours, theirs) {
        return merge_attributes(ctx, values);
    }

    // Reduce each side to a source-aware key (so refs to the same merged
    // instance compare equal across sides), then apply the three-way rule with
    // plain equality. Keys are computed once per side rather than per comparison.
    let sourced = Sides {
        base: (base, ValueSource::Base),
        ours: (ours, ValueSource::Ours),
        theirs: (theirs, ValueSource::Theirs),
    };
    let keys = sourced
        .map(|(value, source)| value.map(|value| variant_key(value, source, identities, doms)));
    if let Ok(side) = keys.three_way_side() {
        let (value, source) = sourced.pick(side);
        return keep_or_delete(value, source);
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

fn merge_attributes(ctx: &MergeCtx<'_>, values: Sides<Option<&Variant>>) -> PropertyMerge {
    let Sides { base, ours, theirs } = values;
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
        let values = Sides {
            base: base.and_then(|attrs| attrs.get(key.as_str())),
            ours: ours.and_then(|attrs| attrs.get(key.as_str())),
            theirs: theirs.and_then(|attrs| attrs.get(key.as_str())),
        };
        match merge_property_value(ctx, ustr("Attributes"), values) {
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

fn conflict_subject<'a>(
    entry: &MergeEntry,
    doms: &SemanticInputs<'a>,
) -> (&'a SemanticDom, NodeId) {
    if let Some(id) = entry.ours {
        (doms.ours, id)
    } else if let Some(id) = entry.theirs {
        (doms.theirs, id)
    } else {
        (
            doms.base,
            entry.base.expect("conflict entry has no source node"),
        )
    }
}

/// Render a parent merge identity as a human-readable path for conflict output.
fn parent_label(ctx: &MergeCtx<'_>, parent: MergeNodeId) -> String {
    let entry = match ctx.identities.entry(parent) {
        Some(entry) => entry,
        None => return format!("{parent:?}"),
    };
    let (dom, id) = conflict_subject(entry, &ctx.doms);
    dom.path(id)
}

fn node_conflict(
    kind: ConflictKind,
    dom: &SemanticDom,
    id: NodeId,
    property: Option<&str>,
    labels: Sides<Option<impl Into<String>>>,
) -> Conflict {
    let node = dom.node(id);
    let Sides { base, ours, theirs } =
        labels.map(|label| label.map(|value| DisplayValue::new(value.into())));
    Conflict {
        kind,
        path: dom.path(id),
        class: node.class.to_string(),
        name: node.name.clone(),
        property: property.map(str::to_owned),
        base,
        ours,
        theirs,
    }
}

/// Resolve a conflict from the supplied `Resolutions`, or record it. Returns the
/// chosen `Side` when a resolution applies, leaving the caller to act on it (the
/// per-kind fallback and how a chosen side maps to a value differ by site);
/// otherwise pushes a `node_conflict` built from the given per-side display
/// values and returns `None`.
fn resolve_or_report(
    kind: ConflictKind,
    dom: &SemanticDom,
    id: NodeId,
    property: Option<&str>,
    labels: Sides<Option<impl Into<String>>>,
    resolutions: &Resolutions,
    conflicts: &mut Vec<Conflict>,
) -> Option<Side> {
    if let Some(side) = resolutions.lookup(&kind, &dom.path(id), property) {
        return Some(side);
    }
    conflicts.push(node_conflict(kind, dom, id, property, labels));
    None
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

fn assign_child_order(ctx: &MergeCtx<'_>, graph: &mut MergedGraph, conflicts: &mut Vec<Conflict>) {
    let (doms, identities) = (&ctx.doms, ctx.identities);
    let ids: Vec<_> = graph.nodes.keys().copied().collect();

    // Parent links are fixed for the duration of this pass (only `children` is
    // rewritten), so map every parent to its graph children once in insertion
    // order rather than rescanning all nodes per parent in
    // `append_surviving_children` (which made the pass O(N^2) in node count).
    let mut children_by_parent: HashMap<MergeNodeId, Vec<MergeNodeId>> = HashMap::new();
    for (&id, node) in &graph.nodes {
        if let Some(parent) = node.parent {
            children_by_parent.entry(parent).or_default().push(id);
        }
    }

    for id in ids {
        if !graph.nodes.contains_key(&id) {
            continue;
        }
        let entry = identities.entry(id).unwrap();
        let seqs = Sides {
            base: entry.base.map(|parent| {
                child_merge_ids(doms.base, parent, ValueSource::Base, identities, id, graph)
            }),
            ours: entry.ours.map(|parent| {
                child_merge_ids(doms.ours, parent, ValueSource::Ours, identities, id, graph)
            }),
            theirs: entry.theirs.map(|parent| {
                child_merge_ids(
                    doms.theirs,
                    parent,
                    ValueSource::Theirs,
                    identities,
                    id,
                    graph,
                )
            }),
        }
        .map(Option::unwrap_or_default);

        let merged = merge_child_sequence(seqs.as_ref().map(Vec::as_slice));
        let mut children = match merged {
            ChildOrderMerge::Clean(children) => children,
            ChildOrderMerge::Conflict => {
                let (dom, node_id) = conflict_subject(entry, doms);
                match resolve_or_report(
                    ConflictKind::ChildOrder,
                    dom,
                    node_id,
                    None,
                    seqs.as_ref().map(|seq| Some(child_order_label(seq, graph))),
                    ctx.resolutions,
                    conflicts,
                ) {
                    Some(side) => seqs.as_ref().pick(side).clone(),
                    None => continue,
                }
            }
        };

        // A node kept by a resolved delete/modify survives in the graph but no
        // side's sequence places it; append any such surviving children so they
        // are not orphaned out of the output.
        append_surviving_children(children_by_parent.get(&id), &mut children);
        if let Some(node) = graph.nodes.get_mut(&id) {
            node.children = children;
        }
    }
}

/// Append a parent's graph children that the merged sequence omitted, keeping
/// them in graph (identity) order for determinism. `graph_children` is the
/// parent's precomputed child list (in insertion order), or `None` if it has
/// none.
fn append_surviving_children(
    graph_children: Option<&Vec<MergeNodeId>>,
    children: &mut Vec<MergeNodeId>,
) {
    let Some(graph_children) = graph_children else {
        return;
    };
    let present: HashSet<MergeNodeId> = children.iter().copied().collect();
    for &id in graph_children {
        if !present.contains(&id) {
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

fn merge_child_sequence(seqs: Sides<&[MergeNodeId]>) -> ChildOrderMerge {
    let Sides { base, ours, theirs } = seqs;
    if let Ok(seq) = seqs.three_way_merge() {
        return ChildOrderMerge::Clean(seq.to_vec());
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
fn detect_unique_id_collisions(
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
    let mut visited = HashSet::new();
    while let Some(node_id) = current {
        if node_id == graph.root {
            break;
        }
        if !visited.insert(node_id) {
            // The parent chain cycles. The merge can produce one when two
            // instances are independently reparented under each other: neither
            // side's tree has a cycle, but the merged parents do. Such a
            // component is unreachable from the root and so never built into the
            // output, but it still lives in `graph.nodes` and is walked here.
            // Stop rather than loop forever (which would grow `parts` without
            // bound).
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
fn detect_ref_targets(ctx: &MergeCtx<'_>, graph: &mut MergedGraph, conflicts: &mut Vec<Conflict>) {
    let (doms, identities) = (&ctx.doms, ctx.identities);
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
                if ctx
                    .resolutions
                    .lookup(&ConflictKind::RefTarget, &path, Some(key.as_str()))
                    .is_some()
                {
                    drop_properties.push((id, key));
                    break;
                }
                let target_path = identities
                    .entry(target)
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
    for (merge_id, entry) in identities.entries() {
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
fn detect_dropped_references(
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
        let Some(entry) = identities.entry(merge_id) else {
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
                    .entry(target)
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
fn scan_unknown_properties(graph: &MergedGraph, diagnostics: &mut Vec<Diagnostic>) {
    let Ok(database) = rbx_reflection_database::get() else {
        return;
    };
    for (&id, node) in &graph.nodes {
        // Reconstruct the node's path at most once, and only if it actually has
        // an unknown property. Rebuilding it per property walks the ancestry and
        // clones a string for every level each time; a node with many unknown
        // properties (or many nodes that do) turns that into a large amount of
        // redundant allocation.
        let mut path: Option<Arc<str>> = None;
        for &key in node.properties.keys() {
            if is_known_property(database, node.class.as_str(), key.as_str()) {
                continue;
            }
            let path = path.get_or_insert_with(|| Arc::from(graph_path(graph, id)));
            diagnostics.push(unknown_property_diagnostic(
                Arc::clone(path),
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
        current = descriptor.superclass;
    }
    false
}

fn build_weak_dom(
    graph: &MergedGraph,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> Result<WeakDom, Error> {
    let refs = BuildRefMaps::new(graph, identities, doms);
    let mut built = HashSet::new();
    let root_builder = build_instance_builder(graph, &refs, graph.root, &mut built)?;
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
        for (merge_id, entry) in identities.entries() {
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
    built: &mut HashSet<MergeNodeId>,
) -> Result<InstanceBuilder, Error> {
    // The graph is a tree: each node is reachable from the root by exactly one
    // path. A node appears in exactly one parent's `children` because every such
    // sequence is filtered to that parent (see `child_merge_ids`), so building
    // it twice would mean an upstream invariant broke. Fail loudly instead of
    // duplicating a subtree (which compounds into exponential output for nested
    // duplicates).
    if !built.insert(id) {
        return Err(Error::Internal(format!(
            "merged node {id:?} reached from two parents"
        )));
    }
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
            builder.add_child(build_instance_builder(graph, refs, child, built)?);
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
