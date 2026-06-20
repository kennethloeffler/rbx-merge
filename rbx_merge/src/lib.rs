use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error::Error as StdError,
    fmt,
    io::Cursor,
    path::Path,
};

use indexmap::IndexMap;
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use rbx_reflection::ClassTag;
use rbx_types::{Attributes, Content, ContentType, Ref, UniqueId, Variant};
use rbx_xml::{DecodeOptions, DecodePropertyBehavior, EncodeOptions, EncodePropertyBehavior};
use thiserror::Error;
use ustr::{Ustr, ustr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileFormat {
    BinaryModel,
    BinaryPlace,
    XmlModel,
    XmlPlace,
}

impl FileFormat {
    pub fn from_extension(path: &Path) -> Option<Self> {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("rbxm") => Some(Self::BinaryModel),
            Some("rbxl") => Some(Self::BinaryPlace),
            Some("rbxmx") => Some(Self::XmlModel),
            Some("rbxlx") => Some(Self::XmlPlace),
            _ => None,
        }
    }

    fn is_xml(self) -> bool {
        matches!(self, Self::XmlModel | Self::XmlPlace)
    }
}

impl fmt::Display for FileFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FileFormat::BinaryModel => "binary model (.rbxm)",
            FileFormat::BinaryPlace => "binary place (.rbxl)",
            FileFormat::XmlModel => "XML model (.rbxmx)",
            FileFormat::XmlPlace => "XML place (.rbxlx)",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictPolicy {
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnknownPropertyPolicy {
    PreserveWhenSupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOptions {
    pub base_format: Option<FileFormat>,
    pub ours_format: Option<FileFormat>,
    pub theirs_format: Option<FileFormat>,
    pub output_format: Option<FileFormat>,
    pub conflict_policy: ConflictPolicy,
    pub unknown_property_policy: UnknownPropertyPolicy,
}

impl Default for MergeOptions {
    fn default() -> Self {
        Self {
            base_format: None,
            ours_format: None,
            theirs_format: None,
            output_format: None,
            conflict_policy: ConflictPolicy::Report,
            unknown_property_policy: UnknownPropertyPolicy::PreserveWhenSupported,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MergeInput<'a> {
    pub base: &'a [u8],
    pub ours: &'a [u8],
    pub theirs: &'a [u8],
    pub path_hint: Option<&'a Path>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    Clean {
        merged: Vec<u8>,
        diagnostics: Vec<Diagnostic>,
    },
    Conflicted {
        conflicts: Vec<Conflict>,
        diagnostics: Vec<Diagnostic>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Info,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    InstanceIdentity,
    UniqueIdCollision,
    DeleteModify,
    PropertyValue,
    ParentMove,
    ChildOrder,
    RefTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayValue {
    pub text: String,
}

impl DisplayValue {
    fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub kind: ConflictKind,
    pub path: String,
    pub class: String,
    pub name: String,
    pub property: Option<String>,
    pub base: Option<DisplayValue>,
    pub ours: Option<DisplayValue>,
    pub theirs: Option<DisplayValue>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("could not detect Roblox file format{path}")]
    UnknownFormat { path: String },

    #[error("failed to decode {format}: {message}")]
    Decode { format: FileFormat, message: String },

    #[error("failed to encode {format}: {message}")]
    Encode { format: FileFormat, message: String },

    #[error("{0}")]
    Internal(String),
}

pub fn detect_format(
    bytes: &[u8],
    path_hint: Option<&Path>,
    explicit: Option<FileFormat>,
) -> Result<FileFormat, Error> {
    if let Some(format) = explicit {
        return Ok(format);
    }

    if let Some(path) = path_hint {
        if let Some(format) = FileFormat::from_extension(path) {
            return Ok(format);
        }
    }

    if bytes.starts_with(b"<roblox!") {
        return Ok(FileFormat::BinaryModel);
    }

    let trimmed = trim_ascii_whitespace_start(bytes);
    if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<roblox") {
        return Ok(FileFormat::XmlModel);
    }

    Err(Error::UnknownFormat {
        path: path_hint
            .map(|path| format!(" for {}", path.display()))
            .unwrap_or_default(),
    })
}

pub fn textconv(bytes: &[u8], path_hint: Option<&Path>) -> Result<String, Error> {
    let decoded = decode_file(bytes, path_hint, None)?;
    let semantic = SemanticDom::from_weak_dom(&decoded.dom)?;
    Ok(render_textconv(&semantic, decoded.format))
}

pub fn merge(input: MergeInput<'_>, options: MergeOptions) -> Result<MergeResult, Error> {
    let mut diagnostics = vec![metadata_diagnostic()];

    let base_file = decode_file(input.base, input.path_hint, options.base_format)?;
    let ours_file = decode_file(input.ours, input.path_hint, options.ours_format)?;
    let theirs_file = decode_file(input.theirs, input.path_hint, options.theirs_format)?;
    let output_format = choose_output_format(
        input.path_hint,
        options.output_format,
        options.ours_format,
        ours_file.format,
    );

    let base = SemanticDom::from_weak_dom(&base_file.dom)?;
    let ours = SemanticDom::from_weak_dom(&ours_file.dom)?;
    let theirs = SemanticDom::from_weak_dom(&theirs_file.dom)?;

    let identities = build_identities(&base, &ours, &theirs);
    let mut conflicts = Vec::new();
    let mut graph = merge_semantic_graph(&base, &ours, &theirs, &identities, &mut conflicts)?;

    detect_unique_id_collisions(&graph, &mut conflicts);

    if !conflicts.is_empty() {
        return Ok(MergeResult::Conflicted {
            conflicts,
            diagnostics,
        });
    }

    assign_child_order(
        &mut graph,
        &base,
        &ours,
        &theirs,
        &identities,
        &mut conflicts,
    );
    detect_unique_id_collisions(&graph, &mut conflicts);

    if !conflicts.is_empty() {
        return Ok(MergeResult::Conflicted {
            conflicts,
            diagnostics,
        });
    }

    let doms = SemanticInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
    };
    let dom = build_weak_dom(&graph, &identities, &doms)?;
    let root_refs = dom.root().children().to_vec();
    let merged = encode_file(&dom, &root_refs, output_format)?;

    diagnostics.push(Diagnostic {
        severity: DiagnosticSeverity::Info,
        code: "output_format".to_owned(),
        message: format!("merged output encoded as {output_format}"),
        path: input.path_hint.map(|path| path.display().to_string()),
    });

    Ok(MergeResult::Clean {
        merged,
        diagnostics,
    })
}

fn trim_ascii_whitespace_start(mut bytes: &[u8]) -> &[u8] {
    while let Some((first, rest)) = bytes.split_first() {
        if !first.is_ascii_whitespace() {
            break;
        }
        bytes = rest;
    }
    bytes
}

fn choose_output_format(
    path_hint: Option<&Path>,
    output_format: Option<FileFormat>,
    explicit_ours_format: Option<FileFormat>,
    detected_ours_format: FileFormat,
) -> FileFormat {
    output_format
        .or(explicit_ours_format)
        .or_else(|| path_hint.and_then(FileFormat::from_extension))
        .unwrap_or(detected_ours_format)
}

fn metadata_diagnostic() -> Diagnostic {
    Diagnostic {
        severity: DiagnosticSeverity::Warning,
        code: "weak_dom_metadata".to_owned(),
        message: "WeakDom does not model every Roblox file-level metadata field; this prototype is semantic, not byte-perfect.".to_owned(),
        path: None,
    }
}

struct DecodedFile {
    format: FileFormat,
    dom: WeakDom,
}

fn decode_file(
    bytes: &[u8],
    path_hint: Option<&Path>,
    explicit: Option<FileFormat>,
) -> Result<DecodedFile, Error> {
    let format = detect_format(bytes, path_hint, explicit)?;
    let dom = if format.is_xml() {
        let options = DecodeOptions::new().property_behavior(DecodePropertyBehavior::ReadUnknown);
        rbx_xml::from_reader(Cursor::new(bytes), options).map_err(|source| Error::Decode {
            format,
            message: error_to_string(source),
        })?
    } else {
        rbx_binary::from_reader(Cursor::new(bytes)).map_err(|source| Error::Decode {
            format,
            message: error_to_string(source),
        })?
    };

    Ok(DecodedFile { format, dom })
}

fn encode_file(dom: &WeakDom, root_refs: &[Ref], format: FileFormat) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    if format.is_xml() {
        let options = EncodeOptions::new().property_behavior(EncodePropertyBehavior::WriteUnknown);
        rbx_xml::to_writer(&mut output, dom, root_refs, options).map_err(|source| {
            Error::Encode {
                format,
                message: error_to_string(source),
            }
        })?;
    } else {
        rbx_binary::to_writer(&mut output, dom, root_refs).map_err(|source| Error::Encode {
            format,
            message: error_to_string(source),
        })?;
    }
    Ok(output)
}

fn error_to_string(error: impl StdError) -> String {
    error.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct NodeId(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct MergeNodeId(usize);

#[derive(Debug)]
struct SemanticDom {
    root: NodeId,
    nodes: IndexMap<NodeId, SemanticInstance>,
    ref_to_node: HashMap<Ref, NodeId>,
}

#[derive(Debug, Clone)]
struct SemanticInstance {
    #[allow(dead_code)]
    id: NodeId,
    source_ref: Ref,
    class: Ustr,
    name: String,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    properties: BTreeMap<Ustr, Variant>,
}

impl SemanticDom {
    fn from_weak_dom(dom: &WeakDom) -> Result<Self, Error> {
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

    fn node(&self, id: NodeId) -> &SemanticInstance {
        self.nodes
            .get(&id)
            .unwrap_or_else(|| panic!("missing semantic node {id:?}"))
    }

    fn path(&self, id: NodeId) -> String {
        if id == self.root {
            return "/".to_owned();
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
        format!("/{}", parts.join("/"))
    }

    fn identity_label(&self, id: NodeId) -> String {
        match self.unique_id(id) {
            Some(unique_id) => format!("uid:{unique_id}"),
            None => format!("path:{}", self.path(id)),
        }
    }

    fn unique_id(&self, id: NodeId) -> Option<UniqueId> {
        match self.node(id).properties.get(&ustr("UniqueId")) {
            Some(Variant::UniqueId(unique_id)) if !unique_id.is_nil() => Some(*unique_id),
            _ => None,
        }
    }

    fn child_merge_ids(
        &self,
        parent: NodeId,
        source: ValueSource,
        identities: &IdentitySet,
        final_parent: MergeNodeId,
        graph: &MergedGraph,
    ) -> Vec<MergeNodeId> {
        self.node(parent)
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
}

#[derive(Debug, Default)]
struct IdentitySet {
    entries: IndexMap<MergeNodeId, MergeEntry>,
    base_to_merge: HashMap<NodeId, MergeNodeId>,
    ours_to_merge: HashMap<NodeId, MergeNodeId>,
    theirs_to_merge: HashMap<NodeId, MergeNodeId>,
}

#[derive(Debug, Clone)]
struct MergeEntry {
    base: Option<NodeId>,
    ours: Option<NodeId>,
    theirs: Option<NodeId>,
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

    fn lookup(&self, source: ValueSource, node: NodeId) -> Option<MergeNodeId> {
        match source {
            ValueSource::Base => self.base_to_merge.get(&node).copied(),
            ValueSource::Ours => self.ours_to_merge.get(&node).copied(),
            ValueSource::Theirs => self.theirs_to_merge.get(&node).copied(),
            ValueSource::Merged => None,
        }
    }

    fn resolve_ref(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ValueSource {
    Base,
    Ours,
    Theirs,
    Merged,
}

struct SemanticInputs<'a> {
    base: &'a SemanticDom,
    ours: &'a SemanticDom,
    theirs: &'a SemanticDom,
}

fn build_identities(base: &SemanticDom, ours: &SemanticDom, theirs: &SemanticDom) -> IdentitySet {
    let base_to_ours = match_base_to_side(base, ours);
    let base_to_theirs = match_base_to_side(base, theirs);

    let mut identities = IdentitySet::default();
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
        if let Some(candidate) = find_added_match(&identities, ours, theirs, theirs_id) {
            identities.set_theirs(candidate, theirs_id);
        } else {
            identities.insert(None, None, Some(theirs_id));
        }
    }

    identities
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
) -> Option<MergeNodeId> {
    let theirs_node = theirs.node(theirs_id);
    let theirs_parent = theirs_node
        .parent
        .and_then(|parent| identities.theirs_to_merge.get(&parent).copied());

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

        let first = matches.next();
        if first.is_some() && matches.next().is_none() {
            return first;
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

    let first = matches.next();
    if first.is_some() && matches.next().is_none() {
        first
    } else {
        None
    }
}

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

fn merge_semantic_graph(
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    identities: &IdentitySet,
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
        if is_deleted_without_conflict(entry, base, ours, theirs, identities, conflicts) {
            continue;
        }

        let Some(class) = merge_class(entry, base, ours, theirs, conflicts) else {
            continue;
        };
        let Some(name) = merge_name(entry, base, ours, theirs, conflicts) else {
            continue;
        };
        let parent = merge_parent(entry, base, ours, theirs, identities, conflicts);
        let properties = merge_properties(entry, &doms, identities, conflicts);
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

fn is_deleted_without_conflict(
    entry: &MergeEntry,
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    identities: &IdentitySet,
    conflicts: &mut Vec<Conflict>,
) -> bool {
    let Some(base_id) = entry.base else {
        return false;
    };

    match (entry.ours, entry.theirs) {
        (None, None) => true,
        (None, Some(theirs_id)) => {
            if side_node_changed_from_base(
                base_id,
                theirs_id,
                ValueSource::Theirs,
                base,
                theirs,
                identities,
            ) {
                conflicts.push(node_conflict(
                    ConflictKind::DeleteModify,
                    base,
                    base_id,
                    None,
                    Some("present in base"),
                    Some("deleted"),
                    Some("modified"),
                ));
            }
            true
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
                conflicts.push(node_conflict(
                    ConflictKind::DeleteModify,
                    base,
                    base_id,
                    None,
                    Some("present in base"),
                    Some("modified"),
                    Some("deleted"),
                ));
            }
            true
        }
        (Some(_), Some(_)) => false,
    }
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
    conflicts: &mut Vec<Conflict>,
) -> Option<Ustr> {
    let base_value = entry.base.map(|id| base.node(id).class);
    let ours_value = entry.ours.map(|id| ours.node(id).class);
    let theirs_value = entry.theirs.map(|id| theirs.node(id).class);
    merge_scalar(base_value, ours_value, theirs_value).or_else(|| {
        let (dom, id) = conflict_subject(entry, base, ours, theirs);
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
    conflicts: &mut Vec<Conflict>,
) -> Option<String> {
    let base_value = entry.base.map(|id| base.node(id).name.clone());
    let ours_value = entry.ours.map(|id| ours.node(id).name.clone());
    let theirs_value = entry.theirs.map(|id| theirs.node(id).name.clone());
    merge_scalar(base_value.clone(), ours_value.clone(), theirs_value.clone()).or_else(|| {
        let (dom, id) = conflict_subject(entry, base, ours, theirs);
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
            conflicts.push(node_conflict(
                ConflictKind::ParentMove,
                dom,
                id,
                None,
                base_parent.map(|value| format!("{value:?}")),
                ours_parent.map(|value| format!("{value:?}")),
                theirs_parent.map(|value| format!("{value:?}")),
            ));
            base_parent
        }
    }
}

fn merge_properties(
    entry: &MergeEntry,
    doms: &SemanticInputs<'_>,
    identities: &IdentitySet,
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
                conflicts.push(node_conflict(
                    ConflictKind::PropertyValue,
                    dom,
                    id,
                    Some(key.as_str()),
                    base_value
                        .map(|value| display_variant(value, ValueSource::Base, identities, doms)),
                    ours_value
                        .map(|value| display_variant(value, ValueSource::Ours, identities, doms)),
                    theirs_value
                        .map(|value| display_variant(value, ValueSource::Theirs, identities, doms)),
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

    if merged.is_empty() {
        PropertyMerge::Delete
    } else {
        PropertyMerge::Keep(MergedProperty {
            value: Variant::Attributes(merged),
            source: ValueSource::Merged,
        })
    }
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

fn assign_child_order(
    graph: &mut MergedGraph,
    base: &SemanticDom,
    ours: &SemanticDom,
    theirs: &SemanticDom,
    identities: &IdentitySet,
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
            .map(|parent| base.child_merge_ids(parent, ValueSource::Base, identities, id, graph))
            .unwrap_or_default();
        let ours_seq = entry
            .ours
            .map(|parent| ours.child_merge_ids(parent, ValueSource::Ours, identities, id, graph))
            .unwrap_or_default();
        let theirs_seq = entry
            .theirs
            .map(|parent| {
                theirs.child_merge_ids(parent, ValueSource::Theirs, identities, id, graph)
            })
            .unwrap_or_default();

        let merged = merge_child_sequence(&base_seq, &ours_seq, &theirs_seq);
        match merged {
            ChildOrderMerge::Clean(children) => {
                if let Some(node) = graph.nodes.get_mut(&id) {
                    node.children = children;
                }
            }
            ChildOrderMerge::Conflict => {
                let (dom, node_id) = conflict_subject(entry, base, ours, theirs);
                conflicts.push(node_conflict(
                    ConflictKind::ChildOrder,
                    dom,
                    node_id,
                    None,
                    Some(format!("{:?}", base_seq)),
                    Some(format!("{:?}", ours_seq)),
                    Some(format!("{:?}", theirs_seq)),
                ));
                if let Some(node) = graph.nodes.get_mut(&id) {
                    node.children = node.children.clone();
                }
            }
        }
        let _ = node;
    }
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

fn detect_unique_id_collisions(graph: &MergedGraph, conflicts: &mut Vec<Conflict>) {
    let mut seen: HashMap<UniqueId, MergeNodeId> = HashMap::new();
    for (&id, node) in &graph.nodes {
        let Some(Variant::UniqueId(unique_id)) = node
            .properties
            .get(&ustr("UniqueId"))
            .map(|property| &property.value)
        else {
            continue;
        };
        if unique_id.is_nil() {
            continue;
        }
        if let Some(first_id) = seen.insert(*unique_id, id) {
            conflicts.push(Conflict {
                kind: ConflictKind::UniqueIdCollision,
                path: format!("{:?}", id),
                class: node.class.to_string(),
                name: node.name.clone(),
                property: Some("UniqueId".to_owned()),
                base: Some(DisplayValue::new(format!("first node {first_id:?}"))),
                ours: Some(DisplayValue::new(unique_id.to_string())),
                theirs: None,
            });
        }
    }
}

fn build_weak_dom(
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

fn variant_options_equal(
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

fn variant_key(
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

fn display_variant(
    value: &Variant,
    source: ValueSource,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) -> String {
    match value {
        Variant::String(value) => format!("{value:?}"),
        Variant::Float32(value) => format!("Float32({value:?})"),
        Variant::Float64(value) => format!("Float64({value:?})"),
        Variant::Ref(referent) => {
            format!("Ref({})", ref_display(*referent, source, identities, doms))
        }
        Variant::Content(content) => match content.value() {
            ContentType::Object(referent) => {
                format!(
                    "Content::Object({})",
                    ref_display(*referent, source, identities, doms)
                )
            }
            ContentType::Uri(uri) => format!("Content::Uri({uri:?})"),
            ContentType::None => "Content::None".to_owned(),
            _ => "Content::<unknown>".to_owned(),
        },
        Variant::Attributes(attributes) => {
            let mut out = String::from("{");
            let mut first = true;
            for (key, value) in attributes {
                if !first {
                    out.push_str(", ");
                }
                first = false;
                out.push_str(key);
                out.push_str(": ");
                out.push_str(&display_variant(value, source, identities, doms));
            }
            out.push('}');
            out
        }
        Variant::Tags(tags) => {
            let values = tags
                .iter()
                .map(|tag| format!("{tag:?}"))
                .collect::<Vec<_>>();
            format!("[{}]", values.join(", "))
        }
        Variant::BinaryString(value) => format!("BinaryString({})", bytes_summary(value.as_ref())),
        Variant::SharedString(value) => format!("SharedString({})", bytes_summary(value.data())),
        Variant::NetAssetRef(value) => format!("NetAssetRef({})", bytes_summary(value.data())),
        other => format!("{other:?}"),
    }
}

fn ref_display(
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

fn bytes_summary(bytes: &[u8]) -> String {
    format!("len={} blake3={}", bytes.len(), blake3::hash(bytes))
}

fn render_textconv(dom: &SemanticDom, format: FileFormat) -> String {
    let identities = single_dom_identities(dom);
    let doms = SemanticInputs {
        base: dom,
        ours: dom,
        theirs: dom,
    };
    let mut out = String::new();
    out.push_str(&format!("# Roblox semantic textconv ({format})\n"));
    render_textconv_node(dom, dom.root, 0, &mut out, &identities, &doms);
    out
}

fn single_dom_identities(dom: &SemanticDom) -> IdentitySet {
    let mut identities = IdentitySet::default();
    for (&id, _) in &dom.nodes {
        identities.insert(Some(id), None, None);
    }
    identities
}

fn render_textconv_node(
    dom: &SemanticDom,
    id: NodeId,
    depth: usize,
    out: &mut String,
    identities: &IdentitySet,
    doms: &SemanticInputs<'_>,
) {
    let node = dom.node(id);
    let indent = "  ".repeat(depth);
    out.push_str(&format!(
        "{indent}{} [{}] id={}\n",
        dom.path(id),
        node.class,
        dom.identity_label(id)
    ));

    if !node.properties.is_empty() {
        out.push_str(&format!("{indent}  Properties:\n"));
        for (&key, value) in &node.properties {
            match value {
                Variant::Attributes(attributes) => {
                    out.push_str(&format!("{indent}    {key}:\n"));
                    for (attr_key, attr_value) in attributes {
                        out.push_str(&format!(
                            "{indent}      {attr_key} = {}\n",
                            display_variant(attr_value, ValueSource::Base, identities, doms)
                        ));
                    }
                }
                _ => out.push_str(&format!(
                    "{indent}    {key} = {}\n",
                    display_variant(value, ValueSource::Base, identities, doms)
                )),
            }
        }
    }

    if !node.children.is_empty() {
        out.push_str(&format!("{indent}  Children:\n"));
        for (index, child) in node.children.iter().enumerate() {
            let child_node = dom.node(*child);
            out.push_str(&format!(
                "{indent}    {index}: {} [{}] id={}\n",
                dom.path(*child),
                child_node.class,
                dom.identity_label(*child)
            ));
        }
    }

    for &child in &node.children {
        render_textconv_node(dom, child, depth + 1, out, identities, doms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn encode_xml(dom: &WeakDom) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let options = EncodeOptions::new().property_behavior(EncodePropertyBehavior::WriteUnknown);
        rbx_xml::to_writer(&mut bytes, dom, dom.root().children(), options)?;
        Ok(bytes)
    }

    fn string_value(name: &str, value: &str) -> InstanceBuilder {
        InstanceBuilder::new("StringValue")
            .with_name(name)
            .with_property("Value", value)
    }

    fn model_with_child(child: InstanceBuilder) -> WeakDom {
        WeakDom::new(InstanceBuilder::new("DataModel").with_child(child))
    }

    fn merge_xml(base: &WeakDom, ours: &WeakDom, theirs: &WeakDom) -> Result<MergeResult> {
        let base = encode_xml(base)?;
        let ours = encode_xml(ours)?;
        let theirs = encode_xml(theirs)?;
        Ok(merge(
            MergeInput {
                base: &base,
                ours: &ours,
                theirs: &theirs,
                path_hint: Some(Path::new("fixture.rbxmx")),
            },
            MergeOptions::default(),
        )?)
    }

    #[test]
    fn one_sided_property_edit_is_clean() -> Result<()> {
        let base = model_with_child(string_value("Value", "base"));
        let ours = model_with_child(string_value("Value", "ours"));
        let theirs = model_with_child(string_value("Value", "base"));

        let result = merge_xml(&base, &ours, &theirs)?;
        let MergeResult::Clean { merged, .. } = result else {
            panic!("expected clean merge: {result:#?}");
        };

        let text = textconv(&merged, Some(Path::new("fixture.rbxmx")))?;
        assert!(text.contains("Value = \"ours\""));
        Ok(())
    }

    #[test]
    fn same_property_edit_is_clean() -> Result<()> {
        let base = model_with_child(string_value("Value", "base"));
        let ours = model_with_child(string_value("Value", "same"));
        let theirs = model_with_child(string_value("Value", "same"));

        let result = merge_xml(&base, &ours, &theirs)?;
        let MergeResult::Clean { merged, .. } = result else {
            panic!("expected clean merge: {result:#?}");
        };

        let text = textconv(&merged, Some(Path::new("fixture.rbxmx")))?;
        assert!(text.contains("Value = \"same\""));
        Ok(())
    }

    #[test]
    fn conflicting_property_edit_reports_no_bytes() -> Result<()> {
        let base = model_with_child(string_value("Value", "base"));
        let ours = model_with_child(string_value("Value", "ours"));
        let theirs = model_with_child(string_value("Value", "theirs"));

        let result = merge_xml(&base, &ours, &theirs)?;
        let MergeResult::Conflicted { conflicts, .. } = result else {
            panic!("expected conflict");
        };

        assert!(
            conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::PropertyValue)
        );
        Ok(())
    }

    #[test]
    fn one_sided_add_is_clean() -> Result<()> {
        let base = WeakDom::new(InstanceBuilder::new("DataModel"));
        let ours = model_with_child(string_value("Added", "ours"));
        let theirs = WeakDom::new(InstanceBuilder::new("DataModel"));

        let result = merge_xml(&base, &ours, &theirs)?;
        let MergeResult::Clean { merged, .. } = result else {
            panic!("expected clean merge: {result:#?}");
        };

        let text = textconv(&merged, Some(Path::new("fixture.rbxmx")))?;
        assert!(text.contains("/Added [StringValue]"));
        Ok(())
    }

    #[test]
    fn textconv_prints_internal_ref_targets() -> Result<()> {
        let target = InstanceBuilder::new("Folder").with_name("Target");
        let target_ref = target.referent();
        let pointer = InstanceBuilder::new("ObjectValue")
            .with_name("Pointer")
            .with_property("Value", Variant::Ref(target_ref));
        let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_children([target, pointer]));
        let bytes = encode_xml(&dom)?;
        let text = textconv(&bytes, Some(Path::new("fixture.rbxmx")))?;

        assert!(text.contains("Value = Ref(internal:MergeNodeId("));
        Ok(())
    }

    #[test]
    fn model_output_does_not_serialize_synthetic_datamodel() -> Result<()> {
        let base = model_with_child(string_value("Value", "base"));
        let result = merge_xml(&base, &base, &base)?;
        let MergeResult::Clean { merged, .. } = result else {
            panic!("expected clean merge: {result:#?}");
        };

        let decoded = decode_file(&merged, Some(Path::new("fixture.rbxmx")), None)?;
        assert_eq!(decoded.dom.root().children().len(), 1);
        let child_ref = decoded.dom.root().children()[0];
        let child = decoded.dom.get_by_ref(child_ref).unwrap();
        assert_eq!(child.class.as_str(), "StringValue");
        Ok(())
    }
}
