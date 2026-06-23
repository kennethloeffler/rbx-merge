//! Cross-side instance identity matching: deciding which base/ours/theirs
//! instances are "the same" instance for the purposes of a three-way merge.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use rbx_reflection::ClassTag;
use rbx_types::{Ref, UniqueId};
use ustr::Ustr;

use crate::diagnostics::{
    Diagnostic, ambiguous_identity_diagnostic, positional_identity_diagnostic,
    renamed_instance_diagnostic,
};
use crate::semantic::{NodeId, SemanticDom, SemanticInputs, ValueSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct MergeNodeId(pub(crate) usize);

#[derive(Debug, Default)]
pub(crate) struct IdentitySet {
    entries: IndexMap<MergeNodeId, MergeEntry>,
    base_to_merge: HashMap<NodeId, MergeNodeId>,
    ours_to_merge: HashMap<NodeId, MergeNodeId>,
    theirs_to_merge: HashMap<NodeId, MergeNodeId>,
}

#[derive(Debug, Clone)]
pub(crate) struct MergeEntry {
    pub(crate) base: Option<NodeId>,
    pub(crate) ours: Option<NodeId>,
    pub(crate) theirs: Option<NodeId>,
}

impl IdentitySet {
    fn insert(
        &mut self,
        base: Option<NodeId>,
        ours: Option<NodeId>,
        theirs: Option<NodeId>,
    ) -> MergeNodeId {
        let id = MergeNodeId(self.entries.len());
        self.entries.insert(id, MergeEntry { base, ours, theirs });
        if let Some(node) = base {
            self.base_to_merge.insert(node, id);
        }
        if let Some(node) = ours {
            self.ours_to_merge.insert(node, id);
        }
        if let Some(node) = theirs {
            self.theirs_to_merge.insert(node, id);
        }
        id
    }

    fn set_theirs(&mut self, id: MergeNodeId, theirs: NodeId) {
        self.entries.get_mut(&id).unwrap().theirs = Some(theirs);
        self.theirs_to_merge.insert(theirs, id);
    }

    /// Every merge identity, in insertion order, paired with its per-side nodes.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (MergeNodeId, &MergeEntry)> + '_ {
        self.entries.iter().map(|(&id, entry)| (id, entry))
    }

    /// The per-side nodes for one merge identity, if it exists.
    pub(crate) fn entry(&self, id: MergeNodeId) -> Option<&MergeEntry> {
        self.entries.get(&id)
    }

    pub(crate) fn lookup(&self, source: ValueSource, node: NodeId) -> Option<MergeNodeId> {
        match source {
            ValueSource::Base => self.base_to_merge.get(&node).copied(),
            ValueSource::Ours => self.ours_to_merge.get(&node).copied(),
            ValueSource::Theirs => self.theirs_to_merge.get(&node).copied(),
            ValueSource::Merged => None,
        }
    }

    pub(crate) fn resolve_ref(
        &self,
        source: ValueSource,
        referent: Ref,
        doms: &SemanticInputs<'_>,
    ) -> Option<MergeNodeId> {
        if referent.is_none() {
            return None;
        }
        let node = match source {
            ValueSource::Base => doms.base.node_for_ref(referent),
            ValueSource::Ours => doms.ours.node_for_ref(referent),
            ValueSource::Theirs => doms.theirs.node_for_ref(referent),
            ValueSource::Merged => None,
        }?;
        self.lookup(source, node)
    }
}

pub(crate) fn build_identities(
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
) -> (IdentitySet, Vec<Diagnostic>) {
    let base_to_ours = match_base_to_side(base, ours);
    let base_to_theirs = match_base_to_side(base, theirs);

    let mut identities = IdentitySet::default();
    let mut diagnostics = Vec::new();
    for base_id in base.node_ids() {
        identities.insert(
            Some(base_id),
            base_to_ours.map.get(&base_id).copied(),
            base_to_theirs.map.get(&base_id).copied(),
        );
    }

    emit_heuristic_diagnostics(base, &base_to_ours, &base_to_theirs, &mut diagnostics);

    for ours_id in ours.node_ids() {
        if identities.ours_to_merge.contains_key(&ours_id) {
            continue;
        }
        identities.insert(None, Some(ours_id), None);
    }

    for theirs_id in theirs.node_ids() {
        if identities.theirs_to_merge.contains_key(&theirs_id) {
            continue;
        }
        match find_added_match(&identities, ours, theirs, theirs_id) {
            AddedMatch::Unique(candidate) => identities.set_theirs(candidate, theirs_id),
            AddedMatch::Ambiguous => {
                diagnostics.push(ambiguous_identity_diagnostic(theirs.path(theirs_id)));
                identities.insert(None, None, Some(theirs_id));
            }
            AddedMatch::None => {
                identities.insert(None, None, Some(theirs_id));
            }
        }
    }

    (identities, diagnostics)
}

