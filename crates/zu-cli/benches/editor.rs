//! What one keystroke costs in the interactive shell.
//!
//! The number that matters to a user is the whole path a key travels:
//! the bytes off the terminal are decoded, the editor applies the key,
//! and the escape sequence that shows the result is built. That is the
//! work between a key going down and the character appearing, minus the
//! write, which belongs to the terminal and to the link it is on. A
//! keystroke is not a place to be clever, but it is a place to be
//! measured, because a redraw is quadratic-shaped work by nature: every
//! key rebuilds the whole line, and a rebuild that starts allocating per
//! character stops being noticeable at exactly the buffer size where
//! somebody is writing a real query.
//!
//! The modules are pulled in by path rather than by import, because the
//! editor lives in a binary crate and a bench cannot depend on one. It
//! is the same source the shell runs, compiled again here.
//!
//! With ZU_GATE=1 the process exits nonzero when a keystroke misses the
//! ceiling in bench/budgets.toml.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-cli --bench editor

// The editor's source is compiled whole, tests and all, and only the
// typing path is measured, so the parts a terminal loop would call are
// unused here. This is the cost of a bench that runs the same source
// the shell does rather than a copy of it that can drift from it.
#![allow(dead_code, unused_imports)]

use std::time::Instant;

#[path = "../src/complete.rs"]
mod complete;
#[path = "../src/highlight.rs"]
mod highlight;
#[path = "../src/keys.rs"]
mod keys;
#[path = "../src/line.rs"]
mod line;

use complete::Names;
use keys::Decoded;
use line::Editor;

/// A statement of the length people actually type, and one of the
/// length a hand-written pattern match reaches.
const SHORT: &str = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name";
const LONG: &str = "MATCH (a:Person {city: 'Hanoi'})-[:KNOWS]->(b:Person)\n\
     WHERE b.age > 30 AND b.name STARTS WITH 'N'\n\
     WITH a, b, count(*) AS mutual\n\
     ORDER BY mutual DESC\n\
     RETURN a.name AS src, b.name AS to, mutual\n\
     LIMIT 25";

fn main() {
    let short = keystroke(SHORT, false);
    let long = keystroke(LONG, false);
    let painted = keystroke(LONG, true);
    println!(
        "keystroke short   {short:8.2} us/key    {} columns",
        SHORT.len()
    );
    println!(
        "keystroke long    {long:8.2} us/key    {} lines",
        LONG.lines().count()
    );
    println!(
        "keystroke colour  {painted:8.2} us/key    {} lines painted",
        LONG.lines().count()
    );
    let scan = completeness();
    println!(
        "completeness      {scan:8.2} us/call   {} bytes",
        LONG.len()
    );
    let tab = completion();
    println!("completion        {tab:8.2} us/tab");

    let worst = short.max(long).max(painted);
    if let Some(ceiling) = budget("cli_keystroke_us")
        && worst > ceiling
    {
        eprintln!("GATE cli_keystroke_us: {worst:.2} us over the {ceiling:.2} us ceiling");
        if std::env::var("ZU_GATE").is_ok_and(|v| v == "1") {
            std::process::exit(1);
        }
    }
}

/// Microseconds for one key, typing a statement out one character at a
/// time and redrawing after each, which is what the loop does.
///
/// The whole statement is typed per sample rather than one key of it,
/// so the number is an average over every buffer length up to the full
/// one and not a measurement of the cheapest position.
fn keystroke(statement: &str, colour: bool) -> f64 {
    let bytes: Vec<Vec<u8>> = statement
        .chars()
        .map(|c| c.to_string().into_bytes())
        .collect();
    let keys = bytes.len();
    let mut best = f64::MAX;
    let mut sink = 0usize;
    for _ in 0..20 {
        let start = Instant::now();
        let mut editor = Editor::new(colour);
        for byte in &bytes {
            let Decoded::Key(key, _) = keys::decode(byte) else {
                unreachable!("a character decodes as a character")
            };
            editor.step(key);
            sink += editor.render(100, "zu> ", "..> ").len();
        }
        best = best.min(start.elapsed().as_secs_f64() * 1e6 / keys as f64);
    }
    assert!(
        sink > 0,
        "the renders were not thrown away by the optimizer"
    );
    best
}

/// Microseconds for one scan of the statement return has to judge.
fn completeness() -> f64 {
    let mut best = f64::MAX;
    let mut sink = 0usize;
    for _ in 0..20 {
        let start = Instant::now();
        for _ in 0..1000 {
            sink += usize::from(line::finished(LONG));
        }
        best = best.min(start.elapsed().as_secs_f64() * 1e6 / 1000.0);
    }
    assert!(sink > 0, "the scans were not thrown away by the optimizer");
    best
}

/// Microseconds for one tab, which is the scan for the word under the
/// cursor plus the walk over every name the file has.
///
/// The name lists are the size a real schema reaches rather than the
/// size a test uses, because the walk is linear in them and a shell
/// that got slower as a graph grew a hundred labels would be a shell
/// that got slower as it got useful.
fn completion() -> f64 {
    let mut names = Names::default();
    for i in 0..100 {
        names.labels.push(format!("Label{i}"));
        names.tables.push(format!("table_{i}"));
        names.properties.push(format!("property_{i}"));
    }
    names.tidy();
    let text = "MATCH (a:Lab";
    let mut best = f64::MAX;
    let mut sink = 0usize;
    for _ in 0..20 {
        let start = Instant::now();
        for _ in 0..1000 {
            sink += complete::complete(text, text.len(), &names).list.len();
        }
        best = best.min(start.elapsed().as_secs_f64() * 1e6 / 1000.0);
    }
    assert!(sink > 0, "the tabs were not thrown away by the optimizer");
    best
}

/// One number out of bench/budgets.toml, or none when the key is absent.
fn budget(key: &str) -> Option<f64> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|line| line.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .and_then(|(_, v)| v.split('#').next()?.trim().parse().ok())
}
