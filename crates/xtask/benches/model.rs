//! How long it takes to turn rustdoc's output into `model.json`.
//!
//! This runs in a codegen job and in CI's `--check`, not on a query
//! path, so nothing here is gated: a floor on a build tool buys a flaky
//! check and no user-visible latency. What it is for is the shape of
//! the curve. The walk resolves every re-export through hash lookups
//! and collects into a `BTreeMap` keyed by identifier, so cost should
//! track the number of entities and not the square of it, and the one
//! way to find out that a lookup went linear is to measure two sizes
//! and compare.
//!
//! Run: cargo bench -p xtask

use std::hint::black_box;
use std::time::Instant;

use xtask::{fixture, model};

fn main() {
    println!(
        "{:>8}  {:>10}  {:>9}  {:>11}",
        "entities", "build ms", "write ms", "ns/entity"
    );
    let mut previous: Option<(f64, f64)> = None;
    for (modules, types, methods) in [(4, 4, 4), (8, 8, 6), (16, 16, 8), (24, 24, 10)] {
        let docs = [fixture::crate_doc("zu", modules, types, methods)];

        let built = model::build(&docs, "zu").expect("the fixture builds");
        let count = built.entities.len() as f64;

        let build_ms = best(|| {
            black_box(model::build(black_box(&docs), "zu").expect("the fixture builds"));
        });
        let json = built.to_json();
        let write_ms = best(|| {
            black_box(black_box(&json).to_pretty());
        });
        println!(
            "{count:8.0}  {build_ms:10.3}  {write_ms:9.3}  {:11.0}",
            build_ms * 1e6 / count
        );

        // Linear means the per-entity cost holds as the input grows.
        // A factor of two either way is measurement noise on a shared
        // runner; an order of magnitude is a lookup that went linear.
        if let Some((prev_count, prev_ns)) = previous {
            let ns = build_ms * 1e6 / count;
            assert!(
                ns < prev_ns * 4.0,
                "per-entity build cost went from {prev_ns:.0} ns at {prev_count:.0} entities \
                 to {ns:.0} ns at {count:.0}, which is not linear"
            );
        }
        previous = Some((count, build_ms * 1e6 / count));
    }

    // The bytes have to be identical run to run or CI's --check is a
    // coin toss, and this is the one place that runs it enough times.
    let docs = [fixture::crate_doc("zu", 8, 8, 6)];
    let once = model::build(&docs, "zu")
        .expect("builds")
        .to_json()
        .to_pretty();
    for _ in 0..32 {
        let again = model::build(&docs, "zu")
            .expect("builds")
            .to_json()
            .to_pretty();
        assert!(again == once, "the generator is not deterministic");
    }
    println!("\n32 rebuilds, identical bytes");
}

/// Milliseconds for one call, best of several, so a scheduler hiccup
/// does not become the reported number.
fn best(mut body: impl FnMut()) -> f64 {
    body();
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        body();
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
    }
    best
}