/// Result of trying to pair a `theirs`-side addition with an `ours`-side
/// addition.
enum AddedMatch {
    /// Exactly one candidate; the two additions are the same instance.
    Unique(MergeNodeId),
    /// More than one candidate; matching is declined to stay deterministic.
    Ambiguous,
    /// No candidate; the addition is distinct.
    None,
}

/// Minimum structural similarity for two differently-named instances to be
/// treated as a rename rather than an independent delete and add. Set high: a
/// pure rename (only the name changed) scores 1.0, while a delete plus add of
/// merely similar instances scores lower, so the two are not confused.
const RENAME_SIMILARITY_THRESHOLD: f64 = 0.8;

/// The matching of base instances to one side, plus the heuristic pairings
/// (positional, rename) made along the way so they can be reported.
struct SideMatch {
    map: HashMap<NodeId, NodeId>,
    positional: Vec<PositionalMatch>,
    renames: Vec<RenameMatch>,
}

struct PositionalMatch {
    base_parent: NodeId,
    class: Ustr,
    name: String,
    count: usize,
}

struct RenameMatch {
    base_id: NodeId,
    class: Ustr,
    from: String,
    to: String,
}

/// Properties ignored when measuring how similar two instances' *content* is.
///
/// These carry identity metadata, not semantic content, so they must not count
/// toward a content comparison. `UniqueId` in particular is regenerated by
/// Studio, so counting it would penalize exactly the regenerated instances a
/// similarity metric is meant to recover. This is the content side of the
/// split: `UniqueId` stays a first-class *matching key* (see `unique_id_index`);
/// only metrics that ask "did the content change?" ignore it.
fn is_volatile_property(key: Ustr) -> bool {
    key.as_str() == "UniqueId"
}

/// Structural similarity of two same-class instances in `[0, 1]`: the fraction
/// of (non-volatile) properties whose values match, plus a point for an equal
/// child count, normalized so identical content (only the name differing) scores
/// `1.0`. Volatile identity metadata (see `is_volatile_property`) is excluded so
/// a regenerated `UniqueId` does not drag the score down.
fn rename_similarity(
    base: &SemanticDom,
    base_id: NodeId,
    side: &SemanticDom,
    side_id: NodeId,
) -> f64 {
    let base_node = base.node(base_id);
    let side_node = side.node(side_id);

    let keys: std::collections::BTreeSet<Ustr> = base_node
        .properties
        .keys()
        .chain(side_node.properties.keys())
        .copied()
        .filter(|key| !is_volatile_property(*key))
        .collect();
    let mut equal = 0usize;
    for key in &keys {
        if let (Some(a), Some(b)) = (base_node.properties.get(key), side_node.properties.get(key))
            && a == b
        {
            equal += 1;
        }
    }

    let children_match = (base_node.children.len() == side_node.children.len()) as usize as f64;
    (equal as f64 + children_match) / (keys.len() as f64 + 1.0)
}

