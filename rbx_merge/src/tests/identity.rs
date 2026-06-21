use anyhow::Result;
use rbx_dom_weak::{InstanceBuilder, ustr};
use rbx_types::{UniqueId, Variant};

use super::common;
use crate::ConflictKind;

/// Build a `Folder` holding several identically-named `StringValue` children,
/// each distinguished only by its `Value`.
fn folder_with_items(path: &std::path::Path, values: &[&str]) -> Result<Vec<u8>> {
    let base = common::read_fixture(path)?;
    common::edit_bytes(&base, path, |dom| {
        for value in values {
            common::insert_child(
                dom,
                "Folder",
                InstanceBuilder::new("StringValue")
                    .with_name("Item")
                    .with_property("Value", *value),
            )?;
        }
        Ok(())
    })
}

fn set_nth_item_value(dom: &mut rbx_dom_weak::WeakDom, index: usize, value: &str) -> Result<()> {
    let target = common::nth_child(dom, "Folder", index)?;
    dom.get_by_ref_mut(target)
        .expect("child should exist")
        .properties
        .insert(ustr("Value"), Variant::String(value.to_owned()));
    Ok(())
}

#[test]
fn independent_edits_to_same_named_siblings_merge() -> Result<()> {
    // Three identically-named children with no UniqueId. `ours` edits the first
    // and `theirs` edits the last. Positional matching keeps the three distinct,
    // so both edits land on the right sibling and the merge is clean — without
    // it, the unmatched siblings would become delete-plus-add noise.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = folder_with_items(&path, &["a", "b", "c"])?;
    let ours = common::edit_bytes(&base, &path, |dom| set_nth_item_value(dom, 0, "a2"))?;
    let theirs = common::edit_bytes(&base, &path, |dom| set_nth_item_value(dom, 2, "c2"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let folder = common::find_by_name(&decoded, "Folder")?;
    assert_eq!(
        common::child_string_values(&decoded, folder),
        vec!["a2".to_owned(), "b".to_owned(), "c2".to_owned()]
    );
    Ok(())
}

#[test]
fn same_named_sibling_match_reports_positional_diagnostic() -> Result<()> {
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = folder_with_items(&path, &["a", "b"])?;
    let ours = common::edit_bytes(&base, &path, |dom| set_nth_item_value(dom, 0, "a2"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path)?;
    let (_, diagnostics) = common::expect_clean(result);

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "positional_identity"),
        "expected a positional_identity diagnostic, got {diagnostics:#?}"
    );
    Ok(())
}

#[test]
fn unique_id_disambiguates_reordered_siblings() -> Result<()> {
    // Same-named siblings *with* UniqueIds are tracked by id, not position, so a
    // reorder on one side and an edit on the other compose without the edit
    // following the position. No positional fallback is needed.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let first_id = UniqueId::new(1, 1, 1);
    let second_id = UniqueId::new(2, 2, 2);
    let base = {
        let bytes = common::read_fixture(&path)?;
        common::edit_bytes(&bytes, &path, |dom| {
            for (id, value) in [(first_id, "a"), (second_id, "b")] {
                common::insert_child(
                    dom,
                    "Folder",
                    InstanceBuilder::new("StringValue")
                        .with_name("Item")
                        .with_property("UniqueId", Variant::UniqueId(id))
                        .with_property("Value", value),
                )?;
            }
            Ok(())
        })?
    };
    // ours moves the first item to the end; theirs edits that same item's value.
    let ours = common::edit_bytes(&base, &path, |dom| {
        let first = common::nth_child(dom, "Folder", 0)?;
        let folder = common::find_by_name(dom, "Folder")?;
        dom.transfer_within(first, folder);
        Ok(())
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| set_nth_item_value(dom, 0, "a2"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, diagnostics) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let folder = common::find_by_name(&decoded, "Folder")?;
    let value_of = |target_id: UniqueId| -> Option<String> {
        decoded
            .get_by_ref(folder)?
            .children()
            .iter()
            .find_map(|child| {
                let node = decoded.get_by_ref(*child)?;
                match node.properties.get(&ustr("UniqueId")) {
                    Some(Variant::UniqueId(id)) if *id == target_id => {
                        match node.properties.get(&ustr("Value")) {
                            Some(Variant::String(value)) => Some(value.clone()),
                            _ => None,
                        }
                    }
                    _ => None,
                }
            })
    };

    // The edit followed the UniqueId, not the slot it used to occupy.
    assert_eq!(value_of(first_id), Some("a2".to_owned()));
    assert_eq!(value_of(second_id), Some("b".to_owned()));
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "positional_identity"),
        "UniqueId matches should not need positional fallback, got {diagnostics:#?}"
    );
    Ok(())
}

#[test]
fn rename_without_unique_id_merges_with_concurrent_edit() -> Result<()> {
    // The case that otherwise conflicts: one side renames an instance with no
    // UniqueId while the other edits it. Rename recovery keeps them the same
    // instance, so the rename and the edit compose cleanly.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = {
        let bytes = common::read_fixture(&path)?;
        common::edit_bytes(&bytes, &path, |dom| {
            common::insert_child(
                dom,
                "Folder",
                InstanceBuilder::new("IntValue")
                    .with_name("Counter")
                    .with_property("Value", 1_i64),
            )?;
            Ok(())
        })?
    };
    let ours = common::edit_bytes(&base, &path, |dom| {
        common::rename_instance(dom, "Counter", "Tally")
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::set_property(dom, "Counter", "Value", 5_i64)
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, diagnostics) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let renamed = common::find_by_name(&decoded, "Tally")?;
    assert!(common::find_by_name(&decoded, "Counter").is_err());
    match decoded
        .get_by_ref(renamed)
        .and_then(|node| node.properties.get(&ustr("Value")))
    {
        Some(Variant::Int64(value)) => assert_eq!(*value, 5),
        other => panic!("expected the edited Value to survive the rename, got {other:?}"),
    }
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "renamed_instance"),
        "expected a renamed_instance diagnostic, got {diagnostics:#?}"
    );
    Ok(())
}

