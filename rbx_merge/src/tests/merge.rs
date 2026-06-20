use anyhow::Result;
use rbx_dom_weak::InstanceBuilder;

use super::common;
use crate::{ConflictKind, MergeOptions, textconv};

#[test]
fn clean_one_sided_property_edit_snapshots_output() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 9001_i64)
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path, MergeOptions::default())?;
    let (merged, diagnostics) = common::expect_clean(result);

    insta::assert_debug_snapshot!("clean_one_sided_property_edit_diagnostics", diagnostics);
    insta::assert_snapshot!(
        "clean_one_sided_property_edit_output_xml",
        common::xml_string(&merged)?
    );
    insta::assert_snapshot!(
        "clean_one_sided_property_edit_textconv",
        textconv(&merged, Some(&path))?
    );
    Ok(())
}

#[test]
fn conflicting_property_edit_snapshots_conflicts() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 1_i64)
    })?;
    let theirs = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 2_i64)
    })?;

    let result =
        common::merge_fixture_bytes(&base, &ours, &theirs, &path, MergeOptions::default())?;
    let (conflicts, diagnostics) = common::expect_conflicted(result);

    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::PropertyValue)
    );
    insta::assert_debug_snapshot!("conflicting_property_edit_conflicts", conflicts);
    insta::assert_debug_snapshot!("conflicting_property_edit_diagnostics", diagnostics);
    Ok(())
}

#[test]
fn clean_one_sided_add_snapshots_output() -> Result<()> {
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        let child = InstanceBuilder::new("StringValue")
            .with_name("Merge Added")
            .with_referent(common::stable_ref(
                0x1111_2222_3333_4444_5555_6666_7777_8888,
            ))
            .with_property("Value", "ours");

        common::insert_child(dom, "Folder", child)?;
        Ok(())
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path, MergeOptions::default())?;
    let (merged, diagnostics) = common::expect_clean(result);

    insta::assert_debug_snapshot!("clean_one_sided_add_diagnostics", diagnostics);
    insta::assert_snapshot!(
        "clean_one_sided_add_output_xml",
        common::xml_string(&merged)?
    );
    insta::assert_snapshot!(
        "clean_one_sided_add_textconv",
        textconv(&merged, Some(&path))?
    );
    Ok(())
}

#[test]
fn unchanged_real_model_does_not_serialize_synthetic_datamodel() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;

    let result = common::merge_fixture_bytes(&base, &base, &base, &path, MergeOptions::default())?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    assert_eq!(decoded.root().children().len(), 3);
    for child_ref in decoded.root().children() {
        let child = decoded.get_by_ref(*child_ref).unwrap();
        assert_eq!(child.class.as_str(), "IntValue");
    }

    insta::assert_snapshot!(
        "unchanged_real_model_output_xml",
        common::xml_string(&merged)?
    );
    Ok(())
}
