//! What the terminology check costs, per page and per term.
//!
//! Two numbers, and they answer two different objections. The first is
//! the one every prose linter has to answer: a check that takes ten
//! seconds over the tree is a check that runs in CI and nowhere else,
//! and a check nobody runs locally is a check that only ever reports
//! things at review time. The whole tree is about a megabyte of prose,
//! so the number to beat is a megabyte in well under a second.
//!
//! The second is the one that decides whether the table can grow. The
//! table is forty six terms today and the obvious naive matcher is a
//! scan per form per line, which is a check that gets slower every time
//! somebody adds a word and is therefore a table that stops growing.
//! Forms here are indexed by their first word, so a line costs one hash
//! lookup per word regardless of how many terms exist, and the third
//! column below is the evidence: the cost of a fixed body of prose as
//! the table goes from thirteen forms to a thousand. It is flat, which
//! is what makes "add the term" a decision about the word and not about
//! the budget.
//!
//! Run: cargo bench -p xtask --bench terms

use std::hint::black_box;
use std::time::Instant;

use xtask::terms::{Kind, Table};

fn main() {
    let table = Table::parse(&generated(0)).expect("the generated table parses");
    println!("{:>9}  {:>9}  {:>9}", "KiB", "ms", "MiB/s");
    let mut previous: Option<(usize, f64)> = None;
    for pages in [8usize, 32, 128, 512] {
        let text = prose(pages);
        let kib = text.len().div_ceil(1024);
        let ms = best(|| {
            black_box(table.check("a.md", black_box(&text), Kind::Markdown));
        });
        let mbs = text.len() as f64 / 1024.0 / 1024.0 / (ms / 1e3);
        println!("{kib:9}  {ms:9.3}  {mbs:9.1}");

        // A factor of two either way is noise on a shared runner. Four
        // is a cost that is not linear in the prose.
        let per = ms / pages as f64;
        if let Some((before, previous_per)) = previous {
            assert!(
                per < previous_per * 4.0,
                "per-page checking went from {previous_per:.4} ms at {before} pages to {per:.4} ms \
                 at {pages}, which is not linear"
            );
        }
        previous = Some((pages, per));
    }

    println!("\n{:>9}  {:>9}  {:>9}", "forms", "ms", "MiB/s");
    let text = prose(128);
    let mut first = None;
    for extra in [0usize, 100, 400, 1_000] {
        let table = Table::parse(&generated(extra)).expect("the generated table parses");
        let ms = best(|| {
            black_box(table.check("a.md", black_box(&text), Kind::Markdown));
        });
        println!(
            "{:9}  {ms:9.3}  {:9.1}",
            table.forms(),
            text.len() as f64 / 1024.0 / 1024.0 / (ms / 1e3)
        );
        let base = *first.get_or_insert(ms);

        // The index is by first word, so a table seventy times the size
        // is the same number of hash lookups and a handful more
        // comparisons. Three times the cost would mean it is not.
        assert!(
            ms < base * 3.0,
            "{} forms cost {ms:.3} ms against {base:.3} ms at thirteen, which is not an index",
            table.forms()
        );
    }

    // Cargo runs a bench from the package directory, so the tree is two
    // levels up and zu-web is beside the tree rather than beside us. The
    // two candidates are the two places `beside` looks, for the two
    // reasons it looks there: CI clones the site into the tree, a person
    // clones it next to the tree.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Some(path) = ["zu-web", "../zu-web"]
        .iter()
        .map(|dir| root.join(dir).join(xtask::terms::PATH))
        .find(|path| path.exists())
    else {
        println!("\nthe committed table is not beside the tree, so the tree is not timed");
        return;
    };
    let table = Table::load(&path).expect("the committed table loads");
    let mut files = 0;
    let mut bytes = 0;
    let mut prose = Vec::new();
    for dir in ["docs", "crates", "conformance"] {
        walk(&root.join(dir), &mut prose);
    }
    for (kind, text) in &prose {
        files += 1;
        bytes += text.len();
        let _ = table.check("a", text, *kind);
    }
    let ms = best(|| {
        for (kind, text) in &prose {
            black_box(table.check("a", black_box(text), *kind));
        }
    });
    println!(
        "\ncommitted tree: {files} files, {} KiB of source, {} terms, {} forms, {ms:.1} ms",
        bytes.div_ceil(1024),
        table.terms.len(),
        table.forms(),
    );
}