#[test]
fn rename_with_regenerated_unique_id_is_recovered() -> Result<()> {
    // Studio can rename an instance *and* regenerate its UniqueId at once. The
    // rename heuristic must still recognize it as one instance: the regenerated
    // UniqueId is volatile metadata excluded from the similarity metric, so it
    // does not drag the score below the rename threshold. Were it counted, the
    // instance would read as a delete on one side and an add on the other,
    // turning the concurrent edit on the other side into a delete/modify
    // conflict instead of a clean merge.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base_id = UniqueId::new(5, 5, 5);
    let regenerated = UniqueId::new(6, 6, 6);
    let base = {
        let bytes = common::read_fixture(&path)?;
        common::edit_bytes(&bytes, &path, |dom| {
            common::insert_child(
                dom,
                "Folder",
                InstanceBuilder::new("IntValue")
                    .with_name("Counter")
                    .with_property("UniqueId", Variant::UniqueId(base_id))
                    .with_property("Value", 1_i64),
            )?;
            Ok(())
        })?
    };
    // ours renames the instance and Studio regenerates its UniqueId.
    let ours = common::edit_bytes(&base, &path, |dom| {
        common::rename_instance(dom, "Counter", "Tally")?;
        common::set_property(dom, "Tally", "UniqueId", Variant::UniqueId(regenerated))
    })?;
    // theirs edits the same instance's value, keeping its original identity.
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::set_property(dom, "Counter", "Value", 5_i64)
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, diagnostics) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let renamed = common::find_by_name(&decoded, "Tally")?;
    assert!(common::find_by_name(&decoded, "Counter").is_err());
    match decoded
        .get_by_ref(renamed)
        .and_then(|node| node.properties.get(&ustr("Value")))
    {
        Some(Variant::Int64(value)) => assert_eq!(*value, 5),
        other => panic!("expected the edited Value to survive the rename, got {other:?}"),
    }
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "renamed_instance"),
        "expected a renamed_instance diagnostic, got {diagnostics:#?}"
    );
    Ok(())
}

#[test]
fn dissimilar_delete_and_add_is_not_a_rename() -> Result<()> {
    // A genuine delete-plus-add of a property-poor instance is not similar
    // enough to be paired as a rename, so a concurrent edit to the deleted
    // instance still surfaces as a delete/modify conflict rather than silently
    // following the unrelated addition.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = {
        let bytes = common::read_fixture(&path)?;
        common::edit_bytes(&bytes, &path, |dom| {
            common::insert_child(
                dom,
                "Folder",
                InstanceBuilder::new("IntValue")
                    .with_name("Old")
                    .with_property("Value", 1_i64),
            )?;
            Ok(())
        })?
    };
    let ours = common::edit_bytes(&base, &path, |dom| {
        common::delete_instance(dom, "Old")?;
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("IntValue")
                .with_name("New")
                .with_property("Value", 999_i64),
        )?;
        Ok(())
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::set_property(dom, "Old", "Value", 7_i64)
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (conflicts, diagnostics) = common::expect_conflicted(result);

    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::DeleteModify),
        "expected a DeleteModify conflict, got {conflicts:#?}"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "renamed_instance"),
        "dissimilar instances should not be matched as a rename, got {diagnostics:#?}"
    );
    Ok(())
}
