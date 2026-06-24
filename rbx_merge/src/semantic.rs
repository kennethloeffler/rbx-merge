//! Format-independent semantic model of a decoded Roblox DOM, plus the
//! value-equality logic the merge relies on.

use std::collections::{BTreeMap, HashMap};

use indexmap::IndexMap;
use rbx_dom_weak::WeakDom;
use rbx_types::{ContentType, Ref, UniqueId, Variant};
use ustr::{Ustr, ustr};

use crate::Error;
use crate::identity::IdentitySet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct NodeId(pub(crate) usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ValueSource {
    Base,
    Ours,
    Theirs,
    Merged,
}

#[derive(Clone, Copy)]
pub(crate) struct SemanticInputs<'a> {
    pub(crate) base: &'a SemanticDom,
    pub(crate) ours: &'a SemanticDom,
    pub(crate) theirs: &'a SemanticDom,
}

#[derive(Debug)]
pub(crate) struct SemanticDom {
    root: NodeId,
    nodes: IndexMap<NodeId, SemanticInstance>,
    ref_to_node: HashMap<Ref, NodeId>,
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticInstance {
    pub(crate) source_ref: Ref,
    pub(crate) class: Ustr,
    pub(crate) name: String,
    pub(crate) parent: Option<NodeId>,
    pub(crate) children: Vec<NodeId>,
    pub(crate) properties: BTreeMap<Ustr, Variant>,
}

impl SemanticDom {
    pub(crate) fn from_weak_dom(dom: &WeakDom) -> Result<Self, Error> {
        let mut semantic = Self {
            root: NodeId(0),
            nodes: IndexMap::new(),
            ref_to_node: HashMap::new(),
        };

        let root = semantic.insert_subtree(dom, dom.root_ref(), None)?;
        semantic.root = root;
        Ok(semantic)
    }

    fn insert_subtree(
        &mut self,
        dom: &WeakDom,
        referent: Ref,
        parent: Option<NodeId>,
    ) -> Result<NodeId, Error> {
        let instance = dom.get_by_ref(referent).ok_or_else(|| {
            Error::Internal(format!("WeakDom child referent {referent} did not resolve"))
        })?;

        let id = NodeId(self.nodes.len());
        self.ref_to_node.insert(referent, id);
        self.nodes.insert(
            id,
            SemanticInstance {
                source_ref: instance.referent(),
                class: instance.class,
                name: instance.name.clone(),
                parent,
                children: Vec::new(),
                properties: instance
                    .properties
                    .iter()
                    .map(|(key, value)| (*key, value.clone()))
                    .collect(),
            },
        );

        let mut children = Vec::with_capacity(instance.children().len());
        for &child_ref in instance.children() {
            let child = self.insert_subtree(dom, child_ref, Some(id))?;
            children.push(child);
        }
        self.nodes.get_mut(&id).unwrap().children = children;
        Ok(id)
    }

    /// The synthetic root node's id.
    pub(crate) fn root(&self) -> NodeId {
        self.root
    }

    /// Every node id, in insertion (document) order.
    pub(crate) fn node_ids(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.nodes.keys().copied()
    }

    /// The node a source referent decoded to, if any.
    pub(crate) fn node_for_ref(&self, referent: Ref) -> Option<NodeId> {
        self.ref_to_node.get(&referent).copied()
    }

    pub(crate) fn node(&self, id: NodeId) -> &SemanticInstance {
        self.nodes
            .get(&id)
            .unwrap_or_else(|| panic!("missing semantic node {id:?}"))
    }

    /// A Roblox-style dotted path to an instance (`Workspace.Folder.Part`). The
    /// synthetic root renders as `<root>`.
    pub(crate) fn path(&self, id: NodeId) -> String {
        if id == self.root {
            return "<root>".to_owned();
        }

        let mut parts = Vec::new();
        let mut current = Some(id);
        while let Some(node_id) = current {
            if node_id == self.root {
                break;
            }
            let node = self.node(node_id);
            parts.push(node.name.as_str());
            current = node.parent;
        }
        parts.reverse();
        parts.join(".")
    }

    pub(crate) fn unique_id(&self, id: NodeId) -> Option<UniqueId> {
        match self.node(id).properties.get(&ustr("UniqueId")) {
            Some(Variant::UniqueId(unique_id)) if !unique_id.is_nil() => Some(*unique_id),
            _ => None,
        }
    }
}

pub(crate) fn variant_options_equal(
    left: Option<&Variant>,
    left_source: ValueSource,
    right: Option<&Variant>,
    right_source: ValueSource,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            variant_eq(left, left_source, right, right_source, identities, doms)
        }
        _ => false,
    }
}

