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
fn unique_id_pairing_resolves_before_heuristic_ambiguity() -> Result<()> {
    // Both sides add two identically-named siblings. One pair shares a UniqueId
    // (an exact match); the other pair has no UniqueId and can only be paired by
    // the (parent, class, name) heuristic. Resolving the UniqueId match first
    // leaves the no-id `theirs` addition with a single remaining candidate, so
    // it pairs cleanly. If the UniqueId pass did not run first, processing the
    // no-id addition before the UniqueId one would see two still-available
    // candidates and report a false ambiguity, splitting one instance into two.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let shared_id = UniqueId::new(7, 7, 7);
    let base = common::read_fixture(&path)?;

    // ours adds the UniqueId-bearing item first, then the no-id item.
    let ours = common::edit_bytes(&base, &path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("UniqueId", Variant::UniqueId(shared_id))
                .with_property("Value", "shared"),
        )?;
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("Value", "plain"),
        )?;
        Ok(())
    })?;
    // theirs adds the no-id item *first*, so document order would hand it to the
    // heuristic before the UniqueId match is resolved.
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("Value", "plain"),
        )?;
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("UniqueId", Variant::UniqueId(shared_id))
                .with_property("Value", "shared"),
        )?;
        Ok(())
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, diagnostics) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "ambiguous_identity"),
        "UniqueId-paired additions should not leave the no-id addition ambiguous, got {diagnostics:#?}"
    );

    let folder = common::find_by_name(&decoded, "Folder")?;
    let mut values = common::child_string_values(&decoded, folder);
    values.sort();
    // Both same-instance pairs collapse to one instance each: exactly two
    // children, not three.
    assert_eq!(values, vec!["plain".to_owned(), "shared".to_owned()]);
    Ok(())
}

