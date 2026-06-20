//! Cross-side instance identity matching: deciding which base/ours/theirs
//! instances are "the same" instance for the purposes of a three-way merge.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use rbx_reflection::ClassTag;
use rbx_types::{Ref, UniqueId};
use ustr::Ustr;

use crate::diagnostics::{Diagnostic, ambiguous_identity_diagnostic};
use crate::semantic::{NodeId, SemanticDom, SemanticInputs, ValueSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct MergeNodeId(pub(crate) usize);

#[derive(Debug, Default)]
pub(crate) struct IdentitySet {
    pub(crate) entries: IndexMap<MergeNodeId, MergeEntry>,
    pub(crate) base_to_merge: HashMap<NodeId, MergeNodeId>,
    pub(crate) ours_to_merge: HashMap<NodeId, MergeNodeId>,
    pub(crate) theirs_to_merge: HashMap<NodeId, MergeNodeId>,
}

#[derive(Debug, Clone)]
pub(crate) struct MergeEntry {
    pub(crate) base: Option<NodeId>,
    pub(crate) ours: Option<NodeId>,
    pub(crate) theirs: Option<NodeId>,
}

impl IdentitySet {
    pub(crate) fn insert(
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
            ValueSource::Base => doms.base.ref_to_node.get(&referent).copied(),
            ValueSource::Ours => doms.ours.ref_to_node.get(&referent).copied(),
            ValueSource::Theirs => doms.theirs.ref_to_node.get(&referent).copied(),
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
    for (&base_id, _) in &base.nodes {
        identities.insert(
            Some(base_id),
            base_to_ours.get(&base_id).copied(),
            base_to_theirs.get(&base_id).copied(),
        );
    }

    for (&ours_id, _) in &ours.nodes {
        if identities.ours_to_merge.contains_key(&ours_id) {
            continue;
        }
        identities.insert(None, Some(ours_id), None);
    }

    for (&theirs_id, _) in &theirs.nodes {
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

fn match_base_to_side(base: &SemanticDom, side: &SemanticDom) -> HashMap<NodeId, NodeId> {
    let mut result = HashMap::new();
    let mut used_side = HashSet::new();
    result.insert(base.root, side.root);
    used_side.insert(side.root);

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
            let base_candidates =
                unique_unmatched_children_by_class_name(base, base_parent, &result);
            let side_candidates =
                unique_unmatched_side_children_by_class_name(side, side_parent, &used_side);
            for (key, base_child) in base_candidates {
                let Some(side_child) = side_candidates.get(&key).copied() else {
                    continue;
                };
                if result.contains_key(&base_child) || !used_side.insert(side_child) {
                    continue;
                }
                result.insert(base_child, side_child);
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    result
}

fn unique_id_index(dom: &SemanticDom) -> HashMap<UniqueId, Vec<NodeId>> {
    let mut by_unique_id: HashMap<UniqueId, Vec<NodeId>> = HashMap::new();
    for (&id, _) in &dom.nodes {
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
    let base_services = service_children_by_class(base, base.root);
    let side_services = service_children_by_class(side, side.root);
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

fn unique_unmatched_children_by_class_name(
    dom: &SemanticDom,
    parent: NodeId,
    existing: &HashMap<NodeId, NodeId>,
) -> HashMap<(Ustr, String), NodeId> {
    let mut grouped: HashMap<(Ustr, String), Vec<NodeId>> = HashMap::new();
    for &child in &dom.node(parent).children {
        if existing.contains_key(&child) {
            continue;
        }
        let node = dom.node(child);
        grouped
            .entry((node.class, node.name.clone()))
            .or_default()
            .push(child);
    }
    grouped
        .into_iter()
        .filter_map(|(key, nodes)| (nodes.len() == 1).then_some((key, nodes[0])))
        .collect()
}

fn unique_unmatched_side_children_by_class_name(
    dom: &SemanticDom,
    parent: NodeId,
    used: &HashSet<NodeId>,
) -> HashMap<(Ustr, String), NodeId> {
    let mut grouped: HashMap<(Ustr, String), Vec<NodeId>> = HashMap::new();
    for &child in &dom.node(parent).children {
        if used.contains(&child) {
            continue;
        }
        let node = dom.node(child);
        grouped
            .entry((node.class, node.name.clone()))
            .or_default()
            .push(child);
    }
    grouped
        .into_iter()
        .filter_map(|(key, nodes)| (nodes.len() == 1).then_some((key, nodes[0])))
        .collect()
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
