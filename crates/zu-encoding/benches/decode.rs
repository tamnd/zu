//! Decode throughput bench and the M0 performance gate.
//!
//! Uses real data when available: neighbor ids from soc-LiveJournal1
//! (set ZU_DATA to the directory holding soc-LiveJournal1.txt), otherwise a
//! synthetic sorted id stream with LiveJournal-like gap distribution.
//! With ZU_GATE=1 the process exits nonzero if any encoding decodes below
//! its floor in bench/budgets.toml, measured as decoded output bytes/s.
//!
//! Run: ZU_GATE=1 ZU_DATA=~/data/zu cargo bench -p zu-encoding

use std::time::Instant;

const TARGET_VALUES: usize = 16 << 20;

fn load_livejournal(dir: &str) -> Option<Vec<u64>> {
    let path = format!("{dir}/soc-LiveJournal1.txt");
    let text = std::fs::read_to_string(&path).ok()?;
    let mut ids: Vec<u64> = Vec::with_capacity(TARGET_VALUES);
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(_src), Some(dst)) = (parts.next(), parts.next()) else {
            continue;
        };
        ids.push(dst.parse().ok()?);
        if ids.len() >= TARGET_VALUES {
            break;
        }
    }
    // Neighbor lists are stored sorted per source in CSR; sorting the
    // sample models the on-disk stream the decoder actually sees.
    ids.sort_unstable();
    println!("data: soc-LiveJournal1.txt, {} neighbor ids", ids.len());
    Some(ids)
}

fn synthetic() -> Vec<u64> {
    let mut rng = 0x2545F4914F6CDD1Du64;
    let mut v = 0u64;
    let ids = (0..TARGET_VALUES)
        .map(|_| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            v += rng % 128;
            v
        })
        .collect();
    println!("data: synthetic sorted ids ({TARGET_VALUES} values), set ZU_DATA for real input");
    ids
}

struct Budgets(Vec<(String, f64)>);

impl Budgets {
    fn load() -> Self {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/budgets.toml");
        let mut floors = Vec::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if let Some((k, v)) = line.split_once('=')
                    && let Ok(f) = v.trim().parse::<f64>()
                {
                    floors.push((k.trim().to_string(), f));
                }
            }
        }
        Self(floors)
    }

    fn floor(&self, key: &str) -> Option<f64> {
        self.0.iter().find(|(k, _)| k == key).map(|&(_, f)| f)
    }
}

fn measure(name: &str, encoded: &[u8], count: usize, decode: DecodeFn) -> f64 {
    let mut out = Vec::with_capacity(count);
    // Warm up, then run for at least a second of wall time.
    decode(encoded, count, &mut out).unwrap();
    let mut iters = 0u32;
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < 1.0 {
        out.clear();
        decode(encoded, count, &mut out).unwrap();
        iters += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    let decoded_bytes = (count as f64) * 8.0 * f64::from(iters);
    let gbps = decoded_bytes / secs / 1e9;
    let ratio = (count as f64) * 8.0 / (encoded.len() as f64);
    println!("{name}: {gbps:.2} GB/s decode, {ratio:.1}x vs raw, {iters} iters");
    gbps
}

type EncodeFn = fn(&[u64], &mut Vec<u8>) -> usize;
type DecodeFn = fn(&[u8], usize, &mut Vec<u64>) -> zu_common::Result<()>;

fn main() {
    let data = std::env::var("ZU_DATA")
        .ok()
        .and_then(|d| load_livejournal(&d))
        .unwrap_or_else(synthetic);
    let budgets = Budgets::load();
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let mut failed = false;

    let cases: [(&str, EncodeFn, DecodeFn); 5] = [
        (
            "for_bitpack",
            zu_encoding::for_bitpack::encode,
            zu_encoding::for_bitpack::decode,
        ),
        (
            "delta_bitpack",
            zu_encoding::delta::encode,
            zu_encoding::delta::decode,
        ),
        (
            "delta_patch",
            zu_encoding::delta_patch::encode,
            zu_encoding::delta_patch::decode,
        ),
        ("rle", zu_encoding::rle::encode, zu_encoding::rle::decode),
        ("dict", zu_encoding::dict::encode, zu_encoding::dict::decode),
    ];

    for (name, encode, decode) in cases {
        let mut encoded = Vec::new();
        encode(&data, &mut encoded);
        let gbps = measure(name, &encoded, data.len(), decode);
        if let Some(floor) = budgets.floor(name)
            && gbps < floor
        {
            println!("GATE FAIL {name}: {gbps:.2} GB/s < floor {floor} GB/s");
            failed = true;
        }
    }

    if gate && failed {
        std::process::exit(1);
    }
    if gate {
        println!("gate: all floors met");
    }
}
