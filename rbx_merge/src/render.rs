//! Human-readable rendering: per-value display strings and the deterministic
//! `textconv` tree output used for diffs.

use rbx_types::{ContentType, Variant};

use crate::format::FileFormat;
use crate::identity::IdentitySet;
use crate::semantic::{NodeId, SemanticDom, SemanticInputs, ValueSource, bytes_summary};

pub(crate) fn display_variant(
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
    referent: rbx_types::Ref,
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

pub(crate) fn render_textconv(dom: &SemanticDom, format: FileFormat) -> String {
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
