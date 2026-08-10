//! P1 read path bench and gates (Spec/2064g/perf/04, zu#74): warm CSR
//! pins, pool thrash, zone-skipped scans, and batched gathers.
//!
//! Four measurements, each crosschecked against arithmetic references
//! before any number prints. hop is the warm 1-hop through a pooled
//! group pin, the read the vectorized Expand makes: pin the group,
//! borrow the list, sum it. thrash alternates every group round robin,
//! so the reader's one cached slot misses each time and the decoded
//! pool serves the revisit; the speedup is that pooled read against
//! decoding the same segment fresh, which is what every group revisit
//! cost before the pools. scan drives 100 M sorted i64 through
//! chunk decodes, once flat with a sum and once under a 1 percent
//! range predicate where the chunk zone maps skip everything else;
//! the filtered number is effective GB/s of logical value bytes, and
//! the bench asserts the skip actually happened by counting decoded
//! chunks. gather reads random row batches through the props gather,
//! one decode per touched chunk.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-zu1 --bench read

use std::time::Instant;

use zu_zu1::file::Zu1File;
use zu_zu1::graph::{Direction, GraphReader, bulk_load_as};
use zu_zu1::props::{PropValues, PropsReader, load_props, store_props};
use zu_zu1::segment::{
    CHUNK_ROWS, chunk_zone, decode_chunk, load_chunk_directory, read_segment, write_segment,
};

