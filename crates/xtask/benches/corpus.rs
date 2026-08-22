//! What packing the corpus costs, and what the artifact weighs.
//!
//! Two numbers matter and neither is a latency. Packing happens once
//! per release, so its cost is only interesting if it is superlinear:
//! a packer that is quadratic in the number of cases is a release step
//! that stops finishing somewhere between the corpus of today and the
//! corpus of next year, and the cheapest moment to find that out is
//! now. The assertion is therefore on the per-case cost and not on any
//! absolute time.
//!
//! The size is the number with a real consumer. The artifact is
//! downloaded on every CI run of nine repositories, so the ratio is
//! what a corpus twice the size costs everybody else. The committed
//! corpus compresses about five to one at level 19, which puts the
//! whole artifact under twenty kilobytes and makes the level a
//! decision nobody needs to revisit until the corpus is fifty times
//! the size it is now.
//!
//! The generated corpus below compresses far better than that, and the
//! number is worth reading and not believing: a generator writes the
//! same sentence a thousand times and zstd finds it, where real cases
//! are a thousand different sentences about a thousand different
//! things. The fixture is here for the shape of the packing curve. The
//! committed corpus at the bottom is the one whose ratio means
//! anything.
//!
//! Run: cargo bench -p xtask --bench corpus

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use xtask::corpus;
use xtask::scratch::Scratch;

fn main() {
    println!(
        "{:>7}  {:>9}  {:>10}  {:>9}  {:>7}",
        "cases", "pack ms", "us/case", "packed KiB", "ratio"
    );
    let mut previous: Option<(usize, f64)> = None;
    for cases in [100usize, 400, 1_600, 6_400] {
        let dir = fixture(cases);
        let packed = corpus::pack(&dir, None, "0.0.0").expect("the generated corpus packs");
        assert_eq!(packed.cases(), cases);

        let ms = best(|| {
            black_box(corpus::pack(black_box(&dir), None, "0.0.0").expect("packs"));
        });
        let us = ms * 1e3 / cases as f64;
        let ratio = packed.tar.len() as f64 / packed.archive.len() as f64;
        println!(
            "{cases:7}  {ms:9.3}  {us:10.1}  {:9}  {ratio:6.1}x",
            packed.archive.len().div_ceil(1024)
        );

        // A factor of two either way is noise on a shared runner. Four
        // is a cost that is not linear in the cases.
        if let Some((before, previous_us)) = previous {
            assert!(
                us < previous_us * 4.0,
                "per-case packing went from {previous_us:.1} us at {before} cases to {us:.1} us \
                 at {cases}, which is not linear"
            );
        }
        previous = Some((cases, us));
    }

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = root.join("conformance/cases");
    let readme = root.join("conformance/README.md");
    let packed = corpus::pack(&dir, readme.exists().then_some(&readme), "0.0.0")
        .expect("the committed corpus packs");
    let ms = best(|| {
        black_box(corpus::pack(black_box(&dir), None, "0.0.0").expect("packs"));
    });
    println!(
        "\ncommitted corpus: {} suites, {} cases, {} KiB packed from {} KiB ({:.1}x) in {ms:.1} ms",
        packed.entries.len(),
        packed.cases(),
        packed.archive.len().div_ceil(1024),
        packed.tar.len().div_ceil(1024),
        packed.tar.len() as f64 / packed.archive.len() as f64,
    );
}

/// A corpus of `cases` cases, spread over ten suites so the packer has
/// more than one file to walk. The text repeats far more than real
/// cases do, which is what inflates the ratio and is why only the
/// timing column of this fixture is worth reading.
fn fixture(cases: usize) -> Scratch {
    let dir = Scratch::new(&format!("pack-bench-{cases}"));
    let suites = 10;
    for suite in 0..suites {
        let mut text = format!(
            "schema: {}\nsuite: suite-{suite}\ndoc: A suite the packing bench generated, which \
             exists so the packer has a file to walk and a size to compress.\n\ncases:\n",
            zu_corpus::case::SCHEMA
        );
        for n in (suite..cases).step_by(suites) {
            text.push_str(&format!(
                "  - name: case-{n}\n    doc: A case the packing bench generated, which \
                 says the same thing every other generated case says.\n    query: RETURN {n} AS \
                 n\n    columns:\n      - n\n    rows:\n      - values:\n          - type: \
                 INT64\n            value: \"{n}\"\n"
            ));
        }
        std::fs::write(dir.join(format!("suite-{suite}.yaml")), text).expect("writes");
    }
    dir
}

/// The best of seven, in milliseconds. The best rather than the mean
/// because the thing being measured is the work, and every sample
/// above the floor is the machine doing something else.
fn best(mut body: impl FnMut()) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        body();
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
    }
    best
}
