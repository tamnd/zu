//! What holding the tree to the toolchain table costs.
//!
//! This check runs in every job that runs the test suite, on three
//! platforms, in nine repositories eventually, which is the reason to
//! measure it at all: a gate that costs a second everywhere is a second
//! nine times a day forever, and a gate that costs a few milliseconds is
//! free. The number to beat is the whole table against the whole tree in
//! well under a hundred milliseconds.
//!
//! The second column is the one that decides whether the table can grow
//! past its first two dozen rows. A site is one scan of the file it
//! names, so the total is linear in the sites, and the failure mode
//! worth catching is the accidental square: a scan of every file for
//! every site, which costs nothing at six sites and is the reason a
//! check gets abandoned at two hundred. The per-site cost below is flat,
//! which is what says the shape is the first one and not the second.
//!
//! Run: cargo bench -p xtask --bench pins

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use xtask::pins::Table;
use xtask::scratch::Scratch;

fn main() {
    println!("{:>9}  {:>9}  {:>9}", "rows", "parse ms", "us/row");
    let mut per_row = None;
    for rows in [16usize, 64, 256, 1_024] {
        let text = table(rows);
        let ms = best(|| {
            black_box(Table::parse(black_box(&text)).expect("the generated table parses"));
        });
        let us = ms * 1e3 / rows as f64;
        println!("{rows:9}  {ms:9.3}  {us:9.2}");

        // Parsing is a pass over the lines and a lookup per site. Four
        // times the cost per row is a table whose validation went
        // quadratic in its own rows.
        if let Some((before, was)) = per_row {
            assert!(
                us < was * 4.0,
                "parsing went from {was:.2} us/row at {before} rows to {us:.2} us/row at {rows}, \
                 which is not linear"
            );
        }
        per_row = Some((rows, us));
    }

    println!("\n{:>9}  {:>9}  {:>9}", "sites", "check ms", "us/site");
    let mut per_site = None;
    for sites in [8usize, 32, 128, 512] {
        let text = table(sites);
        let table = Table::parse(&text).expect("the generated table parses");
        let dir = tree(sites);
        let ms = best(|| {
            black_box(table.check(black_box(&dir)).expect("the tree is readable"));
        });
        let us = ms * 1e3 / sites as f64;
        println!("{sites:9}  {ms:9.3}  {us:9.2}");

        // A site reads its own file once. Four times the cost per site
        // is every site reading every file, which is the shape that
        // works until it does not.
        if let Some((before, was)) = per_site {
            assert!(
                us < was * 4.0,
                "checking went from {was:.2} us/site at {before} sites to {us:.2} us/site at \
                 {sites}, which is not linear"
            );
        }
        per_site = Some((sites, us));
    }

    // Cargo runs a bench from the package directory, so the tree is two
    // levels up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let committed = Table::load(&root.join(xtask::pins::PATH)).expect("the committed table loads");
    let ms = best(|| {
        black_box(
            committed
                .check(black_box(&root))
                .expect("the tree is readable"),
        );
    });
    println!(
        "\ncommitted table: {} components, {} sites, {ms:.2} ms",
        committed.components.len(),
        committed.sites.len(),
    );
}

/// A table of `rows` components, each with one site of its own, which is
/// the shape that costs the most: every row is a row this repository
/// builds against and every one of them has a file to read.
fn table(rows: usize) -> String {
    let mut text = String::from(
        "schema = 1\ndoc = \"The versions the bench pins.\"\naudited = \"2026-08-15\"\n",
    );
    for n in 0..rows {
        text.push_str(&format!(
            "\n[[component]]\nname = \"tool{n}\"\npinned = \"{n}.2.0\"\nfloor = \"{n}.0\"\nrepos = \
             [\"zu\"]\ndoc = \"A component the bench generated, which has a version like any \
             other.\"\n\n[[site]]\ncomponent = \"tool{n}\"\nfile = \"tool{n}.toml\"\nkey = \
             \"version\"\nholds = \"pinned\"\nmatch = \"exact\"\n"
        ));
    }
    text
}

/// The tree that table holds: one file per site, each a few dozen lines
/// the way a real manifest is, and one workflow for the backward pass.
fn tree(sites: usize) -> Scratch {
    let dir = Scratch::new(&format!("pins-bench-{sites}"));
    std::fs::create_dir_all(dir.join(".github/workflows")).expect("the scratch dir is writable");
    std::fs::write(
        dir.join(".github/workflows/ci.yml"),
        "jobs:\n  a:\n    steps:\n      - uses: actions/checkout@v7\n",
    )
    .expect("writes");
    for n in 0..sites {
        let mut text = String::from("[package]\nname = \"tool\"\n");
        for other in 0..32 {
            text.push_str(&format!("dep{other} = \"1.0\"\n"));
        }
        text.push_str(&format!("version = \"{n}.2.0\"\n"));
        std::fs::write(dir.join(format!("tool{n}.toml")), text).expect("writes");
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