const GROUP_ROWS: usize = 1 << 17;
/// Groups in the hop graph, sized so both directions of the CSR fit
/// the decoded pools and the thrash measures the pools, not eviction.
const GROUPS: usize = 4;
const NODES: usize = GROUPS * GROUP_ROWS;
const DEGREE: usize = 4;
/// Values per scan segment, and enough segments to pass 100 M total.
const SEG_VALUES: usize = 4_800_000;
const SEGMENTS: usize = 21;

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

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The scan corpus value at `i`: ascending with a small varying delta,
/// so delta bitpack gets a realistic width instead of a constant run.
fn scan_value(i: u64) -> u64 {
    i * 13 + i % 7
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    let mut failed = false;

    // ---- hop graph: 4 groups, degree 4, deterministic edges ----
    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = Zu1File::create(&dir.path().join("read.zu1")).expect("create");
    let mut edges: Vec<(u32, u32)> = Vec::with_capacity(NODES * DEGREE);
    for src in 0..NODES as u32 {
        for k in 0..DEGREE as u32 {
            let dst = (u64::from(src)
                .wrapping_mul(6364136223846793005)
                .wrapping_add(u64::from(k) * 1442695040888963407)
                % NODES as u64) as u32;
            edges.push((src, dst));
        }
    }
    edges.sort_unstable();
    edges.dedup();
    let edge_count = edges.len();
    let t = Instant::now();
    bulk_load_as(&mut db, "person", "knows", NODES as u64, &edges).expect("bulk load");
    println!(
        "load: {NODES} nodes, {edge_count} edges, {GROUPS} groups, {:.1}s",
        t.elapsed().as_secs_f64()
    );
    drop(edges);
    let reader = GraphReader::load_table(&mut db, "knows").expect("load table");

    // Correctness pass, untimed: every node's degree sums to the edge
    // count, through the pooled offsets.
    let mut total = 0u64;
    for g in 0..GROUPS {
        let (offs, nbrs) = reader
            .csr_group(&mut db, g, Direction::Fwd)
            .expect("csr group");
        assert_eq!(offs.len(), GROUP_ROWS + 1, "group {g} offsets");
        total += *offs.last().unwrap();
        assert_eq!(nbrs.len() as u64, *offs.last().unwrap(), "group {g} csr");
    }
    assert_eq!(total, edge_count as u64, "csr crosscheck");
    println!("csr crosscheck: {edge_count} edges through pooled pins");

    // ---- warm 1-hop p50 through the pooled pin ----
    let batch = 256usize;
    let batches = 2000usize;
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut per_op = Vec::with_capacity(batches);
    let mut sum = 0u64;
    for _ in 0..batches {
        let t = Instant::now();
        for _ in 0..batch {
            let node = rng.next() % NODES as u64;
            let g = (node / GROUP_ROWS as u64) as usize;
            let local = (node % GROUP_ROWS as u64) as usize;
            let (offs, nbrs) = reader
                .csr_group(&mut db, g, Direction::Fwd)
                .expect("warm pin");
            for &n in &nbrs[offs[local] as usize..offs[local + 1] as usize] {
                sum = sum.wrapping_add(n);
            }
        }
        per_op.push(t.elapsed().as_secs_f64() / batch as f64);
    }
    std::hint::black_box(sum);
    per_op.sort_unstable_by(f64::total_cmp);
    let hop_p50_us = per_op[batches / 2] * 1e6;
    let hop_p99_us = per_op[batches * 99 / 100] * 1e6;
    println!("hop: warm 1-hop p50 {hop_p50_us:.3} us, p99 {hop_p99_us:.3} us, batches of {batch}");

    // ---- thrash: pooled round robin against fresh decodes ----
    let rounds = 50usize;
    let t = Instant::now();
    let mut edge_sum = 0u64;
    for _ in 0..rounds {
        for g in 0..GROUPS {
            let (offs, _nbrs) = reader
                .csr_group(&mut db, g, Direction::Fwd)
                .expect("thrash pin");
            edge_sum += *offs.last().unwrap();
        }
    }
    let pooled_per_group = t.elapsed().as_secs_f64() / (rounds * GROUPS) as f64;
    assert_eq!(edge_sum, edge_count as u64 * rounds as u64, "thrash sum");
    let mut scratch = Vec::new();
    let t = Instant::now();
    let mut decode_rounds = 0usize;
    // Fresh decodes are slow enough that a fixed round count would
    // stall the vCPU hosts; decode for about a second instead.
    let mut edge_sum = 0u64;
    while t.elapsed().as_secs_f64() < 1.0 {
        for g in 0..GROUPS {
            let meta = &reader.directory().groups[g].dir(Direction::Fwd).neighbors;
            scratch.clear();
            read_segment(&mut db, meta, &mut scratch).expect("fresh decode");
            edge_sum += scratch.len() as u64;
        }
        decode_rounds += 1;
    }
    let fresh_per_group = t.elapsed().as_secs_f64() / (decode_rounds * GROUPS) as f64;
    assert_eq!(
        edge_sum,
        edge_count as u64 * decode_rounds as u64,
        "decode sum"
    );
    let thrash_speedup = fresh_per_group / pooled_per_group;
    println!(
        "thrash: pooled {:.2} us/group, fresh decode {:.0} us/group, speedup {thrash_speedup:.0}x",
        pooled_per_group * 1e6,
        fresh_per_group * 1e6
    );

    // ---- scan: 100 M sorted i64, flat and under a 1 percent range ----
    let scan_path = dir.path().join("scan.zu1");
    let mut sdb = Zu1File::create(&scan_path).expect("create scan");
    let total_values = (SEG_VALUES * SEGMENTS) as u64;
    let mut buf = Vec::with_capacity(SEG_VALUES);
    let mut metas = Vec::with_capacity(SEGMENTS);
    // The predicate range: one percent of the rows, dead center.
    let pred_lo_row = total_values / 2 - total_values / 200;
    let pred_hi_row = total_values / 2 + total_values / 200;
    let (pred_lo, pred_hi) = (scan_value(pred_lo_row), scan_value(pred_hi_row));
    let mut flat_ref = 0u64;
    let mut pred_ref = 0u64;
    let mut pred_rows = 0u64;
    let t = Instant::now();
    for s in 0..SEGMENTS {
        buf.clear();
        for i in (s * SEG_VALUES) as u64..((s + 1) * SEG_VALUES) as u64 {
            let v = scan_value(i);
            buf.push(v);
            flat_ref = flat_ref.wrapping_add(v);
            if v >= pred_lo && v <= pred_hi {
                pred_ref = pred_ref.wrapping_add(v);
                pred_rows += 1;
            }
        }
        metas.push(write_segment(&mut sdb, &buf).expect("write segment"));
    }
    let secs = t.elapsed().as_secs_f64();
    let logical_bytes = total_values * 8;
    let payload: u64 = metas.iter().map(|m| m.payload_len).sum();
    println!(
        "scan corpus: {total_values} values in {SEGMENTS} segments, {:.1} bits/value, write {:.0} M vals/s",
        payload as f64 * 8.0 / total_values as f64,
        total_values as f64 / secs / 1e6
    );
    assert!(metas.iter().all(|m| m.sorted), "corpus must flag sorted");
    let dirs: Vec<_> = metas
        .iter()
        .map(|m| load_chunk_directory(&mut sdb, m).expect("directory"))
        .collect();

    let mut flat_gbs = 0f64;
    for _ in 0..3 {
        let t = Instant::now();
        let mut sum = 0u64;
        for (meta, cdir) in metas.iter().zip(&dirs) {
            let chunks = (meta.value_count as usize).div_ceil(CHUNK_ROWS);
            for c in 0..chunks {
                decode_chunk(&mut sdb, meta, cdir, c, &mut scratch).expect("chunk");
                for &v in &scratch {
                    sum = sum.wrapping_add(v);
                }
            }
        }
        assert_eq!(sum, flat_ref, "flat scan sum");
        flat_gbs = flat_gbs.max(logical_bytes as f64 / t.elapsed().as_secs_f64() / 1e9);
    }
    println!("scan flat: {flat_gbs:.2} GB/s decode+sum, best of 3");

    let mut filtered_gbs = 0f64;
    let mut decoded_chunks = 0usize;
    let mut total_chunks = 0usize;
    for _ in 0..3 {
        let t = Instant::now();
        let mut sum = 0u64;
        let mut rows = 0u64;
        decoded_chunks = 0;
        total_chunks = 0;
        for (meta, cdir) in metas.iter().zip(&dirs) {
            let chunks = (meta.value_count as usize).div_ceil(CHUNK_ROWS);
            total_chunks += chunks;
            if pred_lo > meta.max || pred_hi < meta.min {
                continue;
            }
            for c in 0..chunks {
                if let Some((lo, hi)) = chunk_zone(meta, cdir, c)
                    && (pred_lo > hi || pred_hi < lo)
                {
                    continue;
                }
                decoded_chunks += 1;
                decode_chunk(&mut sdb, meta, cdir, c, &mut scratch).expect("chunk");
                for &v in &scratch {
                    if v >= pred_lo && v <= pred_hi {
                        sum = sum.wrapping_add(v);
                        rows += 1;
                    }
                }
            }
        }
        assert_eq!(sum, pred_ref, "filtered scan sum");
        assert_eq!(rows, pred_rows, "filtered scan rows");
        filtered_gbs = filtered_gbs.max(logical_bytes as f64 / t.elapsed().as_secs_f64() / 1e9);
    }
    // The zone maps must have done the work: about 1 percent of chunks
    // hold the range, and anything past 2.5 percent means the skip
    // logic broke, not that a machine is slow.
    assert!(
        decoded_chunks * 40 < total_chunks,
        "zone skip decoded {decoded_chunks} of {total_chunks} chunks"
    );
    println!(
        "scan filtered: {filtered_gbs:.1} GB/s effective at 1 percent selectivity, {decoded_chunks} of {total_chunks} chunks decoded"
    );

    // ---- gather: random row batches through the props reader ----
    let ints: Vec<u64> = (0..NODES as u64)
        .map(|i| i.wrapping_mul(0x9E37) >> 3)
        .collect();
    store_props(&mut db, "person", &[("v", PropValues::Int(&ints))]).expect("store props");
    let table = zu_zu1::catalog::Catalog::load(&mut db)
        .expect("catalog")
        .node_by_name("person")
        .expect("person")
        .id;
    let mut preader = PropsReader::new(load_props(&mut db, table).expect("props").expect("some"));
    let vcol = preader.col("v").expect("col");
    let batch_rows = 4096usize;
    let gather_batches = 500usize;
    let mut rows_buf = Vec::with_capacity(batch_rows);
    let mut out = Vec::new();
    // Warm pass doubles as the correctness pass.
    rows_buf.extend((0..batch_rows).map(|_| rng.next() % NODES as u64));
    preader
        .gather_int(&mut db, vcol, &rows_buf, &mut out)
        .expect("gather");
    for (i, &r) in rows_buf.iter().enumerate() {
        assert_eq!(out[i], ints[r as usize], "gather row {r}");
    }
    let t = Instant::now();
    let mut sum = 0u64;
    for _ in 0..gather_batches {
        rows_buf.clear();
        rows_buf.extend((0..batch_rows).map(|_| rng.next() % NODES as u64));
        preader
            .gather_int(&mut db, vcol, &rows_buf, &mut out)
            .expect("gather");
        for &v in &out {
            sum = sum.wrapping_add(v);
        }
    }
    std::hint::black_box(sum);
    let gather_mrows_s = (batch_rows * gather_batches) as f64 / t.elapsed().as_secs_f64() / 1e6;
    println!("gather: {gather_mrows_s:.1} M rows/s, batches of {batch_rows} random rows");

    // ---- gates ----
    if let Some(ceiling) = budget("read_hop_p50_us")
        && hop_p50_us > ceiling
    {
        println!("GATE FAIL hop: {hop_p50_us:.3} us > ceiling {ceiling}");
        failed = true;
    }
    if let Some(floor) = budget("read_thrash_speedup")
        && thrash_speedup < floor
    {
        println!("GATE FAIL thrash: {thrash_speedup:.0}x < floor {floor}");
        failed = true;
    }
    if let Some(floor) = budget("read_scan_gbs")
        && flat_gbs < floor
    {
        println!("GATE FAIL scan flat: {flat_gbs:.2} GB/s < floor {floor}");
        failed = true;
    }
    if let Some(floor) = budget("read_scan_filtered_gbs")
        && filtered_gbs < floor
    {
        println!("GATE FAIL scan filtered: {filtered_gbs:.1} GB/s < floor {floor}");
        failed = true;
    }
    if let Some(floor) = budget("read_gather_mrows_s")
        && gather_mrows_s < floor
    {
        println!("GATE FAIL gather: {gather_mrows_s:.1} M rows/s < floor {floor}");
        failed = true;
    }
    if gate && failed {
        std::process::exit(1);
    }
    if gate {
        println!("gate: all floors met");
    }
}
