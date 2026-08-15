//! What holding the project's own list of repositories costs.
//!
//! Nine rows will not be nine forever. Tier 3 bindings arrive through
//! the kit, each one a repository with a tier and a maintainer, and the
//! shape that goes wrong as they arrive is not the parse, it is the
//! check: three sources compared against one table, and each comparison
//! is a lookup that can quietly become a scan.
//!
//! So the second column is the one that matters. Holding the conductor,
//! the README and the artifact contract to the table is one pass over
//! each and a lookup per row, and the cost per repository is flat. Four
//! times the cost per repository at four times the repositories is a
//! lookup that walks, which costs nothing at nine and is the reason a
//! check gets dropped at two hundred.
//!
//! Run: cargo bench -p xtask --bench repos

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use xtask::artifacts;
use xtask::repos::{PATH, Table};

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

        // Parsing is a pass over the lines and a lookup per row, the
        // same shape as every other table here. Four times the cost per
        // row is validation that went quadratic, which is what a name
        // checked against every name before it looks like.
        if let Some((before, was)) = per_row {
            assert!(
                us < was * 4.0,
                "parsing went from {was:.2} us/row at {before} rows to {us:.2} us/row at {rows}, \
                 which is not linear"
            );
        }
        per_row = Some((rows, us));
    }

    println!("\n{:>9}  {:>9}  {:>9}", "repos", "hold ms", "us/repo");
    let mut per_repo = None;
    for rows in [8usize, 32, 128, 512] {
        let table = Table::parse(&table(rows)).expect("the generated table parses");
        let workflow = conductor(rows);
        let readme = clients(rows);
        let contract = artifacts::Table::parse(&published(rows)).expect("the contract parses");
        let ms = best(|| {
            let notes = table
                .hold(black_box(&workflow), black_box(&readme), &contract)
                .expect("the matrix reads");
            assert!(notes.is_empty(), "{notes:?}");
        });
        let us = ms * 1e3 / rows as f64;
        println!("{rows:9}  {ms:9.3}  {us:9.2}");

        // Three passes and a lookup per row. Four times the cost per
        // repository is one of the three looking every repository up by
        // walking the table.
        if let Some((before, was)) = per_repo {
            assert!(
                us < was * 4.0,
                "holding went from {was:.2} us/repo at {before} repositories to {us:.2} us/repo \
                 at {rows}, which is not linear"
            );
        }
        per_repo = Some((rows, us));
    }

    // Cargo runs a bench from the package directory, so the tree is two
    // levels up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let committed = Table::load(&root.join(PATH)).expect("the committed table loads");
    let ms = best(|| {
        black_box(
            committed
                .check(black_box(&root))
                .expect("the tree is readable"),
        );
    });
    let reports: usize = committed.dispatched().map(|r| r.reports.len()).sum();
    println!(
        "\ncommitted split: {} repositories, {} dispatches, {reports} reports collected, {ms:.2} ms",
        committed.repos.len(),
        committed.dispatched().count(),
    );
}

/// A table of `rows` repositories, one engine and the rest on the
/// train, which is the shape that costs the most: every row the engine
/// does not own is a row all three checks look at.
fn table(rows: usize) -> String {
    let mut text = String::from(
        "schema = 1\ndoc = \"What the bench splits.\"\naudited = \"2026-08-16\"\n\n[[repo]]\nname \
         = \"zu\"\nrole = \"engine\"\ncreated = \"exists\"\ndoc = \"The engine.\"\n",
    );
    for n in 1..rows {
        text.push_str(&format!(
            "\n[[repo]]\nname = \"zu-lang{n}\"\nrole = \"binding\"\ntier = 3\ncreated = \
             \"DX5\"\nworkflow = \"release.yml\"\nreports = [\"scorecard\", \"corpus\"]\ndoc = \"A \
             binding the bench generated, driven by the train like any other.\"\n"
        ));
    }
    text
}

/// A conductor that dispatches to exactly those repositories.
fn conductor(rows: usize) -> String {
    let mut text =
        String::from("jobs:\n  dispatch:\n    strategy:\n      matrix:\n        include:\n");
    for n in 1..rows {
        text.push_str(&format!(
            "          - repo: zu-lang{n}\n            workflow: release.yml\n            \
             reports: scorecard corpus\n"
        ));
    }
    text
}

/// A README listing exactly those repositories.
fn clients(rows: usize) -> String {
    let mut text = String::from("| Repository | What it is | Tier |\n|---|---|---|\n");
    for n in 1..rows {
        text.push_str(&format!(
            "| [zu-lang{n}](https://github.com/tamnd/zu-lang{n}) | A binding | 3 |\n"
        ));
    }
    text
}

/// A contract every one of those repositories consumes something from,
/// which is the case with no notes and therefore the one that does the
/// most work before answering.
fn published(rows: usize) -> String {
    let consumers: Vec<String> = (1..rows).map(|n| format!("\"zu-lang{n}\"")).collect();
    format!(
        "schema = 1\ndoc = \"What the bench publishes.\"\naudited = \"2026-08-16\"\n\n\
         [[artifact]]\nname = \"model.json\"\nmade = \"file\"\nfrom = \
         \"docs/api/model.json\"\nconsumers = [{}]\ndoc = \"The API model, which everything \
         generates from.\"\n",
        consumers.join(", ")
    )
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