fn match_base_to_side(base: &SemanticDom, side: &SemanticDom) -> SideMatch {
    let mut result = HashMap::new();
    let mut used_side = HashSet::new();
    let mut positional = Vec::new();
    let mut renames = Vec::new();
    result.insert(base.root(), side.root());
    used_side.insert(side.root());

    // UniqueId is authoritative: it identifies an instance across renames,
    // moves, and same-name collisions. WeakDom guarantees uniqueness per file,
    // so a shared UniqueId is an exact match. This runs first, so positional
    // matching below only ever sees instances that lack a UniqueId.
    //
    // TODO: UniqueId is a strong signal that an instance has the same semantic
    // identity, but not the last word. Studio regenerates it on some instances
    // (e.g. Welds) in some circumstances, so the "same" instance can have
    // different ids across sides. Single such instances already match
    // structurally (and Lever 1 keeps their divergent UniqueId from
    // conflicting), but ambiguous groups — several same-class, same-name
    // siblings whose ids all regenerated — fall to positional pairing below,
    // which pairs by sibling order and can mismatch after a Studio rebuild. Add
    // property similarity as an additional matching signal to pair these by
    // content. `rename_similarity` already excludes volatile identity metadata
    // (see `is_volatile_property`) so a regenerated UniqueId does not penalize
    // the score. The remaining wrinkle: instances like Welds are defined by Ref
    // properties (Part0/Part1), and `rename_similarity` compares raw Variants,
    // so cross-file Refs never count as equal — the metric must become
    // identity-aware for Refs (resolving through the match map, which means
    // ordering so referenced instances are matched first).
    let base_unique_ids = unique_id_index(base);
    let side_unique_ids = unique_id_index(side);
    for (unique_id, base_nodes) in base_unique_ids {
        let Some(side_nodes) = side_unique_ids.get(&unique_id) else {
            continue;
        };
        if base_nodes.len() == 1 && side_nodes.len() == 1 {
            let base_id = base_nodes[0];
            let side_id = side_nodes[0];
            if !result.contains_key(&base_id) && used_side.insert(side_id) {
                result.insert(base_id, side_id);
            }
        }
    }

    match_services(base, side, &mut result, &mut used_side);

    loop {
        let mut changed = false;
        let parent_pairs: Vec<_> = result
            .iter()
            .map(|(base_id, side_id)| (*base_id, *side_id))
            .collect();
        for (base_parent, side_parent) in parent_pairs {
            let base_groups =
                unmatched_children_grouped(base, base_parent, |child| result.contains_key(&child));
            let side_groups =
                unmatched_children_grouped(side, side_parent, |child| used_side.contains(&child));
            for (key, base_list) in &base_groups {
                let Some(side_list) = side_groups.get(key) else {
                    continue;
                };
                // Pair same-(class, name) siblings by their order under the
                // parent. For unique names each group has one member, so this is
                // the ordinary one-to-one match; for duplicates it recovers
                // identity that would otherwise be lost to delete-plus-add.
                let mut paired = 0;
                for (&base_child, &side_child) in base_list.iter().zip(side_list.iter()) {
                    if result.contains_key(&base_child) || !used_side.insert(side_child) {
                        continue;
                    }
                    result.insert(base_child, side_child);
                    changed = true;
                    paired += 1;
                }
                if paired > 0 && (base_list.len() > 1 || side_list.len() > 1) {
                    positional.push(PositionalMatch {
                        base_parent,
                        class: key.0,
                        name: key.1.clone(),
                        count: paired,
                    });
                }
            }

            // Rename recovery: a child still unmatched after name-based pairing
            // may be the same instance under a new name. Only attempt it when a
            // class has exactly one unmatched candidate on each side (so the
            // pairing is unambiguous) and the two are structurally similar (so a
            // genuine delete and add are not mistaken for a rename).
            let base_by_class =
                unmatched_by_class(base, base_parent, |child| result.contains_key(&child));
            let side_by_class =
                unmatched_by_class(side, side_parent, |child| used_side.contains(&child));
            for (class, base_only) in &base_by_class {
                let [base_child] = base_only[..] else {
                    continue;
                };
                let Some(side_only) = side_by_class.get(class) else {
                    continue;
                };
                let [side_child] = side_only[..] else {
                    continue;
                };
                if rename_similarity(base, base_child, side, side_child)
                    < RENAME_SIMILARITY_THRESHOLD
                {
                    continue;
                }
                if used_side.insert(side_child) {
                    result.insert(base_child, side_child);
                    changed = true;
                    renames.push(RenameMatch {
                        base_id: base_child,
                        class: *class,
                        from: base.node(base_child).name.clone(),
                        to: side.node(side_child).name.clone(),
                    });
                }
            }
        }

        if !changed {
            break;
        }
    }

    SideMatch {
        map: result,
        positional,
        renames,
    }
}

/// Emit deterministic diagnostics for the heuristic pairings (positional and
/// rename) either side relied on, deduplicated and sorted for stable output.
fn emit_heuristic_diagnostics(
    base: &SemanticDom,
    ours: &SideMatch,
    theirs: &SideMatch,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut positional: Vec<(String, String, Ustr, usize)> = Vec::new();
    let mut seen = HashSet::new();
    for matched in ours.positional.iter().chain(theirs.positional.iter()) {
        if seen.insert((matched.base_parent, matched.class, matched.name.clone())) {
            positional.push((
                base.path(matched.base_parent),
                matched.name.clone(),
                matched.class,
                matched.count,
            ));
        }
    }
    positional.sort_by(|a, b| (&a.0, &a.1, a.2.as_str()).cmp(&(&b.0, &b.1, b.2.as_str())));
    for (path, name, class, count) in positional {
        diagnostics.push(positional_identity_diagnostic(
            path,
            class.as_str(),
            &name,
            count,
        ));
    }

    let mut renames: Vec<(String, String, String, Ustr)> = Vec::new();
    let mut seen_renames = HashSet::new();
    for matched in ours.renames.iter().chain(theirs.renames.iter()) {
        if seen_renames.insert((matched.base_id, matched.to.clone())) {
            renames.push((
                base.path(matched.base_id),
                matched.from.clone(),
                matched.to.clone(),
                matched.class,
            ));
        }
    }
    renames.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    for (path, from, to, class) in renames {
        diagnostics.push(renamed_instance_diagnostic(
            path,
            class.as_str(),
            &from,
            &to,
        ));
    }
}

