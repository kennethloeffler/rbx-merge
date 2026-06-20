//! Property-based invariants for the merge engine. Each case derives `ours`
//! and `theirs` from a common base by applying randomized edit sequences, then
//! asserts merge invariants that must hold regardless of the edits.

use std::path::Path;

use proptest::prelude::*;
use rbx_dom_weak::{InstanceBuilder, WeakDom, ustr};
use rbx_types::Variant;

use super::common;
use crate::{FileInput, MergeSettings, merge_files, textconv};

/// A single randomized edit. Instances are addressed by index into the current
/// descendant list (taken modulo its length), so an edit always targets some
/// live instance even after earlier edits reshape the tree.
///
/// Renames and additions deliberately do *not* carry a name: names are drawn
/// from a per-side counter so they are unique within a side. Two instances with
/// the same name and class under one parent have no stable identity under the
/// heuristic matcher (reported as an `ambiguous_identity` diagnostic), so the
/// merge cannot — and these invariants do not — promise canonical output for
/// them.
#[derive(Debug, Clone)]
enum Edit {
    SetProbe(usize, u8),
    Rename(usize),
    AddChild(usize),
    Delete(usize),
    MoveToEnd(usize),
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (any::<usize>(), 0u8..4).prop_map(|(i, n)| Edit::SetProbe(i, n)),
        any::<usize>().prop_map(Edit::Rename),
        any::<usize>().prop_map(Edit::AddChild),
        any::<usize>().prop_map(Edit::Delete),
        any::<usize>().prop_map(Edit::MoveToEnd),
    ]
}

/// Name space for derived instances: effectively unbounded names keep every
/// instance distinct, while a small space forces same-name/same-class siblings.
const UNIQUE_NAMES: u32 = u32::MAX;
const FEW_NAMES: u32 = 2;

fn apply_edit(dom: &mut WeakDom, edit: &Edit, counter: &mut u32, name_space: u32) {
    let root = dom.root_ref();
    let targets: Vec<_> = dom
        .descendants()
        .map(|instance| instance.referent())
        .filter(|referent| *referent != root)
        .collect();
    if targets.is_empty() {
        return;
    }
    let pick = |index: usize| targets[index % targets.len()];
    let mut fresh_name = |prefix: char| {
        let name = format!("{prefix}{}", *counter % name_space);
        *counter += 1;
        name
    };

    match *edit {
        Edit::SetProbe(index, value) => {
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                instance
                    .properties
                    .insert(ustr("Probe"), Variant::String(format!("v{value}")));
            }
        }
        Edit::Rename(index) => {
            let name = fresh_name('R');
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                instance.name = name;
            }
        }
        Edit::AddChild(index) => {
            let name = fresh_name('A');
            dom.insert(pick(index), InstanceBuilder::new("StringValue").with_name(name));
        }
        Edit::Delete(index) => {
            dom.destroy(pick(index));
        }
        Edit::MoveToEnd(index) => {
            let referent = pick(index);
            if let Some(parent) = dom.get_by_ref(referent).map(|instance| instance.parent())
                && parent.is_some()
            {
                dom.transfer_within(referent, parent);
            }
        }
    }
}

fn derive(base: &[u8], path: &Path, edits: &[Edit], name_space: u32) -> Vec<u8> {
    let mut dom = common::decode_bytes(base, path).expect("decode base fixture");
    let mut counter = 0;
    for edit in edits {
        apply_edit(&mut dom, edit, &mut counter, name_space);
    }
    common::encode_fixture(&dom, path).expect("encode derived side")
}

fn semantic_text(bytes: &[u8], path: &Path) -> String {
    textconv(bytes, Some(path)).expect("textconv")
}

/// Merge the three sides, returning the merged bytes or `None` on conflict.
fn merged_bytes(base: &[u8], ours: &[u8], theirs: &[u8], path: &Path) -> Option<Vec<u8>> {
    merge_files(
        FileInput::new(base).with_path_hint(path),
        FileInput::new(ours).with_path_hint(path),
        FileInput::new(theirs).with_path_hint(path),
        MergeSettings::default(),
    )
    .expect("merge should not error")
    .merged
}

/// Merge the three sides and return the merged output's semantic text, or
/// `None` if the merge reported conflicts.
fn merged_text(base: &[u8], ours: &[u8], theirs: &[u8], path: &Path) -> Option<String> {
    merged_bytes(base, ours, theirs, path).map(|bytes| semantic_text(&bytes, path))
}

/// A side put through the merge's own normalization (e.g. dropping all-empty
/// `Attributes`), via a self-merge. This is the right yardstick for one-sided
/// invariants: the merge of a one-sided change equals the *normalized* side,
/// not the raw side.
fn normalized(side: &[u8], path: &Path) -> String {
    merged_text(side, side, side, path).expect("self-merge is always clean")
}

#[test]
fn no_op_merge_is_clean_and_stable() {
    let path = common::model_path("three-intvalues", "xml.rbxmx");
    let base = common::read_fixture(&path).expect("read base");

    let report = merge_files(
        FileInput::new(&base).with_path_hint(&path),
        FileInput::new(&base).with_path_hint(&path),
        FileInput::new(&base).with_path_hint(&path),
        MergeSettings::default(),
    )
    .expect("merge");
    let merged = report.merged.expect("no-op merge should be clean");

    // Re-merging the merged output with itself changes nothing (idempotence).
    assert_eq!(semantic_text(&merged, &path), normalized(&merged, &path));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// With every instance distinctly named, a change on one side with the other
    /// unchanged always merges cleanly and reproduces the normalized changed
    /// side. Conflict detection is symmetric under swapping `ours` and `theirs`.
    #[test]
    fn merge_invariants(
        ours_edits in prop::collection::vec(edit_strategy(), 0..6),
        theirs_edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", "xml.rbxmx");
        let base = common::read_fixture(&path).expect("read base");
        let ours = derive(&base, &path, &ours_edits, UNIQUE_NAMES);
        let theirs = derive(&base, &path, &theirs_edits, UNIQUE_NAMES);

        prop_assert_eq!(
            merged_text(&base, &ours, &base, &path),
            Some(normalized(&ours, &path))
        );
        prop_assert_eq!(
            merged_text(&base, &base, &theirs, &path),
            Some(normalized(&theirs, &path))
        );

        let forward_clean = merged_text(&base, &ours, &theirs, &path).is_some();
        let backward_clean = merged_text(&base, &theirs, &ours, &path).is_some();
        prop_assert_eq!(forward_clean, backward_clean);
    }

    /// With heavy same-name/same-class collisions the stronger invariants no
    /// longer hold: positional matching can misattribute reordered siblings (so
    /// the merge need not equal the normalized side), and conflict detection is
    /// not symmetric under swapping sides (added-instance matching is
    /// directional, so ambiguous duplicate additions resolve differently each
    /// way). Idempotence does still hold — a self-merge is order-stable — so it
    /// is the invariant worth pinning down for duplicates.
    #[test]
    fn duplicate_name_idempotence(
        ours_edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", "xml.rbxmx");
        let base = common::read_fixture(&path).expect("read base");
        let ours = derive(&base, &path, &ours_edits, FEW_NAMES);

        let once = merged_bytes(&ours, &ours, &ours, &path).expect("self-merge is clean");
        let twice = merged_bytes(&once, &once, &once, &path).expect("self-merge is clean");
        prop_assert_eq!(semantic_text(&once, &path), semantic_text(&twice, &path));
    }
}
