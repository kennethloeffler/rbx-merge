use std::path::Path;

use anyhow::Result;
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use rbx_types::{UniqueId, Variant};

use super::common;
use crate::{TextconvOptions, textconv, textconv_to};

/// The renderer drops noise that bloats diffs — properties at their class default
/// and the volatile `UniqueId` — while keeping properties set to a real value,
/// and the opt-out restores everything.
#[test]
fn filters_default_and_volatile_properties() -> Result<()> {
    let part = InstanceBuilder::new("Part")
        .with_name("P")
        // `false` is `BasePart.Anchored`'s default → dropped.
        .with_property("Anchored", Variant::Bool(false))
        // `0.5` is not `Transparency`'s default (0.0) → kept.
        .with_property("Transparency", Variant::Float32(0.5))
        // Volatile identity metadata → dropped regardless of value.
        .with_property("UniqueId", Variant::UniqueId(UniqueId::new(1, 2, 3)));
    let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(part));

    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, &dom, dom.root().children())?;
    let path = Path::new("part.rbxm");

    let filtered = textconv(&bytes, Some(path), TextconvOptions::default())?;
    assert!(
        filtered.contains("Transparency = Float32(0.5)"),
        "non-default property should be kept:\n{filtered}"
    );
    assert!(
        !filtered.contains("Anchored"),
        "default-valued property should be dropped:\n{filtered}"
    );
    assert!(
        !filtered.contains("UniqueId"),
        "volatile UniqueId should be dropped:\n{filtered}"
    );

    // The opt-out brings every property back.
    let full = textconv(&bytes, Some(path), TextconvOptions::all())?;
    assert!(
        full.contains("Anchored") && full.contains("UniqueId"),
        "all_properties should restore filtered properties:\n{full}"
    );
    Ok(())
}

/// The streaming renderer must produce exactly the bytes the buffered one does —
/// for every value type and tree shape in the corpus, and under either filtering
/// mode — since it is the same output, just flushed node by node.
#[test]
fn streaming_matches_buffered() -> Result<()> {
    for path in common::all_fixture_paths() {
        let bytes = common::read_fixture(&path)?;
        for options in [TextconvOptions::default(), TextconvOptions::all()] {
            let buffered = textconv(&bytes, Some(&path), options)?;
            let mut streamed = Vec::new();
            textconv_to(&bytes, Some(&path), &mut streamed, options)?;
            assert_eq!(
                buffered.as_bytes(),
                streamed.as_slice(),
                "streamed output diverged for {} (all_properties={})",
                path.display(),
                options.all_properties,
            );
        }
    }
    Ok(())
}

/// The renderer snapshots use the full, unfiltered output so they lock the
/// rendering of every value type and stay independent of the reflection
/// database's default values. The diff-oriented filtering is covered by
/// [`filters_default_and_volatile_properties`].
#[test]
fn textconv_snapshots_xml_model() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let bytes = common::read_fixture(&path)?;
    let text = textconv(&bytes, Some(&path), TextconvOptions::all())?;

    insta::assert_snapshot!("three_intvalues_xml_textconv", text);
    Ok(())
}

#[test]
fn textconv_snapshots_internal_refs() -> Result<()> {
    let path = common::model_path("ref-child", "xml.rbxmx");
    let bytes = common::read_fixture(&path)?;
    let text = textconv(&bytes, Some(&path), TextconvOptions::all())?;

    insta::assert_snapshot!("ref_child_xml_textconv", text);
    Ok(())
}

#[test]
fn textconv_snapshots_binary_model() -> Result<()> {
    let path = common::model_path("attributes", "binary.rbxm");
    let bytes = common::read_fixture(&path)?;
    let text = textconv(&bytes, Some(&path), TextconvOptions::all())?;

    insta::assert_snapshot!("attributes_binary_textconv", text);
    Ok(())
}
