//! Encode throughput bench: what a fold pays to put a chunk on disk.
//!
//! The decode bench beside this one measures the read side, one big
//! buffer at a time. This is the write side, and it measures the way
//! storage calls it: `encode_auto` per MiniBlock chunk of 1024 values,
//! which is what a column rewrite runs once per chunk it touched and
//! what a bulk load runs over every chunk it writes. The number that
//! matters is input bytes per second, since that is what the fold has
//! to get through, and the ratio is printed beside it because a faster
//! encoder that ships bigger blocks has not helped anybody.
//!
//! The shapes are the ones storage actually holds: neighbor ids in CSR
//! order, the gap stream those ids leave, a dense row counter, a
//! categorical column, a boolean column, and a scattered wide column
//! where nothing compresses.
//!
//! With ZU_GATE=1 the process exits nonzero if any shape encodes below
//! its floor in bench/budgets.toml.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-encoding --bench encode

use std::time::Instant;

use zu_encoding::segment::encode_auto;

/// Values per call, the MiniBlock chunk storage encodes at.
const CHUNK: usize = 1024;
/// Values per shape. Big enough that the loop below is measuring the
/// encoder rather than the clock, small enough to stay in cache.
const VALUES: usize = 1 << 20;

fn budget(key: &str) -> Option<f64> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
    for line in std::fs::read_to_string(path).ok()?.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            return v.trim().parse().ok();
        }
    }
    None
}

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Sorted neighbor ids with a power-law-ish gap distribution, which is
/// the stream a CSR holds and the one the fold rewrites most of.
fn neighbor_ids() -> Vec<u64> {
    let mut rng = 0x2545F4914F6CDD1Du64;
    let mut v = 0u64;
    (0..VALUES)
        .map(|_| {
            v += xorshift(&mut rng) % 128;
            v
        })
        .collect()
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let ids = neighbor_ids();
    let gaps: Vec<u64> = ids.windows(2).map(|w| w[1] - w[0]).collect();
    let rows: Vec<u64> = (0..VALUES as u64).collect();
    let mut rng = 0xABCDEFu64;
    let categorical: Vec<u64> = (0..VALUES)
        .map(|_| (xorshift(&mut rng) % 200).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .collect();
    let flags: Vec<u64> = ids.iter().map(|&v| v & 1).collect();
    let wide: Vec<u64> = (0..VALUES).map(|_| xorshift(&mut rng)).collect();

    let cases: [(&str, &[u64]); 6] = [
        ("ids", &ids),
        ("gaps", &gaps),
        ("rows", &rows),
        ("categorical", &categorical),
        ("flags", &flags),
        ("wide", &wide),
    ];

    println!("shape          encode      ratio   pick");
    let mut failed = false;
    for (name, values) in cases {
        let mut out = Vec::with_capacity(values.len() * 9);
        let mut pick = None;
        // Warm up, then run for at least a second of wall time. Every
        // pass encodes every chunk, so the pick is per chunk the way
        // storage picks it.
        for chunk in values.chunks(CHUNK) {
            pick = Some(encode_auto(chunk, &mut out));
        }
        let encoded = out.len();
        let mut iters = 0u32;
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < 1.0 {
            out.clear();
            for chunk in values.chunks(CHUNK) {
                encode_auto(chunk, &mut out);
            }
            iters += 1;
        }
        let secs = start.elapsed().as_secs_f64();
        let mbps = (values.len() as f64) * 8.0 * f64::from(iters) / secs / 1e6;
        let ratio = (values.len() as f64) * 8.0 / encoded as f64;
        let pick = pick.expect("a chunk was encoded");
        println!("{name:<14} {mbps:>6.0} MB/s  {ratio:>5.1}x   {pick:?}");
        let key = format!("encode_{name}_mbps");
        if let Some(floor) = budget(&key)
            && mbps < floor
        {
            println!("GATE FAIL {key}: {mbps:.0} MB/s < floor {floor} MB/s");
            failed = true;
        }
    }
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: all floors met");
    }
}
