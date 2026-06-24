//! Scaling benchmarks for the `textconv` tree renderer.
//!
//! `textconv` decodes a file, builds the semantic model, and renders the
//! deterministic indentation tree used for diffs. The renderer is the dominant
//! cost — on a property-heavy tree it is ~70% of the call, the rest being decode
//! and semantic-model construction — and it is allocation-bound: every value
//! becomes a freshly formatted `String`, every line is built with a `format!`,
//! and every node allocates its indentation with `str::repeat`. Decode and model
//! construction do not change when the renderer does, so the `textconv` time
//! tracks renderer changes directly.
//!
//! The renderer's cost scales on two axes, one scenario each:
//!
//! - `textconv/instances` renders a tree of `N` bare folders, exercising the
//!   per-node structural path (indentation, the `ClassName "Name"` header line,
//!   and the blank-line separators) without any property formatting.
//! - `textconv/properties` renders `N` instances each carrying `P` diverse typed
//!   properties (`Vector3`, `CFrame`, `Color3`, floats, bools), exercising
//!   `display_variant` — the per-value formatting that allocates the most.
//! - `textconv/attributes` renders `N` instances each carrying an `Attributes`
//!   map of `A` entries, exercising the nested attribute rendering path.
//!
//! Sample sizes are kept small to keep a run practical at the larger sizes.

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rbx_dom_weak::{InstanceBuilder, WeakDom};
use rbx_merge::{TextconvOptions, textconv};
use rbx_types::{CFrame, Color3, Matrix3, Variant, Vector3};

/// Build a binary-encoded tree of `n` instances, each parent given `fanout`
/// children breadth-first, so the tree is both wide and deep. `make(i)` produces
/// the `i`th instance.
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

/// The first `count` of a fixed list of real, typed `Part` properties, chosen to
/// stress the renderer: `CFrame` and the `Vector3`s flatten into many formatted
/// floats, exercising the deepest formatting path. Integer-valued floats round
/// trip exactly through the binary codec. `count` saturates at the list length.
fn part_props(count: usize, seed: u8) -> Vec<(&'static str, Variant)> {
    let f = f32::from(seed);
    let c = f / 255.0;
    let all = vec![
        ("Size", Variant::Vector3(Vector3::new(f, f, f))),
        ("Position", Variant::Vector3(Vector3::new(f, f, f))),
        (
            "CFrame",
            Variant::CFrame(CFrame::new(Vector3::new(f, f, f), Matrix3::identity())),
        ),
        ("Color", Variant::Color3(Color3::new(c, c, c))),
        ("Transparency", Variant::Float32(c)),
        ("Reflectance", Variant::Float32(1.0 - c)),
        ("Anchored", Variant::Bool(seed.is_multiple_of(2))),
        ("CanCollide", Variant::Bool(seed.is_multiple_of(3))),
    ];
    all.into_iter().take(count).collect()
}

fn encode(dom: &WeakDom) -> Vec<u8> {
    let mut bytes = Vec::new();
    rbx_binary::to_writer(&mut bytes, dom, dom.root().children()).expect("dom should encode");
    bytes
}

fn render(bytes: &[u8]) {
    // Benchmark the default, diff-oriented path (filtering on) — what the Git
    // textconv driver actually runs.
    textconv(bytes, None, TextconvOptions::default()).expect("textconv should succeed");
}

fn bench_instances(c: &mut Criterion) {
    let mut group = c.benchmark_group("textconv/instances");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    for &n in &[1_000usize, 5_000, 20_000] {
        let bytes = build_with(n, 8, |i| {
            InstanceBuilder::new("Folder").with_name(format!("F{i}"))
        });
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &bytes, |b, bytes| {
            b.iter(|| render(bytes));
        });
    }
    group.finish();
}

fn bench_properties(c: &mut Criterion) {
    let mut group = c.benchmark_group("textconv/properties");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    let n = 5_000usize;
    for &props in &[1usize, 2, 4, 8] {
        let bytes = build_with(n, 8, |i| {
            InstanceBuilder::new("Part")
                .with_name(format!("P{i}"))
                .with_properties(part_props(props, i as u8))
        });
        // Count the property values rendered, so the metric reads as
        // time-per-value across widths.
        group.throughput(Throughput::Elements((n * props) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(props), &bytes, |b, bytes| {
            b.iter(|| render(bytes));
        });
    }
    group.finish();
}

fn bench_attributes(c: &mut Criterion) {
    let mut group = c.benchmark_group("textconv/attributes");
    group
        .sample_size(10)
        .measurement_time(Duration::from_secs(3));
    let n = 5_000usize;
    for &attrs in &[1usize, 4, 8, 16] {
        let bytes = build_with(n, 8, |i| {
            let mut map = rbx_types::Attributes::new();
            for k in 0..attrs {
                map.insert(format!("Attr{k}"), Variant::String(format!("v{i}_{k}")));
            }
            InstanceBuilder::new("Folder")
                .with_name(format!("F{i}"))
                .with_property("Attributes", Variant::Attributes(map))
        });
        group.throughput(Throughput::Elements((n * attrs) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(attrs), &bytes, |b, bytes| {
            b.iter(|| render(bytes));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_instances, bench_properties, bench_attributes);
criterion_main!(benches);
