use anyhow::Result;
use rbx_dom_weak::{InstanceBuilder, ustr};
use rbx_types::{UniqueId, Variant};

use super::common;
use crate::{ConflictKind, DiagnosticSeverity, FileInput, MergeSettings, merge_files, textconv};

#[test]
fn clean_one_sided_property_edit_snapshots_output() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 9001_i64)
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path)?;
    let (merged, diagnostics) = common::expect_clean(result);

    common::with_path_redaction(|| {
        insta::assert_debug_snapshot!("clean_one_sided_property_edit_diagnostics", diagnostics);
    });
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
fn regenerated_unique_id_diverges_three_ways_but_merges_cleanly() -> Result<()> {
    // Studio regenerates the UniqueId of some instances (e.g. Welds) when a place
    // is opened, so both sides can independently carry a different UniqueId for an
    // instance that is otherwise unchanged. The instance still matches by
    // structure, and the divergent UniqueId — base, ours, theirs all distinct —
    // must resolve cleanly to the base value rather than surfacing a conflict the
    // user cannot meaningfully resolve.
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base_id = UniqueId::new(1, 1, 1);
    let ours_id = UniqueId::new(2, 2, 2);
    let theirs_id = UniqueId::new(3, 3, 3);

    let base = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "UniqueId", Variant::UniqueId(base_id))
    })?;
    let ours = common::edit_bytes(&base, &path, |dom| {
        common::set_property(dom, "Value=1337", "UniqueId", Variant::UniqueId(ours_id))
    })?;
    let theirs = common::edit_bytes(&base, &path, |dom| {
        common::set_property(dom, "Value=1337", "UniqueId", Variant::UniqueId(theirs_id))
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let target = common::find_by_name(&decoded, "Value=1337")?;
    let merged_id = match decoded
        .get_by_ref(target)
        .and_then(|node| node.properties.get(&ustr("UniqueId")))
    {
        Some(Variant::UniqueId(id)) => Some(*id),
        _ => None,
    };
    assert_eq!(
        merged_id,
        Some(base_id),
        "the matched instance should keep the base UniqueId"
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

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
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

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path)?;
    let (merged, diagnostics) = common::expect_clean(result);

    common::with_path_redaction(|| {
        insta::assert_debug_snapshot!("clean_one_sided_add_diagnostics", diagnostics);
    });
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

    let result = common::merge_fixture_bytes(&base, &base, &base, &path)?;
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

#[test]
fn clean_one_sided_rename_keeps_new_name() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::rename_instance(dom, "Value=1337", "Renamed")
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let names = common::child_names(&decoded, decoded.root_ref());
    assert!(
        names.contains(&"Renamed".to_owned()),
        "names were {names:?}"
    );
    assert!(!names.contains(&"Value=1337".to_owned()));
    Ok(())
}

#[test]
fn clean_one_sided_delete_removes_instance() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| common::delete_instance(dom, "Value=1337"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let names = common::child_names(&decoded, decoded.root_ref());
    assert_eq!(names.len(), 2);
    assert!(!names.contains(&"Value=1337".to_owned()));
    Ok(())
}

#[test]
fn delete_versus_modify_conflicts() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| common::delete_instance(dom, "Value=1337"))?;
    let theirs = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 42_i64)
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (conflicts, _) = common::expect_conflicted(result);

    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::DeleteModify),
        "expected a DeleteModify conflict, got {conflicts:#?}"
    );
    Ok(())
}

#[test]
fn child_order_independent_additions_merge_cleanly() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::insert_child_at_root(dom, InstanceBuilder::new("StringValue").with_name("OurAdd"));
        Ok(())
    })?;
    let theirs = common::edit_fixture(&path, |dom| {
        common::insert_child_at_root(
            dom,
            InstanceBuilder::new("StringValue").with_name("TheirAdd"),
        );
        Ok(())
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let names = common::child_names(&decoded, decoded.root_ref());
    assert!(names.contains(&"OurAdd".to_owned()), "names were {names:?}");
    assert!(
        names.contains(&"TheirAdd".to_owned()),
        "names were {names:?}"
    );
    assert_eq!(names.len(), 5);
    Ok(())
}

#[test]
fn child_order_divergent_reorder_conflicts() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| common::move_to_end(dom, "Value=1234567"))?;
    let theirs = common::edit_fixture(&path, |dom| common::move_to_end(dom, "Value=1337"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (conflicts, _) = common::expect_conflicted(result);

    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::ChildOrder),
        "expected a ChildOrder conflict, got {conflicts:#?}"
    );
    Ok(())
}