fn unique_id_index(dom: &SemanticDom) -> HashMap<UniqueId, Vec<NodeId>> {
    let mut by_unique_id: HashMap<UniqueId, Vec<NodeId>> = HashMap::new();
    for id in dom.node_ids() {
        if let Some(unique_id) = dom.unique_id(id) {
            by_unique_id.entry(unique_id).or_default().push(id);
        }
    }
    by_unique_id
}

fn match_services(
    base: &SemanticDom,
    side: &SemanticDom,
    result: &mut HashMap<NodeId, NodeId>,
    used_side: &mut HashSet<NodeId>,
) {
    let base_services = service_children_by_class(base, base.root());
    let side_services = service_children_by_class(side, side.root());
    for (class, base_id) in base_services {
        let Some(side_id) = side_services.get(&class).copied() else {
            continue;
        };
        if !result.contains_key(&base_id) && used_side.insert(side_id) {
            result.insert(base_id, side_id);
        }
    }
}

fn service_children_by_class(dom: &SemanticDom, parent: NodeId) -> HashMap<Ustr, NodeId> {
    let mut counts: HashMap<Ustr, Vec<NodeId>> = HashMap::new();
    for &child in &dom.node(parent).children {
        let node = dom.node(child);
        if is_service_class(node.class) {
            counts.entry(node.class).or_default().push(child);
        }
    }
    counts
        .into_iter()
        .filter_map(|(class, nodes)| (nodes.len() == 1).then_some((class, nodes[0])))
        .collect()
}

fn is_service_class(class: Ustr) -> bool {
    let Ok(database) = rbx_reflection_database::get() else {
        return false;
    };
    match database.classes.get(class.as_str()) {
        Some(descriptor) => descriptor.tags.contains(&ClassTag::Service),
        None => false,
    }
}

/// Group a parent's not-yet-matched children by `(class, name)`, preserving
/// sibling order within each group so the groups can be paired by position.
fn unmatched_children_grouped(
    dom: &SemanticDom,
    parent: NodeId,
    is_matched: impl Fn(NodeId) -> bool,
) -> HashMap<(Ustr, String), Vec<NodeId>> {
    let mut grouped: HashMap<(Ustr, String), Vec<NodeId>> = HashMap::new();
    for &child in &dom.node(parent).children {
        if is_matched(child) {
            continue;
        }
        let node = dom.node(child);
        grouped
            .entry((node.class, node.name.clone()))
            .or_default()
            .push(child);
    }
    grouped
}

/// Group a parent's not-yet-matched children by class alone, preserving order.
fn unmatched_by_class(
    dom: &SemanticDom,
    parent: NodeId,
    is_matched: impl Fn(NodeId) -> bool,
) -> HashMap<Ustr, Vec<NodeId>> {
    let mut grouped: HashMap<Ustr, Vec<NodeId>> = HashMap::new();
    for &child in &dom.node(parent).children {
        if is_matched(child) {
            continue;
        }
        grouped
            .entry(dom.node(child).class)
            .or_default()
            .push(child);
    }
    grouped
}

fn find_added_match(
    identities: &IdentitySet,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    theirs_id: NodeId,
) -> AddedMatch {
    let theirs_node = theirs.node(theirs_id);
    let theirs_parent = theirs_node
        .parent
        .and_then(|parent| identities.theirs_to_merge.get(&parent).copied());

    // A shared UniqueId is an exact, intentional pairing: trust it even when the
    // name/class/parent heuristic below would be ambiguous.
    let unique_id = theirs.unique_id(theirs_id);
    if let Some(unique_id) = unique_id {
        let mut matches = identities.entries.iter().filter_map(|(merge_id, entry)| {
            let ours_id = entry.ours?;
            if entry.base.is_none()
                && entry.theirs.is_none()
                && ours.unique_id(ours_id) == Some(unique_id)
            {
                Some(*merge_id)
            } else {
                None
            }
        });

        if let (Some(only), None) = (matches.next(), matches.next()) {
            return AddedMatch::Unique(only);
        }
    }

    let mut matches = identities.entries.iter().filter_map(|(merge_id, entry)| {
        let ours_id = entry.ours?;
        let ours_node = ours.node(ours_id);
        let ours_parent = ours_node
            .parent
            .and_then(|parent| identities.ours_to_merge.get(&parent).copied());
        if entry.base.is_none()
            && entry.theirs.is_none()
            && ours_parent == theirs_parent
            && ours_node.class == theirs_node.class
            && ours_node.name == theirs_node.name
        {
            Some(*merge_id)
        } else {
            None
        }
    });

    match (matches.next(), matches.next()) {
        (Some(first), None) => AddedMatch::Unique(first),
        (Some(_), Some(_)) => AddedMatch::Ambiguous,
        _ => AddedMatch::None,
    }
}
