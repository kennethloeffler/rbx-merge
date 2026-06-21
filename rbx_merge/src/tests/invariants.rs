//! Property-based invariants for the merge engine. Each case derives `ours`
//! and `theirs` from a common base by applying randomized edit sequences, then
//! asserts merge invariants that must hold regardless of the edits.

use std::collections::HashSet;
use std::path::Path;

use proptest::prelude::*;
use rbx_dom_weak::{InstanceBuilder, WeakDom, ustr};
use rbx_types::{Attributes, CFrame, Color3, Matrix3, Ref, UniqueId, Variant, Vector3};

use super::common;
use crate::{FileInput, MergeSettings, Resolutions, Side, merge_files, textconv};

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
    /// Set a synthetic property (chosen from `PROPS` by index) to a value, where
    /// the index also selects the Variant type. Seeded on the base, so on base
    /// instances this is a change; on freshly added instances it is an add.
    SetProp(usize, usize, u8),
    /// Remove a synthetic property, exercising a present→absent transition.
    RemoveProp(usize, usize),
    /// Set an attribute, exercising the key-level `Attributes` merge.
    SetAttribute(usize, u8),
    /// Replace `Attributes` with an empty map, exercising empty-attribute
    /// preservation through the merge.
    ClearAttributes(usize),
    Rename(usize),
    /// Add a child of a class chosen from `ADD_CLASSES` by index.
    AddChild(usize, usize),
    Delete(usize),
    MoveToEnd(usize),
    /// Move an instance under a *different* parent, exercising the parent-move
    /// merge (and `ParentMove` conflicts when both sides move it differently).
    Reparent(usize, usize),
    /// Overwrite an instance's `UniqueId` with a fresh value, simulating Studio
    /// regenerating it (as it does for e.g. Welds when a place is opened). The
    /// instance still matches by structure, so this exercises the authoritative
    /// UniqueId matching path and Lever 1's resolution of a divergent UniqueId.
    Regenerate(usize),
    /// Point a synthetic `Ref` property at another instance (chosen by index),
    /// exercising the merge's reference rewriting. When the target is later
    /// deleted on a side, the reference dangles — the dropped-reference path.
    SetRef(usize, usize),
}

/// The Variant type written under a given synthetic property name.
#[derive(Debug, Clone, Copy)]
enum PropKind {
    Str,
    Int,
    Bool,
    Float,
    Vector,
    Cframe,
    Color,
}

/// Synthetic properties the edits write. Each name is bound to one Variant type:
/// the binary codec keys a property's type by `(class, name)`, so a name must be
/// one consistent type everywhere. These are deliberately *not* reflected
/// properties, which exercises the merge's type-agnostic property handling
/// uniformly across whatever class an edit happens to land on.
const PROPS: &[(&str, PropKind)] = &[
    ("Alpha", PropKind::Str),
    ("Bravo", PropKind::Int),
    ("Charlie", PropKind::Bool),
    ("Delta", PropKind::Float),
    ("Echo", PropKind::Vector),
    ("Foxtrot", PropKind::Cframe),
    ("Golf", PropKind::Color),
];

/// Classes `AddChild` draws from, so additions span several classes rather than
/// a single one.
const ADD_CLASSES: &[&str] = &[
    "Folder",
    "StringValue",
    "IntValue",
    "BoolValue",
    "NumberValue",
    "Configuration",
    "Model",
    "ObjectValue",
];

