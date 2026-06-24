//! Human-readable rendering: per-value display strings and the deterministic
//! `textconv` tree output used for diffs.
//!
//! The tree uses indentation alone to convey nesting (no repeated paths) and
//! renders instances as `ClassName "Name"` with their properties beneath them.
//! References are shown as Roblox-style dotted paths to their target.
//!
//! Rendering is allocation-bound, so values and lines are appended directly into
//! the caller's output buffer rather than each returning its own `String`. The
//! `*_into` functions are the primitives; [`display_variant`] is a thin
//! `String`-returning wrapper kept for the (cold) conflict-reporting path.

use std::fmt::Write as _;

use rbx_types::{CFrame, ContentType, PhysicalProperties, Ref, Variant, Vector3};

use crate::format::FileFormat;
use crate::semantic::{NodeId, SemanticDom, SemanticInputs, ValueSource, bytes_summary};

const INDENT: &str = "  ";

/// Append a formatted fragment to a `String`. Writing to a `String` is
/// infallible, so the `fmt::Result` is discarded.
macro_rules! w {
    ($out:expr, $($arg:tt)*) => {{
        let _ = write!($out, $($arg)*);
    }};
}

/// `depth` levels of indentation, written without an intermediate `String`.
fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str(INDENT);
    }
}

/// The three components of a `Vector3`, comma-separated, each the shortest
/// round-tripping decimal (Rust's `{:?}`).
fn push_vec3(out: &mut String, value: &Vector3) {
    w!(out, "{:?}, {:?}, {:?}", value.x, value.y, value.z);
}

/// A `CFrame` as `position | rotation`, with the 3x3 orientation flattened.
fn push_cframe(out: &mut String, value: &CFrame) {
    let m = &value.orientation;
    out.push_str("CFrame(");
    push_vec3(out, &value.position);
    out.push_str(" | ");
    push_vec3(out, &m.x);
    out.push_str(", ");
    push_vec3(out, &m.y);
    out.push_str(", ");
    push_vec3(out, &m.z);
    out.push(')');
}

/// Render `value` into `out`. This is the renderer's hot primitive: every arm
/// appends in place rather than building and returning a `String`.
pub(crate) fn display_variant_into(
    out: &mut String,
    value: &Variant,
    source: ValueSource,
    doms: &SemanticInputs<'_>,
) {
    match value {
        Variant::String(value) => w!(out, "{value:?}"),
        Variant::Bool(value) => w!(out, "Bool({value})"),
        Variant::Int32(value) => w!(out, "Int32({value})"),
        Variant::Int64(value) => w!(out, "Int64({value})"),
        Variant::Float32(value) => w!(out, "Float32({value:?})"),
        Variant::Float64(value) => w!(out, "Float64({value:?})"),
        Variant::Enum(value) => w!(out, "Enum({})", value.to_u32()),
        Variant::Vector2(value) => w!(out, "Vector2({:?}, {:?})", value.x, value.y),
        Variant::Vector2int16(value) => w!(out, "Vector2int16({}, {})", value.x, value.y),
        Variant::Vector3(value) => {
            out.push_str("Vector3(");
            push_vec3(out, value);
            out.push(')');
        }
        Variant::Vector3int16(value) => {
            w!(out, "Vector3int16({}, {}, {})", value.x, value.y, value.z)
        }
        Variant::CFrame(value) => push_cframe(out, value),
        Variant::OptionalCFrame(value) => match value {
            Some(value) => {
                out.push_str("OptionalCFrame(");
                push_cframe(out, value);
                out.push(')');
            }
            None => out.push_str("OptionalCFrame(none)"),
        },
        Variant::Color3(value) => {
            w!(out, "Color3({:?}, {:?}, {:?})", value.r, value.g, value.b)
        }
        Variant::Color3uint8(value) => {
            w!(out, "Color3uint8({}, {}, {})", value.r, value.g, value.b)
        }
        Variant::UDim(value) => w!(out, "UDim({:?}, {})", value.scale, value.offset),
        Variant::UDim2(value) => w!(
            out,
            "UDim2({:?}, {}, {:?}, {})",
            value.x.scale,
            value.x.offset,
            value.y.scale,
            value.y.offset,
        ),
        Variant::NumberRange(value) => w!(out, "NumberRange({:?}, {:?})", value.min, value.max),
        Variant::Rect(value) => w!(
            out,
            "Rect({:?}, {:?}, {:?}, {:?})",
            value.min.x,
            value.min.y,
            value.max.x,
            value.max.y,
        ),
        Variant::Region3(value) => {
            out.push_str("Region3(");
            push_vec3(out, &value.min);
            out.push_str(" | ");
            push_vec3(out, &value.max);
            out.push(')');
        }
        Variant::Region3int16(value) => w!(
            out,
            "Region3int16({}, {}, {} | {}, {}, {})",
            value.min.x,
            value.min.y,
            value.min.z,
            value.max.x,
            value.max.y,
            value.max.z,
        ),
        Variant::Ray(value) => {
            out.push_str("Ray(");
            push_vec3(out, &value.origin);
            out.push_str(" | ");
            push_vec3(out, &value.direction);
            out.push(')');
        }
        Variant::SecurityCapabilities(value) => w!(out, "SecurityCapabilities({})", value.bits()),
        Variant::PhysicalProperties(value) => match value {
            PhysicalProperties::Default => out.push_str("PhysicalProperties(Default)"),
            PhysicalProperties::Custom(props) => w!(
                out,
                "PhysicalProperties({:?}, {:?}, {:?}, {:?}, {:?}, {:?})",
                props.density(),
                props.friction(),
                props.elasticity(),
                props.friction_weight(),
                props.elasticity_weight(),
                props.acoustic_absorption(),
            ),
        },
        Variant::NumberSequence(value) => {
            out.push_str("NumberSequence([");
            for (i, kp) in value.keypoints.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                w!(out, "({:?}, {:?}, {:?})", kp.time, kp.value, kp.envelope);
            }
            out.push_str("])");
        }
        Variant::ColorSequence(value) => {
            out.push_str("ColorSequence([");
            for (i, kp) in value.keypoints.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                w!(
                    out,
                    "({:?}, {:?}, {:?}, {:?})",
                    kp.time,
                    kp.color.r,
                    kp.color.g,
                    kp.color.b,
                );
            }
            out.push_str("])");
        }
        Variant::Ref(referent) => ref_display_into(out, *referent, source, doms),
        Variant::Content(content) => match content.value() {
            ContentType::Object(referent) => ref_display_into(out, *referent, source, doms),
            ContentType::Uri(uri) => w!(out, "content {uri:?}"),
            ContentType::None => out.push_str("content none"),
            _ => out.push_str("content <unknown>"),
        },
        Variant::Attributes(attributes) => {
            out.push('{');
            for (i, (key, value)) in attributes.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(key);
                out.push_str(": ");
                display_variant_into(out, value, source, doms);
            }
            out.push('}');
        }
        Variant::Tags(tags) => {
            out.push('[');
            for (i, tag) in tags.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                w!(out, "{tag:?}");
            }
            out.push(']');
        }
        Variant::BinaryString(value) => w!(out, "BinaryString({})", bytes_summary(value.as_ref())),
        Variant::SharedString(value) => w!(out, "SharedString({})", bytes_summary(value.data())),
        Variant::NetAssetRef(value) => w!(out, "NetAssetRef({})", bytes_summary(value.data())),
        other => w!(out, "{other:?}"),
    }
}

