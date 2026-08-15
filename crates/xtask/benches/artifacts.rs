//! What holding a release to the artifact contract costs.
//!
//! Two numbers, and neither of them is the release itself: assembling
//! is compression and copying, and it costs what those cost. What is
//! measured here is the bookkeeping around them, because that is the
//! part that runs on every pull request as well as on release day, and
//! because it is the part with a shape that can go wrong.
//!
//! The second column is the one that matters. Verifying reads the
//! release directory once and then looks each expected name up, so the
//! cost per artifact is flat. The failure worth catching is the
//! accidental square, a scan of the directory per row, which costs
//! nothing at ten artifacts and is the reason a check gets skipped at
//! several hundred, and several hundred is where this goes when tier 2
//! platforms and per-binding artifacts arrive.
//!
//! Run: cargo bench -p xtask --bench artifacts

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use xtask::artifacts::{PATH, Table, tier1};

fn main() {
    println!("{:>9}  {:>9}  {:>9}", "rows", "parse ms", "us/row");
    let mut per_row = None;
    for rows in [8usize, 32, 128, 512] {
        let text = table(rows);
        let ms = best(|| {
            black_box(Table::parse(black_box(&text)).expect("the generated table parses"));
        });
        let us = ms * 1e3 / rows as f64;
        println!("{rows:9}  {ms:9.3}  {us:9.2}");

        // Parsing is a pass over the lines and a lookup per row. Four
        // times the cost per row is a table whose own validation went
        // quadratic, which is what a name checked against every name
        // before it looks like.
        if let Some((before, was)) = per_row {
            assert!(
                us < was * 4.0,
                "parsing went from {was:.2} us/row at {before} rows to {us:.2} us/row at {rows}, \
                 which is not linear"
            );
        }
        per_row = Some((rows, us));
    }

    println!("\n{:>9}  {:>9}  {:>9}", "files", "verify ms", "us/file");
    let mut per_file = None;
    for rows in [8usize, 32, 128, 512] {
        let text = table(rows);
        let table = Table::parse(&text).expect("the generated table parses");
        let dir = release(&table, rows);
        let ms = best(|| {
            let (shipped, faults) = table
                .verify(black_box(&dir), "0.5.0", &[])
                .expect("the release is readable");
            assert!(faults.is_empty(), "{faults:?}");
            black_box(shipped);
        });
        let us = ms * 1e3 / rows as f64;
        println!("{rows:9}  {ms:9.3}  {us:9.2}");

        // One read of the directory and one lookup per name. Four times
        // the cost per file is a lookup that walks the directory again.
        if let Some((before, was)) = per_file {
            assert!(
                us < was * 4.0,
                "verifying went from {was:.2} us/file at {before} files to {us:.2} us/file at \
                 {rows}, which is not linear"
            );
        }
        per_file = Some((rows, us));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Cargo runs a bench from the package directory, so the tree is two
    // levels up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let committed = Table::load(&root.join(PATH)).expect("the committed table loads");
    let targets = tier1(&root).expect("the platform table loads");
    let ms = best(|| {
        black_box(
            committed
                .check(black_box(&root))
                .expect("the tree is readable"),
        );
    });
    println!(
        "\ncommitted contract: {} rows, {} names for one release, {ms:.2} ms",
        committed.artifacts.len(),
        committed.names("0.5.0", &targets).len(),
    );
}

/// A contract of `rows` artifacts, all of them published, which is the
/// shape that costs the most: a row nothing makes yet is a row neither
/// the assemble nor the verify step looks at.
fn table(rows: usize) -> String {
    let mut text =
        String::from("schema = 1\ndoc = \"What the bench publishes.\"\naudited = \"2026-08-16\"\n");
    for n in 0..rows {
        text.push_str(&format!(
            "\n[[artifact]]\nname = \"thing{n}-<version>.json\"\nmade = \"file\"\nfrom = \
             \"docs/thing{n}.json\"\nconsumers = [\"zu-python\", \"zu-web\"]\ndoc = \"An artifact \
             the bench generated, which a release publishes like any other.\"\n"
        ));
    }
    text
}

/// A release directory holding exactly what that table publishes, which
/// is the case with no faults in it and therefore the one that does the
/// most work before answering.
fn release(table: &Table, rows: usize) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("zu-artifacts-bench-{}-{rows}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch dir is writable");
    for name in table.names("0.5.0", &[]) {
        std::fs::write(dir.join(name), b"{}\n").expect("writes");
    }
    dir
}

/// The best of seven, in milliseconds. The best rather than the mean
/// because the thing being measured is the work, and every sample above
/// the floor is the machine doing something else.
fn best(mut body: impl FnMut()) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        body();
        best = best.min(start.elapsed().as_secs_f64() * 1e3);
    }
    best
}
