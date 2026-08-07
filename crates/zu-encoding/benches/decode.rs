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
type FloatEncodeFn = fn(&[f64], &mut Vec<u8>) -> usize;
type FloatDecodeFn = fn(&[u8], usize, &mut Vec<f64>) -> zu_common::Result<()>;
#[cfg(feature = "zstd")]
type ByteDecodeFn = fn(&[u8], usize, &mut Vec<u8>) -> zu_common::Result<()>;

fn main() {
    let data = std::env::var("ZU_DATA")
        .ok()
        .and_then(|d| load_livejournal(&d))
        .unwrap_or_else(synthetic);
    let budgets = Budgets::load();
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let mut failed = false;

    let cases: [(&str, EncodeFn, DecodeFn); 4] = [
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

    // Derived streams for the typed encodings. The parity bit of each
    // neighbor id models a boolean property column at the real length,
    // the gap stream of the sorted ids has a genuinely dominant value,
    // gap zero from duplicate destinations, which is the shape Frequency
    // exists for, and folding each id into a scrambled pool at the
    // format cap models a categorical column, the shape Dict exists
    // for, since the cap makes the raw id stream an illegal dictionary.
    let parity: Vec<u64> = data.iter().map(|&v| v & 1).collect();
    let gaps: Vec<u64> = data.windows(2).map(|w| w[1] - w[0]).collect();
    let categorical: Vec<u64> = data
        .iter()
        .map(|&v| (v % zu_encoding::dict::MAX_ENTRIES as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .collect();
    let typed: [(&str, &[u64], EncodeFn, DecodeFn); 3] = [
        (
            "dict",
            &categorical,
            zu_encoding::dict::encode,
            zu_encoding::dict::decode,
        ),
        (
            "bool_bitpack",
            &parity,
            zu_encoding::bool_bitpack::encode,
            zu_encoding::bool_bitpack::decode,
        ),
        (
            "frequency",
            &gaps,
            zu_encoding::frequency::encode,
            zu_encoding::frequency::decode,
        ),
    ];
    for (name, values, encode, decode) in typed {
        let mut encoded = Vec::new();
        encode(values, &mut encoded);
        let gbps = measure(name, &encoded, values.len(), decode);
        if let Some(floor) = budgets.floor(name)
            && gbps < floor
        {
            println!("GATE FAIL {name}: {gbps:.2} GB/s < floor {floor} GB/s");
            failed = true;
        }
    }

    // FSST works on bytes, not u64 values, so it gets its own corpus and
    // measure loop. Real data is the raw edge-list text itself, which is
    // honest string input: tab-separated ascii numerals with heavy
    // digram repetition, the shape id-bearing string columns take.
    let text = std::env::var("ZU_DATA")
        .ok()
        .and_then(|d| {
            let bytes = std::fs::read(format!("{d}/soc-LiveJournal1.txt")).ok()?;
            let len = bytes.len().min(64 << 20);
            println!("fsst data: soc-LiveJournal1.txt raw text, {len} bytes");
            Some(bytes[..len].to_vec())
        })
        .unwrap_or_else(|| {
            let mut text = Vec::new();
            for pair in data.chunks_exact(2).take(4 << 20) {
                text.extend_from_slice(format!("{}\t{}\n", pair[0], pair[1]).as_bytes());
            }
            println!("fsst data: synthetic edge-list text, {} bytes", text.len());
            text
        });
    let mut encoded = Vec::new();
    zu_encoding::fsst::encode(&text, &mut encoded);
    let mut out = Vec::with_capacity(text.len());
    zu_encoding::fsst::decode(&encoded, text.len(), &mut out).unwrap();
    assert_eq!(out, text, "fsst roundtrip broke on the bench corpus");
    let mut iters = 0u32;
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < 1.0 {
        out.clear();
        zu_encoding::fsst::decode(&encoded, text.len(), &mut out).unwrap();
        iters += 1;
    }
    let secs = start.elapsed().as_secs_f64();
    let gbps = (text.len() as f64) * f64::from(iters) / secs / 1e9;
    let ratio = (text.len() as f64) / (encoded.len() as f64);
    println!("fsst: {gbps:.2} GB/s decode, {ratio:.1}x vs raw, {iters} iters");
    if let Some(floor) = budgets.floor("fsst")
        && gbps < floor
    {
        println!("GATE FAIL fsst: {gbps:.2} GB/s < floor {floor} GB/s");
        failed = true;
    }

    // The Zstd leaf exists for cold string segments, so honest input is
    // the same raw edge-list text fsst measures on. Both read paths run:
    // libzstd behind the feature and the pure Rust ruzstd fallback that
    // default builds decode with.
    #[cfg(feature = "zstd")]
    {
        let mut encoded = Vec::new();
        zu_encoding::zstd_leaf::encode(&text, &mut encoded);
        let leaf_cases: [(&str, ByteDecodeFn); 2] = [
            ("zstd", zu_encoding::zstd_leaf::decode),
            ("zstd_fallback", zu_encoding::zstd_leaf::decode_fallback),
        ];
        for (name, decode) in leaf_cases {
            let mut out = Vec::with_capacity(text.len());
            decode(&encoded, text.len(), &mut out).unwrap();
            assert_eq!(out, text, "{name} roundtrip broke on the bench corpus");
            let mut iters = 0u32;
            let start = Instant::now();
            while start.elapsed().as_secs_f64() < 1.0 {
                out.clear();
                decode(&encoded, text.len(), &mut out).unwrap();
                iters += 1;
            }
            let secs = start.elapsed().as_secs_f64();
            let gbps = (text.len() as f64) * f64::from(iters) / secs / 1e9;
            let ratio = (text.len() as f64) / (encoded.len() as f64);
            println!("{name}: {gbps:.2} GB/s decode, {ratio:.1}x vs raw, {iters} iters");
            if let Some(floor) = budgets.floor(name)
                && gbps < floor
            {
                println!("GATE FAIL {name}: {gbps:.2} GB/s < floor {floor} GB/s");
                failed = true;
            }
        }
    }
    #[cfg(not(feature = "zstd"))]
    println!("zstd: skipped (build without --features zstd)");

    // The float encodings measure on real doubles: the latitude and
    // longitude columns of the Gowalla check-ins (SNAP), 12.8 M values
    // with seven decimal digits, which is ALP's shape. ALP_RD gets the
    // same coordinates in radians: an irrational transform fills the
    // mantissas, which is exactly the fallback's shape. Without ZU_DATA
    // both derive from the synthetic ids at the same scale.
    let floats: Vec<f64> = std::env::var("ZU_DATA")
        .ok()
        .and_then(|d| {
            let text =
                std::fs::read_to_string(format!("{d}/loc-gowalla_totalCheckins.txt")).ok()?;
            let mut lat = Vec::new();
            let mut lon = Vec::new();
            for line in text.lines() {
                let mut parts = line.split_whitespace();
                let (_, _, la, lo) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
                lat.push(la.parse().ok()?);
                lon.push(lo.parse().ok()?);
            }
            lat.extend_from_slice(&lon);
            println!(
                "float data: loc-gowalla_totalCheckins.txt, {} coordinates",
                lat.len()
            );
            Some(lat)
        })
        .unwrap_or_else(|| {
            let coords = data
                .iter()
                .map(|&v| (v % 3_600_000_000) as f64 / 1e7 - 180.0)
                .collect::<Vec<f64>>();
            println!("float data: synthetic coordinates, {} values", coords.len());
            coords
        });
    let radians: Vec<f64> = floats.iter().map(|d| d.to_radians()).collect();
    let float_cases: [(&str, &[f64], FloatEncodeFn, FloatDecodeFn); 2] = [
        (
            "alp",
            &floats,
            zu_encoding::alp::encode,
            zu_encoding::alp::decode,
        ),
        (
            "alp_rd",
            &radians,
            zu_encoding::alp_rd::encode,
            zu_encoding::alp_rd::decode,
        ),
    ];
    for (name, values, encode, decode) in float_cases {
        let mut encoded = Vec::new();
        encode(values, &mut encoded);
        let mut out = Vec::with_capacity(values.len());
        decode(&encoded, values.len(), &mut out).unwrap();
        let same = out
            .iter()
            .zip(values)
            .all(|(a, b)| a.to_bits() == b.to_bits());
        assert!(same, "{name} roundtrip broke on the bench corpus");
        let mut iters = 0u32;
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < 1.0 {
            out.clear();
            decode(&encoded, values.len(), &mut out).unwrap();
            iters += 1;
        }
        let secs = start.elapsed().as_secs_f64();
        let gbps = (values.len() as f64) * 8.0 * f64::from(iters) / secs / 1e9;
        let ratio = (values.len() as f64) * 8.0 / (encoded.len() as f64);
        println!("{name}: {gbps:.2} GB/s decode, {ratio:.1}x vs raw, {iters} iters");
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
