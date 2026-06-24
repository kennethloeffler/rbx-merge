//! Scaling benchmarks for the three-way merge.
//!
//! These drive the merge with synthetic trees sized by instance count, the axis
//! the merge's cost actually scales on. A fixed on-disk fixture cannot vary that
//! axis — the largest in rbx-test-files is only ~240 instances, far too small to
//! expose the node-count behavior of the identity match, child-order resolution,
//! and graph lowering. The node-count scenarios:
//!
//! - `merge/identical` merges three identical copies of an `N`-node tree,
//!   exercising the structural and child-order passes that touch every node.
//! - `merge/additions` merges a common base where each side independently adds
//!   `A` brand-new nodes, exercising the added-instance identity matching that
//!   pairs one side's additions against the other's.
//!
//! The remaining scenarios hold the node count fixed and scale the per-instance
//! property work instead — the other axis the merge's cost rides on, since every
//! pass that touches a node also reduces its properties to comparison keys:
//!
//! - `merge/properties` merges three identical copies of an `N`-instance tree
//!   whose instances each carry `P` real, typed properties, isolating the
//!   per-property comparison (`merge_properties` over the union of keys, and the
//!   `variant_key` lowering each value passes through). Throughput counts the
//!   `N * P` property values reduced per merge, so results read as time-per-value.
//! - `merge/property_edits` merges a common base where each side modifies a
//!   different property on the same `E` instances, driving the three-way value
//!   merge down its *divergent* path — the resolution the all-equal `identical`
//!   case never reaches — plus the property diff the identity match runs.
//! - `merge/attributes` merges three identical copies whose instances each carry
//!   an `Attributes` map of `A` entries, exercising the per-attribute three-way
//!   merge and the recursive `variant_key` over attribute values.
//!
//! Sample sizes are kept small: the cost grows quickly enough at the larger
//! sizes that the criterion default would make a run impractical.

use std::io::Cursor;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rbx_dom_weak::{InstanceBuilder, WeakDom, ustr};
use rbx_merge::{FileInput, MergeSettings, merge_files};
use rbx_types::{Attributes, CFrame, Color3, Matrix3, Ref, Variant, Vector3};

/// Build a binary-encoded tree of `n` instances, each parent given `fanout`
/// children breadth-first, so the tree is both wide and deep. `make(i)` produces
/// the `i`th instance, so callers can vary class and per-instance properties
/// while reusing the same breadth-first shape.
fn build_with(n: usize, fanout: usize, make: impl Fn(usize) -> InstanceBuilder) -> Vec<u8> {
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel").with_name("game"));
    let mut frontier = vec![dom.root_ref()];
    let mut next = Vec::new();
    let mut made = 0;
    while made < n {
        next.clear();
        for &parent in &frontier {
            for _ in 0..fanout {
                if made >= n {
                    break;
                }
                let child = dom.insert(parent, make(made));
                next.push(child);
                made += 1;
            }
        }
        std::mem::swap(&mut frontier, &mut next);
        if frontier.is_empty() {
            break;
        }
    }
    encode(&dom)
}

/// Build a tree of bare `Folder`s carrying only a name — the structural shape
/// the node-count scenarios scale.
fn build(n: usize, fanout: usize) -> Vec<u8> {
    build_with(n, fanout, |i| {
        InstanceBuilder::new("Folder").with_name(format!("F{i}"))
    })
}

/// The first `count` of a fixed list of real, typed `Part` properties, with
/// values derived from `seed`. Using genuine class properties keeps the values
/// round-tripping cleanly through the binary codec; integer-valued floats round
/// trip exactly. `count` saturates at the list length (8).
fn part_props(count: usize, seed: u8) -> Vec<(&'static str, Variant)> {
    let f = f32::from(seed);
    let c = f / 255.0;
    let all = vec![
        ("Size", Variant::Vector3(Vector3::new(f, f, f))),
        (
            "CFrame",
            Variant::CFrame(CFrame::new(Vector3::new(f, f, f), Matrix3::identity())),
        ),
        ("Color", Variant::Color3(Color3::new(c, c, c))),
        ("Transparency", Variant::Float32(c)),
        ("Reflectance", Variant::Float32(1.0 - c)),
        ("Anchored", Variant::Bool(seed.is_multiple_of(2))),
        ("CanCollide", Variant::Bool(seed.is_multiple_of(3))),
        ("CastShadow", Variant::Bool(seed.is_multiple_of(5))),
    ];
    all.into_iter().take(count).collect()
}

/// Build a tree of `Part`s each carrying `props` real properties, so the merge's
/// property passes have `n * props` values to reduce and compare.
fn build_parts(n: usize, fanout: usize, props: usize) -> Vec<u8> {
    build_with(n, fanout, |i| {
        InstanceBuilder::new("Part")
            .with_name(format!("P{i}"))
            .with_properties(part_props(props, i as u8))
    })
}

