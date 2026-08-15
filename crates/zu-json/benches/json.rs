//! Reader and writer throughput, and the shell frame latency gate.
//!
//! Two sizes matter and they are not the same problem. A shell frame is
//! a hundred bytes parsed once per request, and the number that counts
//! is time to parse one, because it sits in front of every query a
//! binding sends over the pipe: a frame that takes longer than a warm
//! point read (10 us, docs/00 G1) would mean the transport costs more
//! than the database. A rustdoc document is a third of a megabyte
//! parsed once per codegen run, and the number that counts there is
//! bytes per second.
//!
//! With ZU_GATE=1 the process exits nonzero when the frame misses its
//! ceiling in bench/budgets.toml. The document throughput is reported
//! and not gated: it runs on a developer machine or in a codegen job,
//! and putting a floor on a build tool buys a flaky check.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-json

use std::hint::black_box;
use std::time::Instant;

use zu_json::{Json, parse};

/// A frame of the shape `zu shell` actually reads: an execute with a
/// handful of bound parameters.
const FRAME: &str = r#"{"op":"execute","stmt":7,"params":{"src":1234567,"depth":3,"name":"Ada Lovelace","weight":0.5,"exact":true,"tag":null}}"#;

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let mut failed = Vec::new();

    let frame_ns = frame_parse();
    println!(
        "frame parse       {frame_ns:8.0} ns/frame  ({} bytes)",
        FRAME.len()
    );
    if let Some(ceiling) = budget("json_frame_parse_ns")
        && frame_ns > ceiling
    {
        failed.push(format!(
            "json_frame_parse_ns: {frame_ns:.0} ns over the {ceiling:.0} ns ceiling"
        ));
    }

    // Two sizes, because the number that matters for a reader is not
    // the throughput but whether it holds. A parser that validates the
    // tail of the input once per character reads at a perfectly
    // respectable rate on a frame and grinds to a halt on a document,
    // and one measurement cannot tell the difference.
    let mut previous: Option<(f64, f64)> = None;
    for items in [100, 400, 1600] {
        let doc = document(items);
        let bytes = doc.len() as f64;
        let read = throughput(bytes, || {
            black_box(parse(black_box(&doc)).expect("the document parses"));
        });
        println!(
            "document read     {read:8.2} MB/s      ({:.0} KiB)",
            bytes / 1024.0
        );
        if let Some((prev_bytes, prev_read)) = previous {
            assert!(
                read > prev_read / 2.0,
                "reading fell from {prev_read:.2} MB/s at {prev_bytes:.0} bytes \
                 to {read:.2} MB/s at {bytes:.0}, which is not linear"
            );
        }
        previous = Some((bytes, read));
    }

    let doc = document(400);
    let bytes = doc.len() as f64;
    let parsed = parse(&doc).expect("the document parses");
    let compact = throughput(bytes, || {
        black_box(black_box(&parsed).to_compact());
    });
    println!("document write    {compact:8.2} MB/s      compact");
    let pretty = throughput(bytes, || {
        black_box(black_box(&parsed).to_pretty());
    });
    println!("document write    {pretty:8.2} MB/s      pretty");

    // Writing the artifact twice has to give the same bytes, and a
    // bench is the one place that runs it enough times to notice a
    // writer that reached for a hash map.
    let once = parsed.to_pretty();
    assert!(
        (0..64).all(|_| parsed.to_pretty() == once),
        "the writer is not deterministic"
    );

    if gate && !failed.is_empty() {
        for line in &failed {
            eprintln!("GATE {line}");
        }
        std::process::exit(1);
    }
}

/// Nanoseconds to parse one frame, taken as the best of several runs so
/// a scheduler hiccup does not become the reported latency.
fn frame_parse() -> f64 {
    let mut best = f64::MAX;
    for _ in 0..8 {
        let start = Instant::now();
        for _ in 0..20_000 {
            black_box(parse(black_box(FRAME)).expect("the frame parses"));
        }
        let ns = start.elapsed().as_secs_f64() * 1e9 / 20_000.0;
        best = best.min(ns);
    }
    best
}

fn throughput(bytes: f64, mut body: impl FnMut()) -> f64 {
    // One untimed pass so the allocator has grown before the clock
    // starts and the first run is not measuring page faults.
    body();
    let mut best: f64 = 0.0;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..8 {
            body();
        }
        let mbps = bytes * 8.0 / start.elapsed().as_secs_f64() / 1e6;
        best = best.max(mbps);
    }
    best
}

/// A document shaped like rustdoc's: many small objects, string-heavy,
/// nested a few levels, with the odd number and boolean.
fn document(items: usize) -> String {
    let entry =
        |i: usize| {
            Json::Obj(vec![
            ("id".into(), Json::Str(format!("zu::module{}::Thing{i}", i % 7))),
            ("kind".into(), Json::Str("struct".into())),
            (
                "docs".into(),
                Json::Str(
                    "One paragraph of documentation, of the length a real doc comment runs to, \
                     with a \"quoted\" phrase and a newline\nin the middle of it."
                        .into(),
                ),
            ),
            (
                "span".into(),
                Json::Obj(vec![
                    ("filename".into(), Json::Str(format!("crates/zu/src/m{i}.rs"))),
                    ("begin".into(), Json::Arr(vec![Json::Int(i as i64), Json::Int(1)])),
                    ("end".into(), Json::Arr(vec![Json::Int(i as i64 + 9), Json::Int(2)])),
                ]),
            ),
            ("deprecated".into(), Json::Bool(i.is_multiple_of(31))),
            ("weight".into(), Json::Float(i as f64 / 7.0)),
            ("stability".into(), Json::Null),
        ])
        };
    Json::Obj(vec![
        ("format_version".into(), Json::Int(61)),
        (
            "index".into(),
            Json::Obj((0..items).map(|i| (i.to_string(), entry(i))).collect()),
        ),
    ])
    .to_compact()
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