/// A property value for `kind`, derived from a small `seed` so the two sides
/// frequently agree (clean) or differ (conflict). Float-bearing types use
/// integer-valued components so they round-trip exactly through both codecs.
fn prop_value(kind: PropKind, seed: u8) -> Variant {
    let f = f32::from(seed);
    match kind {
        PropKind::Str => Variant::String(format!("s{seed}")),
        PropKind::Int => Variant::Int64(i64::from(seed)),
        PropKind::Bool => Variant::Bool(seed.is_multiple_of(2)),
        PropKind::Float => Variant::Float64(f64::from(seed)),
        PropKind::Vector => Variant::Vector3(Vector3::new(f, f, f)),
        PropKind::Cframe => {
            Variant::CFrame(CFrame::new(Vector3::new(f, f, f), Matrix3::identity()))
        }
        PropKind::Color => Variant::Color3(Color3::new(f, f, f)),
    }
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (any::<usize>(), any::<usize>(), 0u8..4).prop_map(|(i, p, v)| Edit::SetProp(i, p, v)),
        (any::<usize>(), any::<usize>()).prop_map(|(i, p)| Edit::RemoveProp(i, p)),
        (any::<usize>(), 0u8..4).prop_map(|(i, n)| Edit::SetAttribute(i, n)),
        any::<usize>().prop_map(Edit::ClearAttributes),
        any::<usize>().prop_map(Edit::Rename),
        (any::<usize>(), any::<usize>()).prop_map(|(i, c)| Edit::AddChild(i, c)),
        any::<usize>().prop_map(Edit::Delete),
        any::<usize>().prop_map(Edit::MoveToEnd),
        (any::<usize>(), any::<usize>()).prop_map(|(i, p)| Edit::Reparent(i, p)),
        any::<usize>().prop_map(Edit::Regenerate),
        (any::<usize>(), any::<usize>()).prop_map(|(i, t)| Edit::SetRef(i, t)),
    ]
}

/// Edit sequences excluding `Regenerate`, for invariants that track instances by
/// `UniqueId`: regenerating an id would make a surviving instance look like a
/// deletion followed by an unrelated addition to that tracking.
fn non_regenerating_edits() -> impl Strategy<Value = Vec<Edit>> {
    prop::collection::vec(
        edit_strategy().prop_filter("excludes Regenerate", |edit| {
            !matches!(edit, Edit::Regenerate(_))
        }),
        0..6,
    )
}

/// Name space for derived instances: effectively unbounded names keep every
/// instance distinct, while a small space forces same-name/same-class siblings.
const UNIQUE_NAMES: u32 = u32::MAX;
const FEW_NAMES: u32 = 2;

/// The fixture variants to search, covering both codecs (the chosen file name
/// drives the codec via its extension). They round-trip values differently: the
/// binary format shares one wire type for `String` and `BinaryString`, so a
/// property the reflection database doesn't describe comes back as a
/// `BinaryString`. Invariants compared across sides still hold (every side takes
/// the same round-trip), but absolute-value assertions must account for it (see
/// `set_attribute_on`).
fn format_strategy() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just("xml.rbxmx"), Just("binary.rbxm")]
}

/// A fresh name unique within a side, drawn from the side's `counter` modulo
/// `name_space` (a small space forces same-name/same-class collisions).
fn fresh_name(counter: &mut u32, name_space: u32, prefix: char) -> String {
    let name = format!("{prefix}{}", *counter % name_space);
    *counter += 1;
    name
}

/// A fresh, distinct `UniqueId` for simulating Studio regeneration. `salt`
/// separates the two sides so the same instance regenerated on each side gets a
/// different id — the three-way-divergent case Lever 1 must resolve. The
/// `time`/`random` fields are held away from the seed ids' fields (see
/// `seed_base`) so a regenerated id never collides with a seeded one.
fn fresh_unique_id(counter: &mut u32, salt: u32) -> UniqueId {
    let id = UniqueId::new(*counter, 50 + salt, i64::from(50 + salt));
    *counter += 1;
    id
}

