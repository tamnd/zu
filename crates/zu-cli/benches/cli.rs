//! What the command line costs before it does anything.
//!
//! Every number here is a whole process: fork, exec, the dynamic loader,
//! argv, and the print. That is the unit a user waits for and the unit a
//! script pays per invocation, and it is the one measurement a library
//! bench cannot give, because in-process the same work is a few
//! microseconds and the ten milliseconds around it belong to the
//! operating system and the size of the binary.
//!
//! It matters for two reasons dx/12 names. Help is generated from a
//! table and rendered on demand, so `zu --help` has to stay a print and
//! not become a build; and the reference pages come out of `zu --help
//! --format json`, which a docs job runs once per page. Both are
//! reported against the floor that `zu version` sets, so a regression
//! shows up as help pulling away from the floor rather than as a number
//! that moved because the CI machine was busy.
//!
//! With ZU_GATE=1 the process exits nonzero when startup misses the
//! ceiling in bench/budgets.toml.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-cli

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// The binary under test, built by cargo before this runs.
const ZU: &str = env!("CARGO_BIN_EXE_zu");

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let edges = dir.path().join("edges.txt");
    let graph = dir.path().join("graph.zu1");
    std::fs::write(&edges, "1 2\n1 3\n2 3\n3 1\n").expect("write edges");
    run(&["copy", &name(&edges), &name(&graph)]);

    // The floor: the shortest thing this binary can be asked to do. Every
    // other line is this plus its own work, so it is subtracted from them
    // rather than compared with them.
    let floor = best(&["version"]);
    println!("startup           {floor:8.2} ms/run   zu version");

    let mut failed = Vec::new();
    if let Some(ceiling) = budget("cli_startup_ms")
        && floor > ceiling
    {
        failed.push(format!(
            "cli_startup_ms: {floor:.2} ms over the {ceiling:.2} ms ceiling"
        ));
    }

    for (label, args) in [
        ("help text", &["--help"][..]),
        ("help json", &["--help", "--format", "json"][..]),
        ("help page", &["copy", "--help"][..]),
        ("version json", &["version", "--format", "json"][..]),
    ] {
        let ms = best(args);
        println!(
            "{label:<17} {ms:8.2} ms/run   {:+.2} ms over startup",
            ms - floor
        );
        // Rendering the whole table is a few hundred formatted bytes. If
        // it ever costs as much again as starting the process, something
        // in it is reading a file or building a document per line, which
        // is the failure this bench exists to name.
        assert!(
            ms < floor * 2.0,
            "`zu {}` at {ms:.2} ms is more than twice the {floor:.2} ms floor, \
             so rendering it costs more than the process it prints from",
            args.join(" ")
        );
    }

    // A file open and its metadata read, which is what an install does
    // first and the smallest end to end use of the database there is.
    let stat = best(&["stat", &name(&graph)]);
    println!(
        "stat              {stat:8.2} ms/run   {:+.2} ms over startup",
        stat - floor
    );

    if !failed.is_empty() {
        for line in &failed {
            eprintln!("GATE {line}");
        }
        if std::env::var("ZU_GATE").is_ok_and(|v| v == "1") {
            std::process::exit(1);
        }
    }
}

/// Milliseconds for one run, best of seven so a scheduler hiccup or a
/// cold page cache does not become the reported latency. Process spawn
/// is noisy in a way an in-process loop is not, so the sample is the
/// minimum and never a mean.
fn best(args: &[&str]) -> f64 {
    // One untimed run so the binary and its libraries are in page cache
    // and the first timed spawn is not measuring the disk.
    run(args);
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        for _ in 0..10 {
            run(args);
        }
        best = best.min(start.elapsed().as_secs_f64() * 1e3 / 10.0);
    }
    best
}

/// One run with both streams thrown away, since what is being timed is
/// the work and not the terminal.
fn run(args: &[&str]) {
    let status = Command::new(ZU)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("the binary runs");
    assert!(status.success(), "zu {} failed", args.join(" "));
}

fn name(p: &Path) -> String {
    p.display().to_string()
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
