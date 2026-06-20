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

pub(crate) struct SemanticInputs<'a> {
    pub(crate) base: &'a SemanticDom,
    pub(crate) ours: &'a SemanticDom,
    pub(crate) theirs: &'a SemanticDom,
}

#[derive(Debug)]
pub(crate) struct SemanticDom {
    pub(crate) root: NodeId,
    pub(crate) nodes: IndexMap<NodeId, SemanticInstance>,
    pub(crate) ref_to_node: HashMap<Ref, NodeId>,
}

#[derive(Debug, Clone)]
pub(crate) struct SemanticInstance {
    #[allow(dead_code)]
    pub(crate) id: NodeId,
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
                id,
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
            variant_key(left, left_source, identities, doms)
                == variant_key(right, right_source, identities, doms)
        }
        _ => false,
    }
}

pub(crate) fn variant_key(
    value: &Variant,
    source: ValueSource,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> String {
    match value {
        Variant::Float32(value) => format!("Float32:{:08x}", value.to_bits()),
        Variant::Float64(value) => format!("Float64:{:016x}", value.to_bits()),
        Variant::Ref(referent) => format!("Ref:{}", ref_key(*referent, source, identities, doms)),
        Variant::Content(content) => match content.value() {
            ContentType::Object(referent) => {
                format!(
                    "Content:Object:{}",
                    ref_key(*referent, source, identities, doms)
                )
            }
            ContentType::Uri(uri) => format!("Content:Uri:{uri:?}"),
            ContentType::None => "Content:None".to_owned(),
            _ => "Content:<unknown>".to_owned(),
        },
        Variant::Attributes(attributes) => {
            let mut out = String::from("Attributes:{");
            for (key, value) in attributes {
                out.push_str(key);
                out.push('=');
                out.push_str(&variant_key(value, source, identities, doms));
                out.push(';');
            }
            out.push('}');
            out
        }
        Variant::Tags(tags) => {
            let joined = tags
                .iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
                .join("\0");
            format!("Tags:{joined:?}")
        }
        Variant::BinaryString(value) => format!("BinaryString:{}", bytes_summary(value.as_ref())),
        Variant::SharedString(value) => format!("SharedString:{}", bytes_summary(value.data())),
        Variant::NetAssetRef(value) => format!("NetAssetRef:{}", bytes_summary(value.data())),
        other => format!("{other:?}"),
    }
}

fn ref_key(
    referent: Ref,
    source: ValueSource,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> String {
    if referent.is_none() {
        return "nil".to_owned();
    }
    match identities.resolve_ref(source, referent, doms) {
        Some(id) => format!("internal:{id:?}"),
        None => format!("external:{referent}"),
    }
}

pub(crate) fn bytes_summary(bytes: &[u8]) -> String {
    format!("len={} blake3={}", bytes.len(), blake3::hash(bytes))
}