fn apply_edit(dom: &mut WeakDom, edit: &Edit, counter: &mut u32, name_space: u32, salt: u32) {
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

    match *edit {
        Edit::SetProp(index, prop, value) => {
            let (name, kind) = PROPS[prop % PROPS.len()];
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                instance
                    .properties
                    .insert(ustr(name), prop_value(kind, value));
            }
        }
        Edit::RemoveProp(index, prop) => {
            let (name, _) = PROPS[prop % PROPS.len()];
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                instance.properties.remove(&ustr(name));
            }
        }
        Edit::SetAttribute(index, value) => {
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                let mut attributes = match instance.properties.get(&ustr("Attributes")) {
                    Some(Variant::Attributes(existing)) => existing.clone(),
                    _ => Attributes::new(),
                };
                attributes.insert("Attr".to_owned(), Variant::String(format!("a{value}")));
                instance
                    .properties
                    .insert(ustr("Attributes"), Variant::Attributes(attributes));
            }
        }
        Edit::ClearAttributes(index) => {
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                instance
                    .properties
                    .insert(ustr("Attributes"), Variant::Attributes(Attributes::new()));
            }
        }
        Edit::Rename(index) => {
            let name = fresh_name(counter, name_space, 'R');
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                instance.name = name;
            }
        }
        Edit::AddChild(index, class) => {
            let name = fresh_name(counter, name_space, 'A');
            let class = ADD_CLASSES[class % ADD_CLASSES.len()];
            dom.insert(pick(index), InstanceBuilder::new(class).with_name(name));
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
        Edit::Reparent(index, parent_index) => {
            let referent = pick(index);
            let new_parent = pick(parent_index);
            // Refuse to create a cycle: the new parent must not be the instance
            // itself or one of its descendants (`descendants_of` includes the
            // instance). Reparenting under another non-root instance also adds
            // tree depth the flat fixture otherwise lacks.
            let would_cycle = dom
                .descendants_of(referent)
                .any(|instance| instance.referent() == new_parent);
            if !would_cycle {
                dom.transfer_within(referent, new_parent);
            }
        }
        Edit::Regenerate(index) => {
            let id = fresh_unique_id(counter, salt);
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                instance
                    .properties
                    .insert(ustr("UniqueId"), Variant::UniqueId(id));
            }
        }
        Edit::SetRef(index, target_index) => {
            let target = pick(target_index);
            if let Some(instance) = dom.get_by_ref_mut(pick(index)) {
                instance
                    .properties
                    .insert(ustr("RefProbe"), Variant::Ref(target));
            }
        }
    }
}

fn derive(base: &[u8], path: &Path, edits: &[Edit], name_space: u32, salt: u32) -> Vec<u8> {
    let mut dom = common::decode_bytes(base, path).expect("decode base fixture");
    let mut counter = 0;
    for edit in edits {
        apply_edit(&mut dom, edit, &mut counter, name_space, salt);
    }
    common::encode_fixture(&dom, path).expect("encode derived side")
}

/// Prepare the base: give every non-root instance a distinct `UniqueId` so the
/// derived sides share stable ids and the merge exercises the authoritative
/// UniqueId matching path (not just the positional/rename fallback), plus a
/// baseline value for every `PROPS` entry so `SetProp` is a real change and
/// `RemoveProp` a real deletion against a value present in the base. (Adds of a
/// previously-absent property are still exercised via freshly added instances,
/// which carry none of these.) Seed ids keep their `time`/`random` fields
/// disjoint from regenerated ids (see `fresh_unique_id`) so the two never
/// collide within a side.
fn seed_base(base: &[u8], path: &Path) -> Vec<u8> {
    let mut dom = common::decode_bytes(base, path).expect("decode base fixture");
    let root = dom.root_ref();
    let targets: Vec<_> = dom
        .descendants()
        .map(|instance| instance.referent())
        .filter(|referent| *referent != root)
        .collect();
    for (index, referent) in targets.into_iter().enumerate() {
        if let Some(instance) = dom.get_by_ref_mut(referent) {
            instance.properties.insert(
                ustr("UniqueId"),
                Variant::UniqueId(UniqueId::new(index as u32 + 1, 1, 1)),
            );
            for &(name, kind) in PROPS {
                instance.properties.insert(ustr(name), prop_value(kind, 0));
            }
        }
    }
    common::encode_fixture(&dom, path).expect("encode seeded base")
}

fn semantic_text(bytes: &[u8], path: &Path) -> String {
    let text = textconv(bytes, Some(path)).expect("textconv");
    canonicalize_volatile_unique_ids(&text)
}

