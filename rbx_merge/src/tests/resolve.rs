use anyhow::Result;
use rbx_dom_weak::{InstanceBuilder, ustr};
use rbx_types::Variant;

use super::common;
use crate::{ConflictKind, FileInput, MergeSettings, Resolutions, Side, merge_files};

/// Encoded base/ours/theirs bytes plus the path hint.
type ConflictInputs = (Vec<u8>, Vec<u8>, Vec<u8>, std::path::PathBuf);

/// base/ours/theirs of three-intvalues where ours and theirs set the same
/// IntValue's Value to different numbers — an unavoidable PropertyValue
/// conflict.
fn conflicting_value_inputs() -> Result<ConflictInputs> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 1_i64)
    })?;
    let theirs = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 2_i64)
    })?;
    Ok((base, ours, theirs, path))
}

fn merged_value(merged: &[u8], path: &std::path::Path, instance: &str) -> Result<i64> {
    let decoded = common::decode_bytes(merged, path)?;
    let referent = common::find_by_name(&decoded, instance)?;
    match decoded
        .get_by_ref(referent)
        .and_then(|node| node.properties.get(&ustr("Value")))
    {
        Some(Variant::Int64(value)) => Ok(*value),
        other => anyhow::bail!("expected Int64 Value, got {other:?}"),
    }
}

fn merge_with(resolutions: Resolutions) -> Result<(Vec<u8>, std::path::PathBuf)> {
    let (base, ours, theirs, path) = conflicting_value_inputs()?;
    let report = merge_files(
        FileInput::new(&base).with_path_hint(&path),
        FileInput::new(&ours).with_path_hint(&path),
        FileInput::new(&theirs).with_path_hint(&path),
        MergeSettings {
            resolutions,
            ..Default::default()
        },
    )?;
    let merged = report
        .merged
        .ok_or_else(|| anyhow::anyhow!("expected a clean merge, got {:#?}", report.conflicts))?;
    Ok((merged, path))
}

#[test]
fn bulk_take_ours_resolves_property_conflict() -> Result<()> {
    let (merged, path) = merge_with(Resolutions::take(Side::Ours))?;
    assert_eq!(merged_value(&merged, &path, "Value=1337")?, 1);
    Ok(())
}

#[test]
fn bulk_take_theirs_resolves_property_conflict() -> Result<()> {
    let (merged, path) = merge_with(Resolutions::take(Side::Theirs))?;
    assert_eq!(merged_value(&merged, &path, "Value=1337")?, 2);
    Ok(())
}

#[test]
fn per_conflict_override_resolves_one_property() -> Result<()> {
    let resolutions = Resolutions::none().resolve(
        ConflictKind::PropertyValue,
        "Value=1337",
        Some("Value".to_owned()),
        Side::Theirs,
    );
    let (merged, path) = merge_with(resolutions)?;
    assert_eq!(merged_value(&merged, &path, "Value=1337")?, 2);
    Ok(())
}

/// base/ours/theirs where ours deletes an instance and theirs modifies it.
fn delete_modify_inputs() -> Result<ConflictInputs> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| common::delete_instance(dom, "Value=1337"))?;
    let theirs = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 42_i64)
    })?;
    Ok((base, ours, theirs, path))
}

fn merge_delete_modify(resolutions: Resolutions) -> Result<(Option<Vec<u8>>, std::path::PathBuf)> {
    let (base, ours, theirs, path) = delete_modify_inputs()?;
    let report = merge_files(
        FileInput::new(&base).with_path_hint(&path),
        FileInput::new(&ours).with_path_hint(&path),
        FileInput::new(&theirs).with_path_hint(&path),
        MergeSettings {
            resolutions,
            ..Default::default()
        },
    )?;
    Ok((report.merged, path))
}

#[test]
fn delete_modify_take_theirs_keeps_modified_instance() -> Result<()> {
    let (merged, path) = merge_delete_modify(Resolutions::take(Side::Theirs))?;
    let merged = merged.expect("take theirs should resolve the delete/modify cleanly");
    let decoded = common::decode_bytes(&merged, &path)?;

    let names = common::child_names(&decoded, decoded.root_ref());
    assert_eq!(names.len(), 3, "the modified instance should survive: {names:?}");
    assert_eq!(merged_value(&merged, &path, "Value=1337")?, 42);
    Ok(())
}

#[test]
fn delete_modify_take_ours_drops_instance() -> Result<()> {
    let (merged, path) = merge_delete_modify(Resolutions::take(Side::Ours))?;
    let merged = merged.expect("take ours should resolve the delete/modify cleanly");
    let decoded = common::decode_bytes(&merged, &path)?;

    let names = common::child_names(&decoded, decoded.root_ref());
    assert_eq!(names.len(), 2, "the deletion should win: {names:?}");
    assert!(!names.contains(&"Value=1337".to_owned()));
    Ok(())
}

#[test]
fn ref_target_conflict_is_resolvable() -> Result<()> {
    // ours repoints the ObjectValue at a new sibling; theirs deletes that
    // sibling, leaving a dangling reference (a RefTarget conflict). Resolving it
    // drops the dangling reference and the merge is clean.
    let path = common::model_path("ref-child", "xml.rbxmx");
    let base = common::edit_fixture(&path, |dom| {
        common::insert_child_at_root(dom, InstanceBuilder::new("Folder").with_name("OtherTarget"));
        Ok(())
    })?;
    let ours = common::edit_bytes(&base, &path, |dom| {
        let other = common::find_by_name(dom, "OtherTarget")?;
        common::set_property(dom, "Value", "Value", Variant::Ref(other))
    })?;
    let theirs =
        common::edit_bytes(&base, &path, |dom| common::delete_instance(dom, "OtherTarget"))?;

    let report = merge_files(
        FileInput::new(&base).with_path_hint(&path),
        FileInput::new(&ours).with_path_hint(&path),
        FileInput::new(&theirs).with_path_hint(&path),
        MergeSettings {
            resolutions: Resolutions::take(Side::Ours),
            ..Default::default()
        },
    )?;

    assert!(report.conflicts.is_empty(), "{:#?}", report.conflicts);
    let merged = report.merged.expect("RefTarget should be resolvable");
    let decoded = common::decode_bytes(&merged, &path)?;

    // No surviving reference dangles: any remaining ref resolves in the output.
    let object_value = common::find_by_name(&decoded, "Value")?;
    if let Some(Variant::Ref(referent)) = decoded
        .get_by_ref(object_value)
        .and_then(|node| node.properties.get(&ustr("Value")))
    {
        assert!(
            referent.is_none() || decoded.get_by_ref(*referent).is_some(),
            "the dangling reference should have been dropped"
        );
    }
    Ok(())
}

#[test]
fn unrelated_override_leaves_conflict_unresolved() -> Result<()> {
    // An override for a different property does not touch this conflict.
    let (base, ours, theirs, path) = conflicting_value_inputs()?;
    let resolutions = Resolutions::none().resolve(
        ConflictKind::PropertyValue,
        "Value=1337",
        Some("SomethingElse".to_owned()),
        Side::Ours,
    );
    let report = merge_files(
        FileInput::new(&base).with_path_hint(&path),
        FileInput::new(&ours).with_path_hint(&path),
        FileInput::new(&theirs).with_path_hint(&path),
        MergeSettings {
            resolutions,
            ..Default::default()
        },
    )?;
    assert!(report.merged.is_none());
    assert!(
        report
            .conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::PropertyValue)
    );
    Ok(())
}
