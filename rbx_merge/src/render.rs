//! Human-readable rendering: per-value display strings and the deterministic
//! `textconv` tree output used for diffs.
//!
//! The tree uses indentation alone to convey nesting (no repeated paths) and
//! renders instances as `ClassName "Name"` with their properties beneath them.
//! References are shown as Roblox-style dotted paths to their target.

use rbx_types::{ContentType, Ref, Variant};

use crate::format::FileFormat;
use crate::semantic::{NodeId, SemanticDom, SemanticInputs, ValueSource, bytes_summary};

const INDENT: &str = "  ";

pub(crate) fn display_variant(
    value: &Variant,
    source: ValueSource,
    doms: &SemanticInputs<'_>,
) -> String {
    match value {
        Variant::String(value) => format!("{value:?}"),
        Variant::Float32(value) => format!("Float32({value:?})"),
        Variant::Float64(value) => format!("Float64({value:?})"),
        Variant::Ref(referent) => ref_display(*referent, source, doms),
        Variant::Content(content) => match content.value() {
            ContentType::Object(referent) => ref_display(*referent, source, doms),
            ContentType::Uri(uri) => format!("content {uri:?}"),
            ContentType::None => "content none".to_owned(),
            _ => "content <unknown>".to_owned(),
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
                out.push_str(&display_variant(value, source, doms));
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

fn ref_display(referent: Ref, source: ValueSource, doms: &SemanticInputs<'_>) -> String {
    if referent.is_none() {
        return "→ nil".to_owned();
    }
    let dom = match source {
        ValueSource::Base => Some(doms.base),
        ValueSource::Ours => Some(doms.ours),
        ValueSource::Theirs => Some(doms.theirs),
        ValueSource::Merged => None,
    };
    match dom.and_then(|dom| dom.ref_to_node.get(&referent).map(|&node| dom.path(node))) {
        Some(path) => format!("→ {path}"),
        None => "→ <external>".to_owned(),
    }
}

pub(crate) fn render_textconv(dom: &SemanticDom, format: FileFormat) -> String {
    let doms = SemanticInputs {
        base: dom,
        ours: dom,
        theirs: dom,
    };
    let mut out = String::new();
    out.push_str(&format!("# rbx-merge — {format}\n"));
    render_node(dom, dom.root, 0, &mut out, &doms);
    out
}

fn render_node(
    dom: &SemanticDom,
    id: NodeId,
    depth: usize,
    out: &mut String,
    doms: &SemanticInputs<'_>,
) {
    let node = dom.node(id);
    let indent = INDENT.repeat(depth);
    if depth == 0 {
        out.push_str(&format!("{indent}{}\n", node.class));
    } else {
        out.push_str(&format!("{indent}{} {:?}\n", node.class, node.name));
    }

    let field = INDENT.repeat(depth + 1);
    for (&key, value) in &node.properties {
        match value {
            Variant::Attributes(attributes) if attributes.iter().next().is_none() => {
                out.push_str(&format!("{field}{key} = {{}}\n"));
            }
            Variant::Attributes(attributes) => {
                out.push_str(&format!("{field}{key}\n"));
                for (attr_key, attr_value) in attributes {
                    out.push_str(&format!(
                        "{field}{INDENT}{attr_key} = {}\n",
                        display_variant(attr_value, ValueSource::Base, doms)
                    ));
                }
            }
            _ => out.push_str(&format!(
                "{field}{key} = {}\n",
                display_variant(value, ValueSource::Base, doms)
            )),
        }
    }

    for &child in &node.children {
        render_node(dom, child, depth + 1, out, doms);
    }
}
