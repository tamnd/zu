//! What holding the client table and rendering its page costs.
//!
//! Seven clients will not be seven forever. The tier 3 bindings of
//! dx/11 arrive through the kit, each one a repository with a tier, a
//! maintainer and a scorecard, and the two things that go wrong as they
//! arrive are both here. The check is one table compared against
//! another, which is a lookup per client that can quietly become a
//! scan. The page is rendered from the same rows, and a render that
//! scores a client by walking every item for every other client is the
//! kind of cost nobody notices at seven.
//!
//! So both columns are per client, and both are held to being flat.
//!
//! Run: cargo bench -p xtask --bench clients

use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use xtask::clients::{PATH, Table};
use xtask::repos;

fn main() {
    println!("{:>9}  {:>9}  {:>9}", "clients", "parse ms", "us/client");
    let mut per_row = None;
    for rows in [8usize, 32, 128, 512] {
        let text = table(rows);
        let ms = best(|| {
            black_box(Table::parse(black_box(&text)).expect("the generated table parses"));
        });
        let us = ms * 1e3 / rows as f64;
        println!("{rows:9}  {ms:9.3}  {us:9.2}");

        // A pass over the lines and a lookup per row. Four times the
        // cost per client is validation gone quadratic, which is what
        // checking a name against every name before it looks like.
        if let Some((before, was)) = per_row {
            assert!(
                us < was * 4.0,
                "parsing went from {was:.2} us/client at {before} clients to {us:.2} us/client at \
                 {rows}, which is not linear"
            );
        }
        per_row = Some((rows, us));
    }

    println!("\n{:>9}  {:>9}  {:>9}", "clients", "hold ms", "us/client");
    let mut per_client = None;
    for rows in [8usize, 32, 128, 512] {
        let table = Table::parse(&table(rows)).expect("the generated table parses");
        let split = repos::Table::parse(&split(rows)).expect("the generated split parses");
        let page = table.render(&split);
        let ms = best(|| {
            let notes = table.hold(black_box(&split), black_box(Some(&page)), true);
            assert!(notes.is_empty(), "{notes:?}");
        });
        let us = ms * 1e3 / rows as f64;
        println!("{rows:9}  {ms:9.3}  {us:9.2}");

        // Holding is the split in both directions, the scores, and a
        // render of the page to compare against. Every one of those is a
        // pass, so the cost per client is flat, and four times it is one
        // of them looking a client up the slow way.
        if let Some((before, was)) = per_client {
            assert!(
                us < was * 4.0,
                "holding went from {was:.2} us/client at {before} clients to {us:.2} us/client at \
                 {rows}, which is not linear"
            );
        }
        per_client = Some((rows, us));
    }

    // Cargo runs a bench from the package directory, so the tree is two
    // levels up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let committed = Table::load(&root.join(PATH)).expect("the committed table loads");
    let ms = best(|| {
        black_box(
            committed
                .check(black_box(&root), false)
                .expect("the tree is readable"),
        );
    });
    let split = repos::Table::load(&root.join(repos::PATH)).expect("the committed split loads");
    let met = committed
        .clients
        .iter()
        .filter(|c| committed.score(c, &split).met())
        .count();
    println!(
        "\ncommitted clients: {} of them, {met} at their tier, {ms:.2} ms to check and render",
        committed.clients.len()
    );
}

/// A table of `rows` clients, every one of them holding half of the
/// practice axis, which is the case that scores the most: a client that
/// holds nothing stops at the first item and one that holds everything
/// never looks for what is missing.
fn table(rows: usize) -> String {
    let mut text = String::from(concat!(
        "schema = 1\ndoc = \"What the bench publishes.\"\naudited = \"2026-08-19\"\n\n",
        "[[tier]]\nlevel = 1\nsurface = 100\nconformance = 100\npractice = 90\ndoc = \"The \
         apparatus and everything it keeps.\"\n\n",
        "[[tier]]\nlevel = 2\nsurface = 100\nconformance = 95\npractice = 75\ndoc = \"The whole \
         surface, with declared skips.\"\n\n",
        "[[tier]]\nlevel = 3\nsurface = 90\nconformance = 90\npractice = 50\ndoc = \"Built with \
         the kit.\"\n\n",
        "[[item]]\nname = \"quickstart\"\nweight = 50\ndoc = \"The README's programs, run as \
         printed.\"\n\n",
        "[[item]]\nname = \"misuse\"\nweight = 50\ndoc = \"Wrong programs, and what each one is \
         told.\"\n",
    ));
    for n in 0..rows {
        text.push_str(&format!(
            "\n[[client]]\nrepository = \"https://github.com/tamnd/zu-lang{n}\"\nlanguage = \
             \"Lang{n}\"\npackage = \"zudb\"\nregistry = \"a registry\"\nmaintainer = \"Ada \
             Lovelace <@ada>\"\ntier = 3\nholds = [\"quickstart\"]\ndoc = \"A client the bench \
             generated, scored like any other.\"\n"
        ));
    }
    text
}

/// A split holding exactly those clients, every one of them owing a
/// scorecard back, which is the case with no notes and therefore the
/// one that does the most work before answering.
fn split(rows: usize) -> String {
    let mut text = String::from(
        "schema = 1\ndoc = \"What the bench splits.\"\naudited = \"2026-08-19\"\n\n[[repo]]\nname \
         = \"zu\"\nrole = \"engine\"\ncreated = \"exists\"\ndoc = \"The engine.\"\n",
    );
    for n in 0..rows {
        text.push_str(&format!(
            "\n[[repo]]\nname = \"zu-lang{n}\"\nrole = \"binding\"\ntier = 3\ncreated = \
             \"DX5\"\nworkflow = \"release.yml\"\nreports = [\"scorecard\", \"corpus\"]\ndoc = \"A \
             binding the bench generated, driven by the train like any other.\"\n"
        ));
    }
    text
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
