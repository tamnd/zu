//! P3 join gate (Spec/2064g/perf/05 section 2, zu#76): the unchained
//! join table, built once and probed by every worker, against the shapes
//! a graph join actually produces.
//!
//! The old engine's join is `HashSet<(u64, u64)>` swept out of the whole
//! rel table by every worker, so the baseline here is the standard
//! library hash set and map on the same keys. That is not a strawman,
//! it is what `exec.rs` does today.
//!
//! Three probe shapes, because they stress different parts of the
//! design. The distinct probe is one build row per key, which is the
//! relational case and where an open-addressed table is at its best. The
//! duplicate probe is forty build rows per key, which is what a join on
//! a graph key looks like once a hub is in the data, and it is where the
//! contiguous bucket range is supposed to pull away from a chain walk.
//! The miss probe never matches at all, which is the antijoin and the
//! only shape where the folded Bloom tag is the whole story: a miss is
//! meant to cost one directory word and no second load.
//!
//! Every timed run is crosschecked against a reference computed by
//! scanning the build side, so a table that drops a duplicate or lets a
//! tag lie fails here rather than printing a fast number.
//!
//! exec_join_probe_mrows_s floors the distinct probe, which is the
//! shape with the least room to hide, and exec_join_dup_mpairs_s floors
//! the duplicate one, which is the shape a graph join produces.
//!
//! Run: ZU_GATE=1 cargo bench -p zu-exec --bench join

use std::collections::{HashMap, HashSet};
use std::hint::black_box;
use std::time::Instant;

use zu_exec::join::JoinTable;

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

const BUILD: u64 = 1_000_000;
const PROBE: u64 = 4_000_000;
const DUPS: u64 = 40;

/// Keys are strided by a large odd number so consecutive build rows land
/// in unrelated buckets and the probe order tells the table nothing.
fn key_of(i: u64) -> u64 {
    i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 16
}

/// The p50 of `runs` timings of `f`, in milliseconds.
fn measure(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut ms: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ms.sort_by(f64::total_cmp);
    ms[ms.len() / 2]
}