/// The UniqueIds carried by a parent's direct children, sorted for a stable
/// comparison.
fn child_unique_ids(dom: &rbx_dom_weak::WeakDom, parent: rbx_types::Ref) -> Vec<UniqueId> {
    let mut ids: Vec<UniqueId> = dom
        .get_by_ref(parent)
        .map(|instance| {
            instance
                .children()
                .iter()
                .filter_map(|child| {
                    match dom.get_by_ref(*child)?.properties.get(&ustr("UniqueId")) {
                        Some(Variant::UniqueId(id)) => Some(*id),
                        _ => None,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    ids.sort_by_key(|id| id.to_string());
    ids
}

#[test]
fn identical_additions_dedupe_despite_differing_unique_ids() -> Result<()> {
    // Both sides independently add the *same* instance — same class, name, and
    // properties — but each carries its own UniqueId, because Studio assigns a
    // fresh id to every independently-created instance. Differing ids must not
    // veto the match: the content is identical (similarity 1.0), so the two are
    // recognized as one instance and deduped into a single child rather than
    // duplicated. This is the payoff of similarity gating.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let ours_id = UniqueId::new(1, 1, 1);
    let theirs_id = UniqueId::new(2, 2, 2);
    let base = common::read_fixture(&path)?;

    let ours = common::edit_bytes(&base, &path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("UniqueId", Variant::UniqueId(ours_id))
                .with_property("Value", "shared"),
        )?;
        Ok(())
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("UniqueId", Variant::UniqueId(theirs_id))
                .with_property("Value", "shared"),
        )?;
        Ok(())
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let folder = common::find_by_name(&decoded, "Folder")?;
    // Deduped to a single instance; the divergent UniqueId is resolved by the
    // merge lever, so exactly one id survives.
    assert_eq!(
        common::child_string_values(&decoded, folder),
        vec!["shared".to_owned()]
    );
    assert_eq!(child_unique_ids(&decoded, folder).len(), 1);
    Ok(())
}

#[test]
fn dissimilar_additions_with_differing_unique_ids_are_kept_distinct() -> Result<()> {
    // Both sides add a same-named child, but with different content *and*
    // different UniqueIds. Neither signal says "same instance": the ids differ
    // and the content is too dissimilar to clear the similarity threshold, so the
    // two are genuinely distinct additions. They must not be combined by name
    // alone — both survive, each keeping its own id.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let ours_id = UniqueId::new(1, 1, 1);
    let theirs_id = UniqueId::new(2, 2, 2);
    let base = common::read_fixture(&path)?;

    let ours = common::edit_bytes(&base, &path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("UniqueId", Variant::UniqueId(ours_id))
                .with_property("Value", "ours-only-content"),
        )?;
        Ok(())
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("UniqueId", Variant::UniqueId(theirs_id))
                .with_property("Value", "theirs-only-content"),
        )?;
        Ok(())
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let folder = common::find_by_name(&decoded, "Folder")?;
    // Both additions survive as separate instances, each keeping its own id.
    assert_eq!(child_unique_ids(&decoded, folder), {
        let mut expected = vec![ours_id, theirs_id];
        expected.sort_by_key(|id| id.to_string());
        expected
    });
    Ok(())
}

#[test]
fn differing_unique_id_addition_does_not_report_false_ambiguity() -> Result<()> {
    // `ours` adds two same-named siblings, each with its own UniqueId and
    // distinct content; `theirs` adds a third same-named sibling with yet another
    // UniqueId and its own distinct content. The `theirs` id matches neither
    // `ours` id, and its content is too dissimilar to either name-mate to clear
    // the similarity threshold, so it is a distinct new instance — not an
    // ambiguous match. No ambiguity is reported and all three instances survive.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let ours_a = UniqueId::new(1, 1, 1);
    let ours_b = UniqueId::new(2, 2, 2);
    let theirs_c = UniqueId::new(3, 3, 3);
    let base = common::read_fixture(&path)?;

    let ours = common::edit_bytes(&base, &path, |dom| {
        for (id, value) in [(ours_a, "a"), (ours_b, "b")] {
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
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("UniqueId", Variant::UniqueId(theirs_c))
                .with_property("Value", "c"),
        )?;
        Ok(())
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, diagnostics) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "ambiguous_identity"),
        "an addition with an unmatched UniqueId is a new instance, not ambiguous, got {diagnostics:#?}"
    );

    let folder = common::find_by_name(&decoded, "Folder")?;
    assert_eq!(child_unique_ids(&decoded, folder), {
        let mut expected = vec![ours_a, ours_b, theirs_c];
        expected.sort_by_key(|id| id.to_string());
        expected
    });
    Ok(())
}

#[test]
fn one_ours_addition_contested_by_two_similar_theirs_is_ambiguous() -> Result<()> {
    // `ours` adds a single no-UniqueId child; `theirs` adds two with the same
    // (parent, class, name) and identical content, so both are equally good
    // similarity matches for the lone `ours` candidate. The pairing is 1:1, so
    // the candidate cannot be assigned to one of the two without an arbitrary,
    // order-dependent guess. This is the mirror of "two `ours` candidates, one
    // `theirs`" and must be declined the same way: report ambiguity and keep all
    // three additions distinct, rather than letting whichever `theirs` comes
    // first in document order claim the match.
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = common::read_fixture(&path)?;

    let ours = common::edit_bytes(&base, &path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue")
                .with_name("Item")
                .with_property("Value", "same"),
        )?;
        Ok(())
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| {
        for _ in 0..2 {
            common::insert_child(
                dom,
                "Folder",
                InstanceBuilder::new("StringValue")
                    .with_name("Item")
                    .with_property("Value", "same"),
            )?;
        }
        Ok(())
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, diagnostics) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "ambiguous_identity"),
        "a lone ours candidate contested by two similar theirs additions is ambiguous, got {diagnostics:#?}"
    );

    let folder = common::find_by_name(&decoded, "Folder")?;
    // No arbitrary pairing: all three additions survive distinctly.
    assert_eq!(
        common::child_string_values(&decoded, folder),
        vec!["same".to_owned(), "same".to_owned(), "same".to_owned()]
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
