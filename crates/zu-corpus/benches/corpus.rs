//! What the corpus costs to read and what it costs to run.
//!
//! Nothing here is on a query path, so there is no latency budget to
//! defend. Two things are worth measuring anyway.
//!
//! The first is the shape of the reader's curve. The corpus is a couple
//! of hundred cases today and will be a few thousand, in nine
//! repositories, read on every CI run of every one of them. A reader
//! that is quadratic in the number of cases is not a problem at thirty
//! and is one at three thousand, and the cheapest moment to find that
//! out is before the cases exist. So the fixture goes past the size the
//! real corpus is heading for, and the assertion is on the per-case cost
//! rather than on any absolute number.
//!
//! The second is the split between reading and running. Every case gets
//! a database of its own, which is the rule that keeps a leaked table
//! out of the next case and is also the most expensive thing the runner
//! does. Knowing what that costs per case is what says whether the rule
//! survives the corpus growing tenfold, and the number below says it
//! does: about 4 ms a case against about 3 us to read one, three orders
//! of magnitude apart. So the reader is not worth optimizing and the
//! database-per-case rule is the only thing that would ever need
//! revisiting, at around four seconds for a thousand cases.
//!
//! Run: cargo bench -p zu-corpus --bench corpus

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use zu_corpus::{Suite, load, run};

fn main() {
    println!("{:>7}  {:>10}  {:>10}", "cases", "parse ms", "ns/case");
    let mut previous: Option<(usize, f64)> = None;
    for cases in [100usize, 400, 1_600, 6_400] {
        let text = suite(cases);
        let parsed = Suite::parse(&text).expect("the generated suite parses");
        assert_eq!(parsed.cases.len(), cases);

        let parse_ms = best(|| {
            black_box(Suite::parse(black_box(&text)).expect("parses"));
        });
        let ns = parse_ms * 1e6 / cases as f64;
        println!("{cases:7}  {parse_ms:10.3}  {ns:10.0}");

        // A factor of two either way is noise on a shared runner. An
        // order of magnitude is a scan that went quadratic, which is
        // what a duplicate-name check written the obvious way does.
        if let Some((before, previous_ns)) = previous {
            assert!(
                ns < previous_ns * 4.0,
                "per-case parse cost went from {previous_ns:.0} ns at {before} cases \
                 to {ns:.0} ns at {cases}, which is not linear"
            );
        }
        previous = Some((cases, ns));
    }

    // The numbers above are synthetic. These are the ones a reader
    // cares about: the corpus that exists, read and then run.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance/cases");
    let suites = load(&dir).expect("the committed corpus loads");
    let count: usize = suites.iter().map(|s| s.cases.len()).sum();
    let load_ms = best(|| {
        black_box(load(black_box(&dir)).expect("loads"));
    });

    let temp = std::env::temp_dir().join(format!("zu-corpus-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).expect("a scratch directory");
    // One run, not a best-of: the runner writes a file per case, so
    // repeating it measures the page cache rather than the engine.
    let start = Instant::now();
    let report = run(&suites, &temp);
    let run_ms = start.elapsed().as_secs_f64() * 1e3;
    let _ = std::fs::remove_dir_all(&temp);
    assert_eq!(report.failures().count(), 0, "{}", report.summary());

    println!(
        "\ncommitted corpus: {} suites, {count} cases, read in {load_ms:.3} ms \
         ({:.0} ns/case), run in {run_ms:.1} ms ({:.2} ms/case)",
        suites.len(),
        load_ms * 1e6 / count as f64,
        run_ms / count as f64
    );
}

/// A suite of the given size, with the fields a real case carries so
/// the reader does the same work per case that it does on the corpus.
fn suite(cases: usize) -> String {
    // The schema is read off the reader rather than written down here.
    // A bump moves one constant and every suite in the tree with it,
    // and a bench holding a literal is the one file that gets left on
    // the old number and only says so on CI.
    let mut text = format!("schema: {}\nsuite: generated\n", zu_corpus::case::SCHEMA);
    text.push_str(
        "doc: A suite the bench generates, which is not a suite anybody runs.\n\ncases:\n",
    );
    for i in 0..cases {
        text.push_str(&format!(
            "  - name: case-{i:06}\n    \
             doc: A generated case, here to give the reader the same work per case that a real one does.\n    \
             query: RETURN {i} AS n\n    \
             columns:\n      - n\n    \
             rows:\n      - values:\n          - type: INT64\n            value: \"{i}\"\n"
        ));
    }
    text
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