/// Replace every rendered `UniqueId` value with a fixed placeholder.
///
/// A `UniqueId` is not a deterministic function of the bytes it was decoded
/// from: `WeakDom` regenerates one with a fresh `UniqueId::now()` (a
/// process-global counter plus wall clock and RNG) whenever two instances share
/// an id on decode — including the nil id that id-less instances (e.g. freshly
/// added ones) serialize as, since the codec fills the column with a nil
/// default. So decoding the *same* bytes twice can yield different ids. That
/// nondeterminism is rbx-dom's, not the merge's: `UniqueId` is volatile identity
/// metadata the engine already treats as such (see `is_volatile_property`).
/// Canonicalizing it here keeps the determinism and stability invariants
/// measuring the merge's own output rather than rbx-dom's id regeneration.
fn canonicalize_volatile_unique_ids(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        match line.split_once("UniqueId = ") {
            Some((indent, _)) => {
                out.push_str(indent);
                out.push_str("UniqueId = <volatile>");
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
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

/// Merge the three sides under an explicit resolution policy, returning the
/// merged bytes or `None` on conflict.
fn merged_bytes_resolved(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    path: &Path,
    resolutions: Resolutions,
) -> Option<Vec<u8>> {
    merge_files(
        FileInput::new(base).with_path_hint(path),
        FileInput::new(ours).with_path_hint(path),
        FileInput::new(theirs).with_path_hint(path),
        MergeSettings {
            resolutions,
            ..MergeSettings::default()
        },
    )
    .expect("merge should not error")
    .merged
}

/// Merge the three sides and return the merged output's semantic text, or
/// `None` if the merge reported conflicts.
fn merged_text(base: &[u8], ours: &[u8], theirs: &[u8], path: &Path) -> Option<String> {
    merged_bytes(base, ours, theirs, path).map(|bytes| semantic_text(&bytes, path))
}

/// A side put through the merge's own normalization (e.g. child reordering and
/// ref rewriting), via a self-merge. This is the right yardstick for one-sided
/// invariants: the merge of a one-sided change equals the *normalized* side,
/// not the raw side.
fn normalized(side: &[u8], path: &Path) -> String {
    merged_text(side, side, side, path).expect("self-merge is always clean")
}

/// The base's instance set: its non-root descendant count. Stable across
/// property-only edits, so an edit index `i` maps to the same instance on every
/// side.
fn nonroot_count(base: &[u8], path: &Path) -> usize {
    let dom = common::decode_bytes(base, path).expect("decode base fixture");
    let root = dom.root_ref();
    dom.descendants()
        .filter(|instance| instance.referent() != root)
        .count()
}

/// Add attribute `key` to each targeted non-root instance, indexing modulo the
/// instance count exactly as `apply_edit` does. Attributes are a reflected
/// property with a known type, so their values survive both codecs unchanged —
/// unlike a synthetic top-level `String`, which the binary codec round-trips as
/// a `BinaryString` (see `format_strategy`) and which an absolute assertion on
/// `Variant::String` would then miss.
fn set_attribute_on(base: &[u8], path: &Path, targets: &[usize], key: &str) -> Vec<u8> {
    let mut dom = common::decode_bytes(base, path).expect("decode base fixture");
    let root = dom.root_ref();
    let instances: Vec<_> = dom
        .descendants()
        .map(|instance| instance.referent())
        .filter(|referent| *referent != root)
        .collect();
    if !instances.is_empty() {
        for &target in targets {
            let referent = instances[target % instances.len()];
            if let Some(instance) = dom.get_by_ref_mut(referent) {
                let mut attributes = match instance.properties.get(&ustr("Attributes")) {
                    Some(Variant::Attributes(existing)) => existing.clone(),
                    _ => Attributes::new(),
                };
                attributes.insert(key.to_owned(), Variant::Bool(true));
                instance
                    .properties
                    .insert(ustr("Attributes"), Variant::Attributes(attributes));
            }
        }
    }
    common::encode_fixture(&dom, path).expect("encode side")
}

/// The set of non-nil `UniqueId`s carried by the non-root instances of a file.
/// This is the merge's authoritative identity, so it is the right key for
/// tracking which instances survived a merge. Instances without a `UniqueId`
/// (e.g. freshly added ones, which serialize as nil) are excluded — they have no
/// stable identity to follow across sides.
fn nonroot_unique_ids(bytes: &[u8], path: &Path) -> HashSet<UniqueId> {
    let dom = common::decode_bytes(bytes, path).expect("decode file");
    let root = dom.root_ref();
    dom.descendants()
        .filter(|instance| instance.referent() != root)
        .filter_map(
            |instance| match instance.properties.get(&ustr("UniqueId")) {
                Some(Variant::UniqueId(id)) if !id.is_nil() => Some(*id),
                _ => None,
            },
        )
        .collect()
}

/// Number of instances whose `Attributes` map contains `key`.
fn instances_with_attribute(dom: &WeakDom, key: &str) -> usize {
    dom.descendants()
        .filter(|instance| {
            matches!(
                instance.properties.get(&ustr("Attributes")),
                Some(Variant::Attributes(attributes)) if attributes.get(key).is_some()
            )
        })
        .count()
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
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// With every instance distinctly named, a change on one side with the other
    /// unchanged always merges cleanly and reproduces the normalized changed
    /// side. Conflict detection is symmetric under swapping `ours` and `theirs`.
    #[test]
    fn merge_invariants(
        file_name in format_strategy(),
        ours_edits in prop::collection::vec(edit_strategy(), 0..6),
        theirs_edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let ours = derive(&base, &path, &ours_edits, UNIQUE_NAMES, 1);
        let theirs = derive(&base, &path, &theirs_edits, UNIQUE_NAMES, 2);

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
        file_name in format_strategy(),
        ours_edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = common::read_fixture(&path).expect("read base");
        let ours = derive(&base, &path, &ours_edits, FEW_NAMES, 1);

        let once = merged_bytes(&ours, &ours, &ours, &path).expect("self-merge is clean");
        let twice = merged_bytes(&once, &once, &once, &path).expect("self-merge is clean");
        prop_assert_eq!(semantic_text(&once, &path), semantic_text(&twice, &path));
    }

    /// Regenerating UniqueIds is never a real conflict. When both sides
    /// regenerate the *same* instance to different values, the instance still
    /// matches by structure and its UniqueId diverges three ways — exactly the
    /// case Lever 1 resolves. A merge whose only changes are regenerations must
    /// therefore always be clean. Restricting the edits to `Regenerate` isolates
    /// that branch: any conflict here could only come from the divergent
    /// UniqueId, so this fails if Lever 1 regresses.
    #[test]
    fn regeneration_alone_never_conflicts(
        file_name in format_strategy(),
        ours_targets in prop::collection::vec(any::<usize>(), 0..6),
        theirs_targets in prop::collection::vec(any::<usize>(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let ours_edits: Vec<Edit> = ours_targets.into_iter().map(Edit::Regenerate).collect();
        let theirs_edits: Vec<Edit> = theirs_targets.into_iter().map(Edit::Regenerate).collect();
        let ours = derive(&base, &path, &ours_edits, UNIQUE_NAMES, 1);
        let theirs = derive(&base, &path, &theirs_edits, UNIQUE_NAMES, 2);

        prop_assert!(
            merged_bytes(&base, &ours, &theirs, &path).is_some(),
            "regeneration-only divergence should never conflict"
        );
    }

    /// Non-conflicting concurrent edits compose. When `ours` and `theirs` write
    /// *different* attribute keys, the merge is always clean and every write
    /// lands — even when both touch the same instance, the key-level attribute
    /// merge keeps both. A bug that adopted one side's attributes wholesale would
    /// drop the other side's keys and be caught here.
    #[test]
    fn disjoint_attribute_edits_compose(
        file_name in format_strategy(),
        ours_targets in prop::collection::vec(any::<usize>(), 0..6),
        theirs_targets in prop::collection::vec(any::<usize>(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let count = nonroot_count(&base, &path);
        prop_assume!(count > 0);

        let ours = set_attribute_on(&base, &path, &ours_targets, "ours");
        let theirs = set_attribute_on(&base, &path, &theirs_targets, "theirs");

        let merged = merged_bytes(&base, &ours, &theirs, &path)
            .expect("disjoint-key attribute edits should never conflict");
        let decoded = common::decode_bytes(&merged, &path).expect("decode merged");

        // Each side's keys land on exactly the instances it targeted, without
        // disturbing the other side's keys.
        let distinct = |targets: &[usize]| {
            targets
                .iter()
                .map(|target| target % count)
                .collect::<HashSet<_>>()
                .len()
        };
        prop_assert_eq!(instances_with_attribute(&decoded, "ours"), distinct(&ours_targets));
        prop_assert_eq!(instances_with_attribute(&decoded, "theirs"), distinct(&theirs_targets));
    }

    /// Identical concurrent changes never conflict. When both sides apply the
    /// *same* edits, every property and structural change agrees, so the merge is
    /// clean and reproduces the normalized side — exercising the `ours == theirs`
    /// branch of the value merge and identical structural matching.
    #[test]
    fn identical_changes_on_both_sides_merge_cleanly(
        file_name in format_strategy(),
        edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let side = derive(&base, &path, &edits, UNIQUE_NAMES, 1);

        prop_assert_eq!(
            merged_text(&base, &side, &side, &path),
            Some(normalized(&side, &path))
        );
    }

    /// A clean merge is deterministic and stable. Re-running the same merge yields
    /// the same result (no ordering leaks from internal hashing into the output),
    /// and the merged output is a fixpoint of the merge's own normalization:
    /// merging it with itself reproduces it. Asserted only on clean merges, since
    /// a conflict is a legitimate outcome.
    #[test]
    fn clean_merge_is_deterministic_and_stable(
        file_name in format_strategy(),
        ours_edits in prop::collection::vec(edit_strategy(), 0..6),
        theirs_edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let ours = derive(&base, &path, &ours_edits, UNIQUE_NAMES, 1);
        let theirs = derive(&base, &path, &theirs_edits, UNIQUE_NAMES, 2);

        if let Some(merged) = merged_bytes(&base, &ours, &theirs, &path) {
            let text = semantic_text(&merged, &path);

            // Determinism: the same inputs produce the same output.
            let again = merged_bytes(&base, &ours, &theirs, &path)
                .expect("a clean merge stays clean when repeated");
            prop_assert_eq!(&text, &semantic_text(&again, &path));

            // Stability: the merged output is a normalized fixpoint.
            prop_assert_eq!(&text, &normalized(&merged, &path));
        }
    }

    /// A clean merge never silently drops an instance both sides kept. Every
    /// instance present in the base whose `UniqueId` still appears on *both*
    /// sides (so neither side deleted it) must appear in a clean merged output.
    /// A clean merge is the engine's promise that it reconciled the two sides
    /// without loss; an instance vanishing from one is data loss masquerading as
    /// success. `Regenerate` is excluded because it rewrites a `UniqueId`, which
    /// would make a surviving instance look deleted to this id-based tracking.
    #[test]
    fn clean_merge_preserves_instances_kept_by_both_sides(
        file_name in format_strategy(),
        ours_edits in non_regenerating_edits(),
        theirs_edits in non_regenerating_edits(),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let ours = derive(&base, &path, &ours_edits, UNIQUE_NAMES, 1);
        let theirs = derive(&base, &path, &theirs_edits, UNIQUE_NAMES, 2);

        if let Some(merged) = merged_bytes(&base, &ours, &theirs, &path) {
            let base_ids = nonroot_unique_ids(&base, &path);
            let ours_ids = nonroot_unique_ids(&ours, &path);
            let theirs_ids = nonroot_unique_ids(&theirs, &path);
            let merged_ids = nonroot_unique_ids(&merged, &path);

            for id in &base_ids {
                if ours_ids.contains(id) && theirs_ids.contains(id) {
                    prop_assert!(
                        merged_ids.contains(id),
                        "instance {id:?} was kept by both sides but is missing from the clean merge"
                    );
                }
            }
        }
    }

    /// A clean merge produces a structurally valid file: no two instances share a
    /// non-nil `UniqueId`. Identity is keyed on `UniqueId`, so a duplicate in the
    /// output is a corrupt result that would mis-match on the next merge. This
    /// pins `detect_unique_id_collisions` and runs the full edit space (including
    /// `Regenerate`, the most likely source of a collision).
    #[test]
    fn clean_merge_output_has_no_duplicate_unique_ids(
        file_name in format_strategy(),
        ours_edits in prop::collection::vec(edit_strategy(), 0..6),
        theirs_edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let ours = derive(&base, &path, &ours_edits, UNIQUE_NAMES, 1);
        let theirs = derive(&base, &path, &theirs_edits, UNIQUE_NAMES, 2);

        if let Some(merged) = merged_bytes(&base, &ours, &theirs, &path) {
            let dom = common::decode_bytes(&merged, &path).expect("decode merged");
            let root = dom.root_ref();
            let mut seen = HashSet::new();
            for instance in dom.descendants() {
                if instance.referent() == root {
                    continue;
                }
                if let Some(Variant::UniqueId(id)) = instance.properties.get(&ustr("UniqueId"))
                    && !id.is_nil()
                {
                    prop_assert!(
                        seen.insert(*id),
                        "duplicate UniqueId {id:?} in clean merged output"
                    );
                }
            }
        }
    }

    /// A bulk take-ours merge is always clean. The default report-everything
    /// merge conflicts on roughly a fifth of random edit pairs; taking ours for
    /// every conflict must always produce output, so no conflict kind — including
    /// `ParentCycle` — is a dead end a caller cannot resolve past. (Already-clean
    /// inputs stay clean, since the resolution only fires on conflicts.) This
    /// exercises the resolution path across the full conflict space, which was
    /// only unit-tested before.
    #[test]
    fn take_ours_merge_is_always_clean(
        file_name in format_strategy(),
        ours_edits in prop::collection::vec(edit_strategy(), 0..6),
        theirs_edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let ours = derive(&base, &path, &ours_edits, UNIQUE_NAMES, 1);
        let theirs = derive(&base, &path, &theirs_edits, UNIQUE_NAMES, 2);

        let resolved = merged_bytes_resolved(
            &base,
            &ours,
            &theirs,
            &path,
            Resolutions::take(Side::Ours),
        );
        prop_assert!(
            resolved.is_some(),
            "take-ours should resolve every conflict into a clean merge"
        );
    }

    /// A clean merge never emits a dangling reference. Every `Ref` property in
    /// the output is nil or points to an instance that exists in the output. A
    /// reference to a deleted target instead becomes a `RefTarget` conflict (so
    /// the merge is not clean) or a nilled-and-reported drop — it never reaches a
    /// clean output as a `Ref` to a vanished instance, which would be a corrupt
    /// file. Exercises the ref-rewriting and ref-target paths across random
    /// reference edits and deletions.
    #[test]
    fn clean_merge_has_no_dangling_references(
        file_name in format_strategy(),
        ours_edits in prop::collection::vec(edit_strategy(), 0..6),
        theirs_edits in prop::collection::vec(edit_strategy(), 0..6),
    ) {
        let path = common::model_path("three-intvalues", file_name);
        let base = seed_base(&common::read_fixture(&path).expect("read base"), &path);
        let ours = derive(&base, &path, &ours_edits, UNIQUE_NAMES, 1);
        let theirs = derive(&base, &path, &theirs_edits, UNIQUE_NAMES, 2);

        if let Some(merged) = merged_bytes(&base, &ours, &theirs, &path) {
            let dom = common::decode_bytes(&merged, &path).expect("decode merged");
            let present: HashSet<Ref> = std::iter::once(dom.root_ref())
                .chain(dom.descendants().map(|instance| instance.referent()))
                .collect();

            for instance in dom.descendants() {
                for value in instance.properties.values() {
                    if let Variant::Ref(referent) = value {
                        prop_assert!(
                            referent.is_none() || present.contains(referent),
                            "instance {:?} has a dangling Ref {referent:?} in a clean merge",
                            instance.name
                        );
                    }
                }
            }
        }
    }
}