fn main() {
    // The distinct build side: one row a key, payload is the row.
    let dkeys: Vec<u64> = (0..BUILD).map(key_of).collect();
    let dpay: Vec<u64> = (0..BUILD).collect();

    // The duplicate build side: the same total row count, but forty
    // rows to a key, so there are forty times fewer distinct keys.
    let distinct = BUILD / DUPS;
    let hkeys: Vec<u64> = (0..BUILD).map(|i| key_of(i % distinct)).collect();
    let hpay: Vec<u64> = (0..BUILD).collect();

    let build_ms = measure(5, || {
        black_box(JoinTable::build(&dkeys, &dpay));
    });
    let dtable = JoinTable::build(&dkeys, &dpay);
    let htable = JoinTable::build(&hkeys, &hpay);
    println!(
        "join: build {BUILD} rows in {build_ms:.2} ms, {:.0} M rows/s",
        BUILD as f64 / 1e3 / build_ms
    );
    // The two tables hold the same rows and differ only in how many
    // distinct keys those rows carry, so the gap between them is what
    // the table costs for storing a key it has already stored.
    println!(
        "join: distinct table {:.1} MB over {} keys, duplicate table {:.1} MB over {} keys",
        dtable.bytes() as f64 / 1e6,
        dtable.distinct(),
        htable.bytes() as f64 / 1e6,
        htable.distinct()
    );

    // Probe keys. Half of the distinct probe hits, because the build
    // side only covers the first BUILD strides.
    let hit: Vec<u64> = (0..PROBE).map(|i| key_of(i % BUILD)).collect();
    let dup: Vec<u64> = (0..PROBE).map(|i| key_of(i % distinct)).collect();
    // key_of shifts sixteen bits off the top, so no build key ever has
    // a bit up there and none of these can match.
    let miss: Vec<u64> = (0..PROBE).map(|i| key_of(i) | 1 << 60).collect();

    // References, computed the slow obvious way once.
    let dset: HashSet<u64> = dkeys.iter().copied().collect();
    let want_hit = hit.iter().filter(|k| dset.contains(k)).count();
    let want_miss = miss.iter().filter(|k| dset.contains(k)).count();
    let mut hmap: HashMap<u64, Vec<u64>> = HashMap::new();
    for (k, p) in hkeys.iter().zip(&hpay) {
        hmap.entry(*k).or_default().push(*p);
    }
    let want_dup: usize = dup.iter().map(|k| hmap.get(k).map_or(0, Vec::len)).sum();

    let mut marks = vec![false; hit.len()];

    let hit_ms = measure(9, || {
        dtable.mark(&hit, &mut marks);
    });
    let got = marks.iter().filter(|m| **m).count();
    assert_eq!(got, want_hit, "distinct probe lost matches");
    println!(
        "join: distinct probe {:.2} ms, {:.0} M rows/s, {got} of {PROBE} hit, crosschecked",
        hit_ms,
        PROBE as f64 / 1e3 / hit_ms
    );

    let miss_ms = measure(9, || {
        dtable.mark(&miss, &mut marks);
    });
    let got = marks.iter().filter(|m| **m).count();
    assert_eq!(got, want_miss, "miss probe invented matches");
    println!(
        "join: miss probe {:.2} ms, {:.0} M rows/s, {got} of {PROBE} hit, crosschecked",
        miss_ms,
        PROBE as f64 / 1e3 / miss_ms
    );

    // Both sides of the duplicate comparison sum the payloads they
    // find, so both actually walk every matched build row. Counting the
    // length of a match list on one side and emitting every pair on the
    // other would measure two different questions.
    let mut pairs = 0usize;
    let mut sum = 0u64;
    let dup_ms = measure(5, || {
        pairs = 0;
        sum = 0;
        for k in &dup {
            let rows = htable.lookup(*k);
            pairs += rows.len();
            for r in rows {
                sum = sum.wrapping_add(*r);
            }
        }
    });
    assert_eq!(pairs, want_dup, "duplicate probe lost pairs");
    let want_sum = sum;
    println!(
        "join: duplicate probe {:.2} ms, {pairs} pairs, {:.0} M pairs/s, crosschecked",
        dup_ms,
        pairs as f64 / 1e3 / dup_ms
    );

    // The same join through the emit callback an executor would use,
    // to price the callback itself rather than the table.
    let mut cb_pairs = 0usize;
    let cb_ms = measure(5, || {
        cb_pairs = 0;
        let mut s = 0u64;
        htable.join(&dup, |_, row| {
            cb_pairs += 1;
            s = s.wrapping_add(row);
        });
        black_box(s);
    });
    assert_eq!(cb_pairs, want_dup, "callback join lost pairs");
    println!(
        "join: duplicate probe through the emit callback {cb_ms:.2} ms ({:.1}x the slice walk)",
        cb_ms / dup_ms
    );

    // The baselines, the shapes exec.rs uses today. Three runs, they
    // are slow enough that the spread does not matter.
    let set_build_ms = measure(3, || {
        black_box(dkeys.iter().copied().collect::<HashSet<u64>>());
    });
    let set_ms = measure(3, || {
        let mut n = 0usize;
        for k in &hit {
            n += usize::from(dset.contains(k));
        }
        black_box(n);
    });
    let set_miss_ms = measure(3, || {
        let mut n = 0usize;
        for k in &miss {
            n += usize::from(dset.contains(k));
        }
        black_box(n);
    });
    let mut map_sum = 0u64;
    let map_ms = measure(3, || {
        map_sum = 0;
        for k in &dup {
            if let Some(rows) = hmap.get(k) {
                for r in rows {
                    map_sum = map_sum.wrapping_add(*r);
                }
            }
        }
    });
    assert_eq!(map_sum, want_sum, "the two duplicate probes disagree");
    println!(
        "join: std HashSet build {set_build_ms:.2} ms ({:.1}x), distinct probe {set_ms:.2} ms ({:.1}x), miss probe {set_miss_ms:.2} ms ({:.1}x)",
        set_build_ms / build_ms,
        set_ms / hit_ms,
        set_miss_ms / miss_ms
    );
    println!(
        "join: std HashMap duplicate probe {map_ms:.2} ms ({:.1}x)",
        map_ms / dup_ms
    );

    if std::env::var("ZU_GATE").as_deref() == Ok("1") {
        if let Some(floor) = budget("exec_join_probe_mrows_s") {
            let rate = PROBE as f64 / 1e3 / hit_ms;
            assert!(
                rate >= floor,
                "distinct probe at {rate:.0} M rows/s under the {floor} M floor"
            );
            println!("gate: join probe floor met");
        }
        if let Some(floor) = budget("exec_join_dup_mpairs_s") {
            let rate = pairs as f64 / 1e3 / dup_ms;
            assert!(
                rate >= floor,
                "duplicate probe at {rate:.0} M pairs/s under the {floor} M floor"
            );
            println!("gate: join duplicate floor met");
        }
    }
}
