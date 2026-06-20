use anyhow::Result;
use rbx_dom_weak::ustr;
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