/// Render `value` to an owned `String`. Used by conflict reporting, which needs
/// an owned display value; the hot rendering path uses [`display_variant_into`].
pub(crate) fn display_variant(
    value: &Variant,
    source: ValueSource,
    doms: &SemanticInputs<'_>,
) -> String {
    let mut out = String::new();
    display_variant_into(&mut out, value, source, doms);
    out
}

fn ref_display_into(out: &mut String, referent: Ref, source: ValueSource, doms: &SemanticInputs<'_>) {
    if referent.is_none() {
        out.push_str("→ nil");
        return;
    }
    let dom = match source {
        ValueSource::Base => Some(doms.base),
        ValueSource::Ours => Some(doms.ours),
        ValueSource::Theirs => Some(doms.theirs),
        ValueSource::Merged => None,
    };
    match dom.and_then(|dom| dom.node_for_ref(referent).map(|node| dom.path(node))) {
        Some(path) => {
            out.push_str("→ ");
            out.push_str(&path);
        }
        None => out.push_str("→ <external>"),
    }
}

pub(crate) fn render_textconv(dom: &SemanticDom, format: FileFormat) -> String {
    let doms = SemanticInputs {
        base: dom,
        ours: dom,
        theirs: dom,
    };
    let mut out = String::new();
    w!(&mut out, "# rbx-merge — {format}\n");
    render_node(dom, dom.root(), 0, &mut out, &doms);
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
    if depth == 0 {
        push_indent(out, depth);
        out.push_str(node.class.as_str());
        out.push('\n');
    } else {
        // Separate each instance from its preceding sibling or its parent's
        // properties with a blank line. Line-based diff tools anchor on these
        // separators, so added/removed instances form hunks that fall on
        // instance boundaries instead of sliding across identical property runs.
        out.push('\n');
        push_indent(out, depth);
        out.push_str(node.class.as_str());
        out.push(' ');
        w!(out, "{:?}", node.name);
        out.push('\n');
    }

    for (&key, value) in &node.properties {
        match value {
            Variant::Attributes(attributes) if attributes.iter().next().is_none() => {
                push_indent(out, depth + 1);
                w!(out, "{key} = {{}}\n");
            }
            Variant::Attributes(attributes) => {
                push_indent(out, depth + 1);
                w!(out, "{key}\n");
                for (attr_key, attr_value) in attributes {
                    push_indent(out, depth + 1);
                    out.push_str(INDENT);
                    w!(out, "{attr_key} = ");
                    display_variant_into(out, attr_value, ValueSource::Base, doms);
                    out.push('\n');
                }
            }
            _ => {
                push_indent(out, depth + 1);
                w!(out, "{key} = ");
                display_variant_into(out, value, ValueSource::Base, doms);
                out.push('\n');
            }
        }
    }

    for &child in &node.children {
        render_node(dom, child, depth + 1, out, doms);
    }
}