/// Build a tree of `Folder`s each carrying an `Attributes` map of `attrs`
/// string-valued entries, so the merge runs its per-attribute three-way pass.
fn build_attributed(n: usize, fanout: usize, attrs: usize) -> Vec<u8> {
    build_with(n, fanout, |i| {
        let mut map = Attributes::new();
        for k in 0..attrs {
            map.insert(format!("Attr{k}"), Variant::String(format!("v{i}_{k}")));
        }
        InstanceBuilder::new("Folder")
            .with_name(format!("F{i}"))
            .with_property("Attributes", Variant::Attributes(map))
    })
}

/// Decode `bytes`, overwrite property `name` on the first `count` `Part`s with a
/// value derived from `seed`, and re-encode. Pointing each side at a different
/// property of the same instances yields a clean divergent merge: every edited
/// instance forces the three-way value merge to resolve two modified keys.
fn with_property_edits(bytes: &[u8], count: usize, name: &str, seed: f32) -> Vec<u8> {
    let mut dom = rbx_binary::from_reader(Cursor::new(bytes)).expect("base should decode");
    let targets: Vec<Ref> = dom
        .descendants()
        .filter(|instance| instance.class.as_str() == "Part")
        .take(count)
        .map(|instance| instance.referent())
        .collect();
    for (i, referent) in targets.into_iter().enumerate() {
        if let Some(instance) = dom.get_by_ref_mut(referent) {
            instance
                .properties
                .insert(ustr(name), Variant::Float32(seed + i as f32));
        }
    }
    encode(&dom)
}

/// Decode `bytes`, append `adds` brand-new top-level folders (none matching the
/// base), and re-encode. The added nodes are unmatched against the base, so the
/// identity pass runs its added-instance matching for each.
fn with_additions(bytes: &[u8], adds: usize, tag: &str) -> Vec<u8> {
    let mut dom = rbx_binary::from_reader(Cursor::new(bytes)).expect("base should decode");
    let root = dom.root_ref();
    for i in 0..adds {
        dom.insert(
            root,
            InstanceBuilder::new("Folder").with_name(format!("Added_{tag}_{i}")),
        );
    }
    encode(&dom)
}

fn encode(dom: &WeakDom) -> Vec<u8> {
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, dom, dom.root().children()).expect("dom should encode");
    bytes
}

fn merge_three(base: &[u8], ours: &[u8], theirs: &[u8]) {
    merge_files(
        FileInput::new(base),
        FileInput::new(ours),
        FileInput::new(theirs),
        MergeSettings::default(),
    )
    .expect("merge should succeed");
}

fn bench_identical(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge/identical");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    for &n in &[1_000usize, 5_000, 20_000] {
        let bytes = build(n, 8);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &bytes, |b, bytes| {
            b.iter(|| merge_three(bytes, bytes, bytes));
        });
    }
    group.finish();
}

fn bench_additions(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge/additions");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    let base = build(5_000, 8);
    for &adds in &[500usize, 1_000, 2_000, 4_000] {
        let ours = with_additions(&base, adds, "ours");
        let theirs = with_additions(&base, adds, "theirs");
        group.throughput(Throughput::Elements(adds as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(adds),
            &(ours, theirs),
            |b, (ours, theirs)| {
                b.iter(|| merge_three(&base, ours, theirs));
            },
        );
    }
    group.finish();
}

fn bench_properties(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge/properties");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    let n = 5_000usize;
    for &props in &[1usize, 2, 4, 8] {
        let bytes = build_parts(n, 8, props);
        // Count the property values reduced per merge (every instance on every
        // side), so the metric reads as time-per-value across widths.
        group.throughput(Throughput::Elements((n * props) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(props), &bytes, |b, bytes| {
            b.iter(|| merge_three(bytes, bytes, bytes));
        });
    }
    group.finish();
}

fn bench_property_edits(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge/property_edits");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    // Six properties so both edited keys (`Transparency`, `Reflectance`) start
    // present on the base and the edits are modifications, not additions.
    let base = build_parts(5_000, 8, 6);
    for &edits in &[500usize, 1_000, 2_000, 4_000] {
        let ours = with_property_edits(&base, edits, "Transparency", 1_000.0);
        let theirs = with_property_edits(&base, edits, "Reflectance", 2_000.0);
        group.throughput(Throughput::Elements(edits as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(edits),
            &(ours, theirs),
            |b, (ours, theirs)| {
                b.iter(|| merge_three(&base, ours, theirs));
            },
        );
    }
    group.finish();
}

fn bench_attributes(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge/attributes");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    let n = 5_000usize;
    for &attrs in &[1usize, 4, 8, 16] {
        let bytes = build_attributed(n, 8, attrs);
        group.throughput(Throughput::Elements((n * attrs) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(attrs), &bytes, |b, bytes| {
            b.iter(|| merge_three(bytes, bytes, bytes));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_identical,
    bench_additions,
    bench_properties,
    bench_property_edits,
    bench_attributes,
);
criterion_main!(benches);
