use anyhow::Result;
use rbx_types::Variant;

use super::common;
use crate::ConflictKind;

#[test]
fn internal_ref_survives_merge_round_trip() -> Result<()> {
    let path = common::model_path("ref-child", "xml.rbxmx");
    let base = common::read_fixture(&path)?;

    let result = common::merge_fixture_bytes(&base, &base, &base, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let object_value = common::find_by_name(&decoded, "Value")?;
    let referent = match decoded
        .get_by_ref(object_value)
        .and_then(|instance| instance.properties.get(&rbx_dom_weak::ustr("Value")))
    {
        Some(Variant::Ref(referent)) => *referent,
        other => panic!("ObjectValue.Value was not a Ref: {other:?}"),
    };

    let target = decoded
        .get_by_ref(referent)
        .expect("merged ref did not resolve to an instance in the merged dom");
    assert_eq!(target.name, "Ref Target");
    Ok(())
}

#[test]
fn ref_to_deleted_target_conflicts() -> Result<()> {
    // A reachable dangling reference: `ours` repoints the ObjectValue at a new
    // sibling target, while `theirs` deletes that target. The merge keeps ours'
    // reference (theirs left the reference itself untouched), so the surviving
    // reference points at an instance dropped from the merge.
    let path = common::model_path("ref-child", "xml.rbxmx");
    let base = common::edit_fixture(&path, |dom| {
        common::insert_child_at_root(
            dom,
            rbx_dom_weak::InstanceBuilder::new("Folder").with_name("OtherTarget"),
        );
        Ok(())
    })?;
    let ours = common::edit_bytes(&base, &path, |dom| {
        let other = common::find_by_name(dom, "OtherTarget")?;
        common::set_property(dom, "Value", "Value", Variant::Ref(other))
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::delete_instance(dom, "OtherTarget")
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (conflicts, _) = common::expect_conflicted(result);

    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::RefTarget),
        "expected a RefTarget conflict, got {conflicts:#?}"
    );
    Ok(())
}

#[test]
fn ref_nilled_by_deletion_is_reported() -> Result<()> {
    // `ours` deletes the referenced target; `theirs` leaves it alone. The merge
    // is clean (the deleter's nilled reference wins), but the lost link is
    // reported rather than dropped silently.
    let path = common::model_path("ref-child", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| common::delete_instance(dom, "Ref Target"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path)?;
    let (_, diagnostics) = common::expect_clean(result);

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "dropped_reference"),
        "expected a dropped_reference diagnostic, got {diagnostics:#?}"
    );
    Ok(())
}