/// Source-aware value equality: the predicate the three-way merge applies to
/// property values to decide whether two sides hold "the same value". References
/// are compared by the merged instance they resolve to (so the same target
/// matches across files despite differing raw referents), and floats by their bit
/// pattern (so `-0.0` stays distinct from `+0.0`).
///
/// This compares values structurally rather than rendering each to a `String`
/// key and comparing the keys, which avoids allocating — and, for composite
/// types, `Debug`-formatting — a string per value on the merge's hottest path.
///
/// One deliberate consequence of comparing structurally: floats *nested* in
/// composite types (`Vector3`, `CFrame`, `Color3`, ...) now compare with IEEE
/// equality via the value's own `PartialEq` rather than bitwise, so `+0.0` and
/// `-0.0` components are equal and distinct-bit `NaN`s are not. The former key
/// distinguished them only as an artifact of `Debug` formatting; scalar floats
/// remain bit-exact as before.
pub(crate) fn variant_eq(
    left: &Variant,
    left_source: ValueSource,
    right: &Variant,
    right_source: ValueSource,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> bool {
    match (left, right) {
        (Variant::Float32(a), Variant::Float32(b)) => a.to_bits() == b.to_bits(),
        (Variant::Float64(a), Variant::Float64(b)) => a.to_bits() == b.to_bits(),
        (Variant::Ref(a), Variant::Ref(b)) => {
            ref_eq(*a, left_source, *b, right_source, identities, doms)
        }
        (Variant::Content(a), Variant::Content(b)) => {
            content_eq(a.value(), left_source, b.value(), right_source, identities, doms)
        }
        // Attributes can nest references, so recurse with source awareness. Both
        // maps iterate in sorted key order (they are `BTreeMap`-backed), so equal
        // length plus a positional compare settles equality.
        (Variant::Attributes(a), Variant::Attributes(b)) => {
            a.len() == b.len()
                && a.iter().zip(b.iter()).all(|((ka, va), (kb, vb))| {
                    ka == kb && variant_eq(va, left_source, vb, right_source, identities, doms)
                })
        }
        // Every remaining variant is a plain value with no source component, so
        // its own `PartialEq` is the comparison — a direct byte compare for the
        // string-like types, a field compare for the rest. Mismatched variants
        // (different kinds) are never equal.
        _ => left == right,
    }
}

/// Equality of two references by the identity they resolve to, mirroring the
/// former key's `nil`/`internal:`/`external:` encoding.
fn ref_eq(
    left: Ref,
    left_source: ValueSource,
    right: Ref,
    right_source: ValueSource,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> bool {
    match (left.is_none(), right.is_none()) {
        (true, true) => true,
        (false, false) => match (
            identities.resolve_ref(left_source, left, doms),
            identities.resolve_ref(right_source, right, doms),
        ) {
            // Both resolve to a merged identity: equal iff it is the same one.
            (Some(a), Some(b)) => a == b,
            // Both external (unresolved): equal iff the same raw referent.
            (None, None) => left == right,
            // One internal, one external: never equal.
            _ => false,
        },
        // One nil, one not: never equal.
        _ => false,
    }
}

/// Equality of two `Content` values, comparing object referents by resolved
/// identity and URIs by string.
fn content_eq(
    left: &ContentType,
    left_source: ValueSource,
    right: &ContentType,
    right_source: ValueSource,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> bool {
    match (left, right) {
        (ContentType::Object(a), ContentType::Object(b)) => {
            ref_eq(*a, left_source, *b, right_source, identities, doms)
        }
        (ContentType::Uri(a), ContentType::Uri(b)) => a == b,
        (ContentType::None, ContentType::None) => true,
        // Future `ContentType` kinds the merge does not model collapse together,
        // matching the former key's catch-all; distinct known kinds stay unequal.
        (a, b) => is_unknown_content(a) && is_unknown_content(b),
    }
}

fn is_unknown_content(content: &ContentType) -> bool {
    !matches!(
        content,
        ContentType::Object(_) | ContentType::Uri(_) | ContentType::None
    )
}

pub(crate) fn bytes_summary(bytes: &[u8]) -> String {
    format!("len={} blake3={}", bytes.len(), blake3::hash(bytes))
}
