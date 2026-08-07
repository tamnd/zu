//! FullZip string segment bench and the M1 gate for the structural
//! layout.
//!
//! Uses real data when available: the raw lines of soc-LiveJournal1.txt
//! (set ZU_DATA to the directory holding it), each line one row, which
//! is what a STRING column bulk load sees. Otherwise synthetic url rows.
//! Measures the writer in M rows/s, the on-disk ratio in payload bytes
//! per value byte, the full scan in GB/s of value bytes, and random
//! single-row point reads in K rows/s, with every number crosschecked
//! against the source rows. With ZU_GATE=1 the process exits nonzero
//! when a floor or ceiling in bench/budgets.toml is missed.
//!
//! Run: ZU_GATE=1 ZU_DATA=~/data/zu cargo bench -p zu-zu1 --bench blob

use std::io::BufRead;
use std::time::Instant;

use zu_zu1::file::Zu1File;
use zu_zu1::fullzip;

/// Rows per segment: one node group.
const GROUP_ROWS: usize = 1 << 17;
const REAL_ROWS: usize = 4_000_000;
const SYNTHETIC_ROWS: usize = 1_000_000;

/// Rows packed end to end so 4 M of them cost two flat buffers, not 4 M
/// allocations.
struct Corpus {
    buf: Vec<u8>,
    ends: Vec<u64>,
}

impl Corpus {
    fn row(&self, i: usize) -> &[u8] {
        let lo = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.buf[lo..self.ends[i] as usize]
    }

    fn len(&self) -> usize {
        self.ends.len()
    }
}

fn load_real(dir: &str) -> Option<Corpus> {
    let path = format!("{dir}/soc-LiveJournal1.txt");
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
    let mut corpus = Corpus {
        buf: Vec::new(),
        ends: Vec::new(),
    };
    let mut line = String::new();
    while corpus.ends.len() < REAL_ROWS {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        corpus.buf.extend_from_slice(trimmed.as_bytes());
        corpus.ends.push(corpus.buf.len() as u64);
    }
    println!(
        "data: soc-LiveJournal1.txt, {} rows, {} bytes",
        corpus.len(),
        corpus.buf.len()
    );
    Some(corpus)
}

fn synthetic() -> Corpus {
    let mut corpus = Corpus {
        buf: Vec::new(),
        ends: Vec::new(),
    };
    for i in 0..SYNTHETIC_ROWS {
        corpus
            .buf
            .extend_from_slice(format!("https://example.com/user/{}/posts?page={}", i * 37, i % 12).as_bytes());
        corpus.ends.push(corpus.buf.len() as u64);
    }
    println!("data: synthetic {SYNTHETIC_ROWS} url rows, set ZU_DATA for real input");
    corpus
}

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

fn main() {
    let real = std::env::var("ZU_DATA").ok().and_then(|d| load_real(&d));
    let is_real = real.is_some();
    let corpus = real.unwrap_or_else(synthetic);
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let n = corpus.len();
    let raw = corpus.buf.len();

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("blob.zu1");
    let mut db = Zu1File::create(&path).expect("create");
    let mut metas = Vec::new();
    let mut refs: Vec<&[u8]> = Vec::with_capacity(GROUP_ROWS);
    let start = Instant::now();
    for base in (0..n).step_by(GROUP_ROWS) {
        refs.clear();
        for i in base..(base + GROUP_ROWS).min(n) {
            refs.push(corpus.row(i));
        }
        metas.push(fullzip::write_blob_segment(&mut db, &refs).expect("write segment"));
    }
    let secs = start.elapsed().as_secs_f64();
    let payload: u64 = metas.iter().map(|m| m.payload_len).sum();
    let ratio = payload as f64 / raw as f64;
    println!(
        "fullzip write: {:.2} M rows/s, ratio {ratio:.3} payload/value bytes ({n} rows, {raw} raw bytes, {} segments, {secs:.2}s)",
        n as f64 / secs / 1e6,
        metas.len()
    );

    // Correctness pass, untimed: every segment must hand back its rows
    // byte for byte before any throughput number means anything.
    let (mut bytes, mut ends) = (Vec::new(), Vec::new());
    for (gi, meta) in metas.iter().enumerate() {
        bytes.clear();
        ends.clear();
        fullzip::read_blob_segment(&mut db, meta, &mut bytes, &mut ends).expect("scan");
        let lo = if gi == 0 {
            0
        } else {
            corpus.ends[gi * GROUP_ROWS - 1] as usize
        };
        let hi = corpus.ends[((gi + 1) * GROUP_ROWS).min(n) - 1] as usize;
        assert_eq!(bytes, corpus.buf[lo..hi], "segment {gi} disagrees with the source");
    }
    println!("fullzip scan crosscheck: {} segments match the source", metas.len());

    let mut best = f64::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let mut total = 0usize;
        for meta in &metas {
            bytes.clear();
            ends.clear();
            fullzip::read_blob_segment(&mut db, meta, &mut bytes, &mut ends).expect("scan");
            total += bytes.len();
        }
        assert_eq!(total, raw);
        best = best.min(t.elapsed().as_secs_f64());
    }
    let scan_gbs = raw as f64 / best / 1e9;
    println!("fullzip scan: {scan_gbs:.2} GB/s value bytes, best of 3");

    let reads = 200_000usize;
    let mut rng = 0x9E3779B97F4A7C15u64;
    let mut lat = Vec::with_capacity(reads);
    let mut out = Vec::new();
    let started = Instant::now();
    for _ in 0..reads {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let row = (rng % n as u64) as usize;
        out.clear();
        let t = Instant::now();
        fullzip::read_blob_row(&mut db, &metas[row / GROUP_ROWS], (row % GROUP_ROWS) as u64, &mut out)
            .expect("row read");
        lat.push(t.elapsed());
        assert_eq!(out, corpus.row(row), "row {row} disagrees with the source");
    }
    let secs = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let kreads_s = reads as f64 / secs / 1e3;
    println!(
        "fullzip point: {kreads_s:.0} K rows/s, p50 {:.1} us, p99 {:.1} us",
        lat[reads / 2].as_secs_f64() * 1e6,
        lat[reads * 99 / 100].as_secs_f64() * 1e6
    );

    let mut failed = false;
    if let Some(floor) = budget("fullzip_scan_gbs")
        && scan_gbs < floor
    {
        println!("GATE FAIL fullzip scan: {scan_gbs:.2} GB/s < floor {floor}");
        failed = true;
    }
    if let Some(floor) = budget("fullzip_row_kreads_s")
        && kreads_s < floor
    {
        println!("GATE FAIL fullzip point: {kreads_s:.0} K rows/s < floor {floor}");
        failed = true;
    }
    // The ratio ceiling is defined against the real LiveJournal lines;
    // FSST training is deterministic, so the number is the same on every
    // machine that reads the same file.
    if is_real
        && let Some(ceiling) = budget("fullzip_ratio")
        && ratio > ceiling
    {
        println!("GATE FAIL fullzip ratio: {ratio:.3} > ceiling {ceiling}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    if gate {
        println!("gate: all floors met");
    }
}
