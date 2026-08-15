//! How long the completeness check takes, and whether it stays linear.
//!
//! Like the model bench beside it, nothing here is gated. This runs in
//! CI and in `cargo test`, not on a query path, so a floor on it would
//! buy a flaky check and no user-visible latency. What it is for is
//! the shape of the curve.
//!
//! Writing this found the check quadratic. Resolving an identifier's
//! tier scanned every group looking for the longest prefix that
//! covered it, and the dead-group direction scanned every identifier
//! per group, so the cost was the product of the two. Twenty-five
//! groups over eight hundred entities hid it completely. At the sizes
//! below it did not: 373 ms for thirty-two thousand entities, against
//! 7.9 ms once each identifier walked its own prefixes instead, which
//! is a binary search per path segment and independent of how long the
//! file has become.
//!
//! So the fixture deliberately keeps one group per ten entities, an
//! order of magnitude denser than the real file, because that is the
//! shape the old code was quadratic in and a fixture that cannot
//! reproduce a defect cannot guard against it either.
//!
//! Run: cargo bench -p xtask --bench apimap

use std::hint::black_box;
use std::time::Instant;

use xtask::apimap::{self, Map};

fn main() {
    println!(
        "{:>8}  {:>10}  {:>9}  {:>11}",
        "entities", "parse ms", "check ms", "ns/entity"
    );
    let mut previous: Option<(usize, f64)> = None;
    for entities in [500usize, 2_000, 8_000, 32_000] {
        let (text, ids) = surface(entities);
        let map = Map::parse(&text).expect("the generated map parses");
        assert!(
            apimap::check_surface(&map, &ids).is_empty(),
            "the generated map covers the generated surface"
        );

        let parse_ms = best(|| {
            black_box(Map::parse(black_box(&text)).expect("parses"));
        });
        let check_ms = best(|| {
            black_box(apimap::check_surface(black_box(&map), black_box(&ids)));
        });
        let ns = check_ms * 1e6 / entities as f64;
        println!("{entities:8}  {parse_ms:10.3}  {check_ms:9.3}  {ns:11.0}");

        // A factor of two either way is noise on a shared runner. An
        // order of magnitude is a lookup that went linear.
        if let Some((before, previous_ns)) = previous {
            assert!(
                ns < previous_ns * 4.0,
                "per-entity check cost went from {previous_ns:.0} ns at {before} entities \
                 to {ns:.0} ns at {entities}, which is not linear"
            );
        }
        previous = Some((entities, ns));
    }

    // The numbers above are synthetic. This is the one a reader cares
    // about: what the check costs on the surface that actually exists.
    let text = include_str!("../../../docs/api/api-map.toml");
    let model =
        zu_json::parse(include_str!("../../../docs/api/model.json")).expect("the model parses");
    let ids = apimap::mappable_ids(&model).expect("the model has identifiers");
    let map = Map::parse(text).expect("the committed map parses");
    let check_ms = best(|| {
        black_box(apimap::check_surface(black_box(&map), black_box(&ids)));
    });
    println!(
        "\ncommitted surface: {} entities, {} groups, {} exceptions, checked in {:.3} ms",
        ids.len(),
        map.groups.len(),
        map.entries.len(),
        check_ms
    );
}

/// A map and the surface it covers, both of the given size, plus one
/// exception per twenty entities so the search over them has something
/// to do.
fn surface(entities: usize) -> (String, Vec<String>) {
    let groups = entities / 10;
    let mut text = String::from("schema = 1\ntarget = \"rust\"\n");
    for g in 0..groups {
        text.push_str(&format!(
            "\n[[group]]\nprefix = \"zu::m{g:06}\"\ntier = 3\nreason = \"generated\"\n"
        ));
    }
    let mut ids = Vec::with_capacity(entities);
    for i in 0..entities {
        ids.push(format!("zu::m{:06}::Item{i:06}", i % groups.max(1)));
    }
    ids.sort();
    for id in ids.iter().step_by(20) {
        text.push_str(&format!("\n[[entity]]\nid = \"{id}\"\ntier = 2\n"));
    }
    (text, ids)
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
