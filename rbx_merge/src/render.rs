//! Human-readable rendering: per-value display strings and the deterministic
//! `textconv` tree output used for diffs.
//!
//! The tree uses indentation alone to convey nesting (no repeated paths) and
//! renders instances as `ClassName "Name"` with their properties beneath them.
//! References are shown as Roblox-style dotted paths to their target.

use rbx_types::{CFrame, ContentType, PhysicalProperties, Ref, Variant, Vector3};

use crate::format::FileFormat;
use crate::semantic::{NodeId, SemanticDom, SemanticInputs, ValueSource, bytes_summary};

const INDENT: &str = "  ";

/// Shortest round-tripping decimal for a float, matching Rust's `{:?}` (e.g.
/// `0.0`, `1785.8`, `-0.98338413`, `inf`, `NaN`).
fn f32s(value: f32) -> String {
    format!("{value:?}")
}

/// The three components of a `Vector3`, comma-separated.
fn vec3(value: &Vector3) -> String {
    format!("{}, {}, {}", f32s(value.x), f32s(value.y), f32s(value.z))
}

/// A `CFrame` as `position | rotation`, with the 3x3 orientation flattened.
fn cframe(value: &CFrame) -> String {
    let m = &value.orientation;
    format!(
        "CFrame({} | {}, {}, {})",
        vec3(&value.position),
        vec3(&m.x),
        vec3(&m.y),
        vec3(&m.z),
    )
}

pub(crate) fn display_variant(
    value: &Variant,
    source: ValueSource,
    doms: &SemanticInputs<'_>,
) -> String {
    match value {
        Variant::String(value) => format!("{value:?}"),
        Variant::Bool(value) => format!("Bool({value})"),
        Variant::Int32(value) => format!("Int32({value})"),
        Variant::Int64(value) => format!("Int64({value})"),
        Variant::Float32(value) => format!("Float32({value:?})"),
        Variant::Float64(value) => format!("Float64({value:?})"),
        Variant::Enum(value) => format!("Enum({})", value.to_u32()),
        Variant::Vector2(value) => format!("Vector2({}, {})", f32s(value.x), f32s(value.y)),
        Variant::Vector2int16(value) => format!("Vector2int16({}, {})", value.x, value.y),
        Variant::Vector3(value) => format!("Vector3({})", vec3(value)),
        Variant::Vector3int16(value) => {
            format!("Vector3int16({}, {}, {})", value.x, value.y, value.z)
        }
        Variant::CFrame(value) => cframe(value),
        Variant::OptionalCFrame(value) => match value {
            Some(value) => format!("OptionalCFrame({})", cframe(value)),
            None => "OptionalCFrame(none)".to_owned(),
        },
        Variant::Color3(value) => {
            format!("Color3({}, {}, {})", f32s(value.r), f32s(value.g), f32s(value.b))
        }
        Variant::Color3uint8(value) => {
            format!("Color3uint8({}, {}, {})", value.r, value.g, value.b)
        }
        Variant::UDim(value) => format!("UDim({}, {})", f32s(value.scale), value.offset),
        Variant::UDim2(value) => format!(
            "UDim2({}, {}, {}, {})",
            f32s(value.x.scale),
            value.x.offset,
            f32s(value.y.scale),
            value.y.offset,
        ),
        Variant::NumberRange(value) => {
            format!("NumberRange({}, {})", f32s(value.min), f32s(value.max))
        }
        Variant::Rect(value) => format!(
            "Rect({}, {}, {}, {})",
            f32s(value.min.x),
            f32s(value.min.y),
            f32s(value.max.x),
            f32s(value.max.y),
        ),
        Variant::Region3(value) => {
            format!("Region3({} | {})", vec3(&value.min), vec3(&value.max))
        }
        Variant::Region3int16(value) => format!(
            "Region3int16({}, {}, {} | {}, {}, {})",
            value.min.x, value.min.y, value.min.z, value.max.x, value.max.y, value.max.z,
        ),
        Variant::Ray(value) => {
            format!("Ray({} | {})", vec3(&value.origin), vec3(&value.direction))
        }
        Variant::SecurityCapabilities(value) => {
            format!("SecurityCapabilities({})", value.bits())
        }
        Variant::PhysicalProperties(value) => match value {
            PhysicalProperties::Default => "PhysicalProperties(Default)".to_owned(),
            PhysicalProperties::Custom(props) => format!(
                "PhysicalProperties({}, {}, {}, {}, {}, {})",
                f32s(props.density()),
                f32s(props.friction()),
                f32s(props.elasticity()),
                f32s(props.friction_weight()),
                f32s(props.elasticity_weight()),
                f32s(props.acoustic_absorption()),
            ),
        },
        Variant::NumberSequence(value) => {
            let keypoints = value
                .keypoints
                .iter()
                .map(|kp| format!("({}, {}, {})", f32s(kp.time), f32s(kp.value), f32s(kp.envelope)))
                .collect::<Vec<_>>();
            format!("NumberSequence([{}])", keypoints.join(", "))
        }
        Variant::ColorSequence(value) => {
            let keypoints = value
                .keypoints
                .iter()
                .map(|kp| {
                    format!(
                        "({}, {}, {}, {})",
                        f32s(kp.time),
                        f32s(kp.color.r),
                        f32s(kp.color.g),
                        f32s(kp.color.b),
                    )
                })
                .collect::<Vec<_>>();
            format!("ColorSequence([{}])", keypoints.join(", "))
        }
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
        // Separate each instance from its preceding sibling or its parent's
        // properties with a blank line. Line-based diff tools anchor on these
        // separators, so added/removed instances form hunks that fall on
        // instance boundaries instead of sliding across identical property runs.
        out.push('\n');
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