/// A table of thirteen forms plus `extra` generated ones. The generated
/// terms are two words each, so they land in the multi-word path that
/// costs the most, and each has a distinct first word, so they spread
/// across the index the way real terms do rather than piling into one
/// bucket.
fn generated(extra: usize) -> String {
    let mut text = [
        "schema: 1",
        "doc: The words the bench uses, and the words it does not.",
        "",
        "terms:",
        "  - term: node",
        "    group: graph model",
        "    doc: An element of a graph, held by one node table.",
        "    instead:",
        "      - vertex",
        "      - vertices",
        "  - term: rel table",
        "    group: graph model",
        "    doc: The table an edge lives in, declared by CREATE REL TABLE.",
        "    instead:",
        "      - edge table",
        "      - relationship table",
        "  - term: node group",
        "    group: storage",
        "    doc: The unit a table is written and scanned in.",
        "    instead:",
        "      - row group",
        "  - term: optimizer",
        "    group: query",
        "    doc: The pass that turns a bound statement into a plan.",
        "    instead:",
        "      - planner",
        "      - query planner",
        "  - term: zu",
        "    group: names",
        "    doc: The database, the repository and the binary, in lower case.",
        "    instead:",
        "      - Zu",
        "      - ZU",
        "  - term: property",
        "    group: graph model",
        "    doc: A named value on a node or an edge.",
        "    instead:",
        "      - attribute",
        "  - term: GQLSTATUS",
        "    group: values",
        "    doc: The status a statement finishes with, as the standard spells it.",
        "    instead:",
        "      - error code",
        "      - status code",
        "  - term: zu1",
        "    group: names",
        "    doc: The on disk format, in lower case.",
        "    instead:",
        "      - ZU1",
        "",
    ]
    .join("\n");
    for n in 0..extra {
        text.push_str(&format!(
            "  - term: term{n} thing\n\
             \x20   group: generated\n\
             \x20   doc: A term the bench generated, which takes up room in the index.\n\
             \x20   instead:\n\
             \x20     - form{n} thing\n"
        ));
    }
    text
}

/// `pages` of markdown, each a heading and eight paragraphs. The prose
/// mentions terms the table has and forms it refuses, in the proportion
/// a real page does, which is mostly neither.
fn prose(pages: usize) -> String {
    let mut text = String::new();
    for page in 0..pages {
        text.push_str(&format!("# The page numbered {page}\n\n"));
        for paragraph in 0..8 {
            text.push_str(
                "A node group is the unit a node table is written in, and a column segment is one \
                 column of one node group, which is the distinction the storage chapter turns on. \
                 The optimizer sees neither, because a plan is written against tables and the \
                 sizes are what the scan picks up afterwards. ",
            );
            if paragraph % 4 == 0 {
                text.push_str(
                    "A row group in the paper this borrows from is the same thing under an older \
                     name, and a vertex is what that literature calls a node. ",
                );
            }
            text.push_str("The `Node` type and `node_group()` are code and not prose.\n\n");
        }
        text.push_str("```rust\nlet vertex = Vertex::new();\n```\n\n");
    }
    text
}

/// Every markdown and Rust file under `root`, read once, so the timing
/// below is the check and not the filesystem.
fn walk(root: &std::path::Path, out: &mut Vec<(Kind, String)>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            walk(&path, out);
        } else if let Some(kind) = Kind::of(&path)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            out.push((kind, text));
        }
    }
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