#[test]
fn parent_move_one_sided_merges_cleanly() -> Result<()> {
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = common::edit_fixture(&path, build_reparent_scaffold)?;
    let ours = common::edit_bytes(&base, &path, |dom| common::reparent(dom, "Leaf", "P1"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path)?;
    let (merged, _) = common::expect_clean(result);
    let decoded = common::decode_bytes(&merged, &path)?;

    let p1 = common::find_by_name(&decoded, "P1")?;
    assert!(common::child_names(&decoded, p1).contains(&"Leaf".to_owned()));
    Ok(())
}

#[test]
fn parent_move_divergent_conflicts() -> Result<()> {
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = common::edit_fixture(&path, build_reparent_scaffold)?;
    let ours = common::edit_bytes(&base, &path, |dom| common::reparent(dom, "Leaf", "P1"))?;
    let theirs = common::edit_bytes(&base, &path, |dom| common::reparent(dom, "Leaf", "P2"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (conflicts, _) = common::expect_conflicted(result);

    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::ParentMove),
        "expected a ParentMove conflict, got {conflicts:#?}"
    );
    Ok(())
}

#[test]
fn mutual_reparent_cycle_conflicts() -> Result<()> {
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    // Two folders at the root, each carrying a `UniqueId` so identity matching
    // tracks them across a move.
    let base = common::edit_fixture(&path, |dom| {
        common::insert_child_at_root(
            dom,
            InstanceBuilder::new("Folder")
                .with_name("A")
                .with_property("UniqueId", Variant::UniqueId(UniqueId::new(1, 1, 1))),
        );
        common::insert_child_at_root(
            dom,
            InstanceBuilder::new("Folder")
                .with_name("B")
                .with_property("UniqueId", Variant::UniqueId(UniqueId::new(2, 2, 2))),
        );
        Ok(())
    })?;
    // ours moves A under B; theirs moves B under A. Each move is clean alone, but
    // together they form an A -> B -> A parent cycle.
    let ours = common::edit_bytes(&base, &path, |dom| common::reparent(dom, "A", "B"))?;
    let theirs = common::edit_bytes(&base, &path, |dom| common::reparent(dom, "B", "A"))?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (conflicts, _) = common::expect_conflicted(result);

    assert!(
        conflicts
            .iter()
            .any(|conflict| conflict.kind == ConflictKind::ParentCycle),
        "expected a ParentCycle conflict rather than silent data loss, got {conflicts:#?}"
    );
    Ok(())
}

/// Adds two sibling folders `P1`/`P2` and a `Leaf` under the existing `Folder`.
/// `Leaf` carries a `UniqueId` so identity matching tracks it across a move
/// instead of reading the move as a delete plus an add.
fn build_reparent_scaffold(dom: &mut rbx_dom_weak::WeakDom) -> Result<()> {
    common::insert_child_at_root(dom, InstanceBuilder::new("Folder").with_name("P1"));
    common::insert_child_at_root(dom, InstanceBuilder::new("Folder").with_name("P2"));
    common::insert_child(
        dom,
        "Folder",
        InstanceBuilder::new("StringValue")
            .with_name("Leaf")
            .with_property("UniqueId", Variant::UniqueId(UniqueId::new(7, 7, 7))),
    )?;
    Ok(())
}

#[test]
fn ambiguous_added_match_reports_diagnostic() -> Result<()> {
    let path = common::model_path("default-inserted-folder", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue").with_name("Dup"),
        )?;
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue").with_name("Dup"),
        )?;
        Ok(())
    })?;
    let theirs = common::edit_fixture(&path, |dom| {
        common::insert_child(
            dom,
            "Folder",
            InstanceBuilder::new("StringValue").with_name("Dup"),
        )?;
        Ok(())
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &theirs, &path)?;
    let (_, diagnostics) = common::expect_clean(result);

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "ambiguous_identity"),
        "expected an ambiguous_identity diagnostic, got {diagnostics:#?}"
    );
    Ok(())
}

#[test]
fn unknown_property_is_reported() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "CustomMergeField", "hello")
    })?;

    let result = common::merge_fixture_bytes(&base, &ours, &base, &path)?;
    let (_, diagnostics) = common::expect_clean(result);

    let unknown = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "unknown_property")
        .unwrap_or_else(|| panic!("expected an unknown_property diagnostic, got {diagnostics:#?}"));
    assert_eq!(unknown.severity, DiagnosticSeverity::Info);
    assert!(unknown.message.contains("CustomMergeField"));
    Ok(())
}

#[test]
fn binary_no_op_merge_round_trips() -> Result<()> {
    let path = common::model_path("attributes", "binary.rbxm");
    let base = common::read_fixture(&path)?;

    let result = common::merge_fixture_bytes(&base, &base, &base, &path)?;
    let (merged, _) = common::expect_clean(result);

    assert!(merged.starts_with(b"<roblox!"), "output was not binary");
    // Re-decoding must succeed and preserve the instance count.
    let original = common::decode_bytes(&base, &path)?;
    let decoded = common::decode_bytes(&merged, &path)?;
    assert_eq!(
        decoded.root().children().len(),
        original.root().children().len()
    );
    Ok(())
}

#[test]
fn merge_files_report_exposes_clean_output() -> Result<()> {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path)?;
    let ours = common::edit_fixture(&path, |dom| {
        common::set_property(dom, "Value=1337", "Value", 9001_i64)
    })?;

    let report = merge_files(
        FileInput::new(&base).with_path_hint(&path),
        FileInput::new(&ours).with_path_hint(&path),
        FileInput::new(&base).with_path_hint(&path),
        MergeSettings::default(),
    )?;

    assert!(report.is_clean());
    assert!(report.conflicts.is_empty());
    assert!(report.merged.is_some());
    Ok(())
}
