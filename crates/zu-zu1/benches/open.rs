//! B7 open-latency bench: a 10 GB zu1 file must open in under 10 ms.
//!
//! Open is specified as O(1) I/O (docs/04 §1, G8): read 12 KiB of
//! headers, pick the valid one with the highest epoch, load the free
//! list, and page everything else lazily. This bench pins that down
//! against a real multi-gigabyte file so an accidentally eager open
//! (walking the catalog, a directory, or a segment) fails the gate
//! instead of shipping.
//!
//! The file is built once into ZU_DATA and reused across runs: the
//! soc-LiveJournal1 edge list is bulk loaded repeatedly under distinct
//! rel table names (the catalog makes one file hold many graphs) until
//! the file crosses the target size, 10 GB by default. A file that fails
//! to open, for example after a format version bump, is rebuilt. Set
//! ZU_B7=1 to run at all; the build needs the target size in free disk,
//! so the fleet script enables it only where the disk allows. ZU_B7_GB
//! overrides the target for local smoke runs; the gate only applies at
//! the full 10 GB on real data.
//!
//! Run: ZU_GATE=1 ZU_DATA=~/data/zu ZU_B7=1 cargo bench -p zu-zu1 --bench open

use std::hint::black_box;
use std::time::Instant;

use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{GraphReader, bulk_load_as, read_edge_list};

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

fn synthetic() -> Vec<(u32, u32)> {
    let n = 1u64 << 20;
    let mut rng = 0x2545F4914F6CDD1Du64;
    (0..16_000_000)
        .map(|_| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let s = ((rng >> 32) * (rng >> 32)) >> 44;
            let d = ((rng & 0xFFFF_FFFF) * (rng & 0xFFFF_FFFF)) >> 44;
            ((s % n) as u32, (d % n) as u32)
        })
        .collect()
}

/// The cached file is usable when it exists, meets the size target, and
/// its first table still opens, which catches format version bumps.
fn usable(path: &std::path::Path, target_bytes: u64) -> bool {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) < target_bytes {
        return false;
    }
    let Ok(mut db) = Zu1File::open(path) else {
        return false;
    };
    GraphReader::load_table(&mut db, "edge0").is_ok()
}

fn build(path: &std::path::Path, target_bytes: u64, real: Option<Vec<(u32, u32)>>) {
    let _ = std::fs::remove_file(path);
    let start = Instant::now();
    let mut edges = real.unwrap_or_else(synthetic);
    edges.sort_unstable();
    edges.dedup();
    let node_count = edges
        .iter()
        .map(|&(s, d)| u64::from(s.max(d)) + 1)
        .max()
        .unwrap_or(0);
    let mut db = Zu1File::create(path).expect("create");
    let mut tables = 0u32;
    while std::fs::metadata(path).expect("metadata").len() < target_bytes {
        bulk_load_as(
            &mut db,
            "node",
            &format!("edge{tables}"),
            node_count,
            &edges,
        )
        .expect("bulk_load_as");
        tables += 1;
    }
    println!(
        "b7 build: {tables} rel tables, {} bytes, {:.0}s (cached for later runs)",
        std::fs::metadata(path).expect("metadata").len(),
        start.elapsed().as_secs_f64()
    );
}

fn main() {
    if !std::env::var("ZU_B7").is_ok_and(|v| v == "1") {
        println!("b7: skipped (set ZU_B7=1, builds a 10 GB file under ZU_DATA)");
        return;
    }
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let target_gb: u64 = std::env::var("ZU_B7_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let target_bytes = target_gb << 30;
    let data_dir = std::env::var("ZU_DATA").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("zu-b7")
            .to_string_lossy()
            .into_owned()
    });
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let lj = format!("{data_dir}/soc-LiveJournal1.txt");
    let real_input = std::path::Path::new(&lj).exists();
    let path = std::path::PathBuf::from(format!("{data_dir}/b7.zu1"));

    if !usable(&path, target_bytes) {
        let real = real_input.then(|| {
            let edges = read_edge_list(std::path::Path::new(&lj)).expect("read edge list");
            println!("data: soc-LiveJournal1.txt, {} edges", edges.len());
            edges
        });
        if real.is_none() {
            println!("data: synthetic 16M edges over 1M nodes, set ZU_DATA for real input");
        }
        build(&path, target_bytes, real);
    }

    let file_bytes = std::fs::metadata(&path).expect("metadata").len();
    let tables = {
        let mut db = Zu1File::open(&path).expect("open");
        Catalog::load(&mut db).expect("catalog").rel_tables().len()
    };

    let opens = 1000usize;
    let mut lat = Vec::with_capacity(opens);
    for _ in 0..opens {
        let t = Instant::now();
        let db = Zu1File::open(&path).expect("open");
        black_box(db.db_header().epoch);
        lat.push(t.elapsed());
    }
    lat.sort_unstable();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
    let p99_ms = ms(lat[opens * 99 / 100]);
    println!(
        "open: {file_bytes} bytes, {tables} rel tables, p50 {:.3} ms, p99 {p99_ms:.3} ms, max {:.3} ms ({opens} opens)",
        ms(lat[opens / 2]),
        ms(lat[opens - 1])
    );

    // Informational: open plus the catalog, index, and one directory
    // chain, the work a first query actually waits on. Not gated; it
    // scales with the table's group count, not the file.
    let loads = 100usize;
    let mut lat = Vec::with_capacity(loads);
    for _ in 0..loads {
        let t = Instant::now();
        let mut db = Zu1File::open(&path).expect("open");
        black_box(GraphReader::load_table(&mut db, "edge0").expect("load table"));
        lat.push(t.elapsed());
    }
    lat.sort_unstable();
    println!(
        "open+load: p50 {:.3} ms, p99 {:.3} ms ({loads} loads of one table)",
        ms(lat[loads / 2]),
        ms(lat[loads * 99 / 100])
    );

    // B7 is defined on a 10 GB file of real data; a smaller local smoke
    // target measures but does not gate.
    let mut failed = false;
    if real_input
        && target_gb >= 10
        && let Some(ceiling) = budget("open_p99_ms")
        && p99_ms > ceiling
    {
        println!("GATE FAIL open: p99 {p99_ms:.3} ms > ceiling {ceiling}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    if gate {
        println!("gate: all floors met");
    }
}
