//! Proxy-overhead benchmarks (spec §12 SLOs).
//!
//! These are microbenchmarks of the two *dominant* CPU costs Limen adds per
//! request. They are components of the SLO's "added latency" — the full path
//! also resolves a request id, filters headers, builds the upstream request, and
//! records metrics — not an end-to-end measurement. What they establish is that
//! this work is microsecond-scale, comfortably inside the millisecond SLO budget
//! (a faithful end-to-end figure would also include the upstream round-trip,
//! which is not Limen's overhead). Two groups track the two SLO scenarios:
//!
//! - `streaming_route_match` — on the streaming path (comparison disabled or not
//!   sampled), the dominant added cost is matching a route by longest path
//!   prefix. SLO budget: p50 < 1 ms, p99 < 5 ms added latency.
//! - `buffer_compare` — the buffer-for-compare path additionally normalizes,
//!   hashes, and diffs the legacy/new bodies. SLO budget (bodies < ~64 KB):
//!   p50 < 3 ms, p99 < 15 ms added latency.
//!
//! Run with `cargo bench`; criterion prints time estimates per benchmark.

use std::hint::black_box;
use std::path::Path;

use axum::http::HeaderMap;
use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use limen::compare::diff::DiffLimits;
use limen::compare::{compare, Captured};
use limen::config::model::Config;
use limen::contract::ComparisonRules;
use limen::routing::{resolve_comparisons, RouteTable};

/// A small, realistic routing table compiled once.
fn route_table() -> RouteTable {
    let config: Config = serde_yaml::from_str(
        r#"
routes:
  - id: get-device
    match: { methods: ["GET"], path_prefix: "/devices/" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: legacy_only
  - id: list-devices
    match: { methods: ["GET"], path_prefix: "/devices" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: legacy_only
  - id: users
    match: { methods: ["GET", "POST"], path_prefix: "/users" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: legacy_only
  - id: orders
    match: { methods: ["GET"], path_prefix: "/orders/" }
    legacy_upstream: "https://legacy.internal"
    new_upstream: "https://new.internal"
    mode: legacy_only
  - id: health
    match: { methods: ["GET"], path_prefix: "/health" }
    new_upstream: "https://new.internal"
    mode: new_only
"#,
    )
    .expect("valid bench config");
    let comparisons = resolve_comparisons(&config, Path::new(".")).expect("resolve comparisons");
    RouteTable::build(&config, comparisons).expect("build route table")
}

/// A response body: a JSON array of `n` device objects. When `vary` is set, one
/// item's value differs (to exercise the diff path on a mismatch).
fn json_body(n: usize, vary: bool) -> Bytes {
    let items: Vec<_> = (0..n)
        .map(|i| {
            let name = if vary && i == 0 {
                format!("Device {i} (changed)")
            } else {
                format!("Device {i}")
            };
            serde_json::json!({
                "id": format!("dev-{i:06}"),
                "name": name,
                "kind": "sensor",
                "enabled": true,
                "tags": ["alpha", "beta", "gamma"],
                "metrics": { "temp_c": 21.5, "humidity": 0.42, "uptime_s": 99_999 },
                "created_at": "2026-01-02T03:04:05Z",
            })
        })
        .collect();
    Bytes::from(serde_json::to_vec(&serde_json::json!({ "items": items })).expect("serialize"))
}

fn captured(body: Bytes) -> Captured {
    Captured {
        status: 200,
        headers: HeaderMap::new(),
        body,
    }
}

/// Streaming-path overhead: longest-prefix route matching.
fn streaming_route_match(c: &mut Criterion) {
    let table = route_table();
    let mut group = c.benchmark_group("streaming_route_match");
    group.bench_function("hit_longest_prefix", |b| {
        b.iter(|| table.match_route(black_box("GET"), black_box("/devices/abc-123")))
    });
    group.bench_function("no_match", |b| {
        b.iter(|| table.match_route(black_box("GET"), black_box("/widgets/xyz")))
    });
    group.finish();
}

/// Buffer-for-compare overhead: normalize + hash + diff over representative
/// JSON bodies, both when they match and when they differ.
fn buffer_compare(c: &mut Criterion) {
    let rules = ComparisonRules::default();
    let limits = DiffLimits::default();
    let mut group = c.benchmark_group("buffer_compare");
    for &n in &[10usize, 200] {
        let legacy = captured(json_body(n, false));
        let same = captured(json_body(n, false));
        let differing = captured(json_body(n, true));
        let size = legacy.body.len() as u64;
        group.throughput(Throughput::Bytes(size));
        group.bench_with_input(
            BenchmarkId::new("match", size),
            &(&legacy, &same),
            |b, (l, r)| {
                b.iter(|| {
                    compare(
                        black_box(&rules),
                        black_box(&limits),
                        black_box(l),
                        black_box(r),
                    )
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("mismatch", size),
            &(&legacy, &differing),
            |b, (l, r)| {
                b.iter(|| {
                    compare(
                        black_box(&rules),
                        black_box(&limits),
                        black_box(l),
                        black_box(r),
                    )
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, streaming_route_match, buffer_compare);
criterion_main!(benches);
