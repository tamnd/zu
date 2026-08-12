//! P3 batch point read gate (docs/07, zu#76): a leading UNWIND over a
//! list of keys is a page of point reads, and the pipeline now takes
//! the whole shape instead of handing it back to the old row at a time
//! engine.
//!
//! The table is two million people, each knowing two others picked far
//! apart in row order, and the batch is fifty thousand keys strided
//! over the table so the lookups land nowhere near each other. That is
//! the serving shape: a client holds a page of ids and wants the rows,
//! or the neighborhoods, behind them.
//!
//! Five queries run over it. The read is the batch and nothing else, so
//! it is the key index and the gather, and that is the shape the floor
//! sits on. The hop walks one edge off every row it found, the count
//! puts a bare count over the same walk, the next one counts per key,
//! which is the shape that has to carry the key itself out of the
//! source and into the group table, and the last counts two hops out,
//! which is the one that never builds a level and reads degrees off
//! rows scattered over every group in the table.
//!
//! The old engine cannot run the same batch. Its index lookup only
//! fires on a key expression that names no variable, so a key that came
//! out of an UNWIND leaves it scanning the whole table once per
//! element, which is hours at this batch size. It runs a slice of the
//! batch instead and both sides are reported per key, so the ratio is
//! honest and its growth with the table is the point rather than a
//! surprise.
//!
//! The other reference is what a client does today: one prepared point
//! query per key, run in a loop. That is the same total work with the
//! per query cost paid fifty thousand times.
//!
//! Every run is crosschecked against the generator, rows and the sum of
//! the last column, so a batch that loses a key or pairs one with the
//! wrong row fails here rather than printing a fast number.
//!
//! Everything runs at one worker, so the rate is per core and the
//! fleet's core counts stay out of the number.
//!
//! exec_batch_mkeys_s_core floors the batch read in millions of keys a
//! second.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench batch

use std::time::Instant;

use zu::query::{self, Value};
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::zu1::props::{PropValues, store_props};

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

const NODES: u64 = 2_000_000;
const KEYS: u64 = 50_000;
/// How many keys the old engine gets. It scans the table once per key,
/// so this is the largest slice that still leaves the bench a bench.
const OLD_KEYS: usize = 8;
/// Coprime with the table size, so the batch is fifty thousand
/// different keys and a group by on one of them has a group per key.
const STRIDE: u64 = 39_916_801;

fn score_of(i: u64) -> i64 {
    ((i * 7919) % 100_000) as i64
}

fn keys() -> Vec<u64> {
    (0..KEYS).map(|j| (j * STRIDE) % NODES).collect()
}

/// Two friends each, picked far apart in row order so the walk does not
/// read one neighborhood over and over.
fn edges() -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(NODES as usize * 2);
    for i in 0..NODES {
        out.push((i as u32, ((i * 7919 + 13) % NODES) as u32));
        out.push((i as u32, ((i * 104_729 + 7) % NODES) as u32));
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn build(path: &std::path::Path, edges: &[(u32, u32)]) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, edges).expect("load");
    let score: Vec<u64> = (0..NODES).map(|i| score_of(i) as u64).collect();
    store_props(&mut db, "person", &[("score", PropValues::Int(&score))]).expect("props");
}

/// What the generator says a query has to answer over one batch of
/// keys: rows out and the sum of the last column, which is the friend
/// ids on the walking shapes and the counts on the counting ones. The
/// sum is what catches a batch that pairs a key with the wrong row.
struct Want {
    rows: u64,
    total: i64,
}

/// The edge list read back the way the queries read it: where each
/// row's neighbors start, how many it has, and what they add up to.
struct Adj {
    edges: Vec<(u32, u32)>,
    starts: Vec<u32>,
    deg: Vec<u32>,
    sum: Vec<i64>,
}

impl Adj {
    fn of(edges: Vec<(u32, u32)>) -> Self {
        let mut deg = vec![0u32; NODES as usize];
        let mut sum = vec![0i64; NODES as usize];
        for &(src, dst) in &edges {
            deg[src as usize] += 1;
            sum[src as usize] += i64::from(dst);
        }
        let mut starts = Vec::with_capacity(NODES as usize + 1);
        let mut at = 0u32;
        for &d in &deg {
            starts.push(at);
            at += d;
        }
        starts.push(at);
        Self {
            edges,
            starts,
            deg,
            sum,
        }
    }

    fn friends(&self, row: u64) -> &[(u32, u32)] {
        let (lo, hi) = (self.starts[row as usize], self.starts[row as usize + 1]);
        &self.edges[lo as usize..hi as usize]
    }
}

/// The five answers, in the order the cases run, for whatever slice of
/// the batch is handed over.
fn wants(keys: &[u64], adj: &Adj) -> [Want; 5] {
    let hops: u64 = keys.iter().map(|&k| u64::from(adj.deg[k as usize])).sum();
    let hops2: u64 = keys
        .iter()
        .flat_map(|&k| adj.friends(k))
        .map(|&(_, f)| u64::from(adj.deg[f as usize]))
        .sum();
    [
        Want {
            rows: keys.len() as u64,
            total: keys.iter().map(|&k| score_of(k)).sum(),
        },
        Want {
            rows: hops,
            total: keys.iter().map(|&k| adj.sum[k as usize]).sum(),
        },
        Want {
            rows: 1,
            total: hops as i64,
        },
        Want {
            rows: keys.len() as u64,
            total: hops as i64,
        },
        Want {
            rows: 1,
            total: hops2 as i64,
        },
    ]
}

fn check(r: &query::QueryResult, want: &Want, source: &str) {
    assert_eq!(r.rows.len() as u64, want.rows, "rows for {source}");
    let mut total = 0i64;
    for row in &r.rows {
        let Value::Int(n) = row[row.len() - 1] else {
            panic!("expected an int in the last column of {source}");
        };
        total += n;
    }
    assert_eq!(total, want.total, "column total for {source}");
}

/// Median ms of `source` over one batch, with the answer checked on
/// every run.
fn measure(db: &mut Zu1File, source: &str, ids: &[Value], want: &Want, runs: usize) -> f64 {
    let params = [("ids", Value::List(ids.to_vec()))];
    let warm = query::run(source, db, &params).expect("warmup");
    check(&warm, want, source);
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &params).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            check(&r, want, source);
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

/// Total ms of one point query per key, the way a client without a
/// batch shape reads a page of ids. The answer is checked the same way,
/// summed over the loop.
fn one_by_one(db: &mut Zu1File, source: &str, keys: &[u64], want: &Want) -> f64 {
    let t = Instant::now();
    let (mut rows, mut total) = (0u64, 0i64);
    for &key in keys {
        let r = query::run(source, db, &[("id", Value::Int(key as i64))]).expect("point read");
        rows += r.rows.len() as u64;
        for row in &r.rows {
            let Value::Int(n) = row[row.len() - 1] else {
                panic!("expected an int in the last column of {source}");
            };
            total += n;
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(rows, want.rows, "rows for the loop over {source}");
    assert_eq!(total, want.total, "column total for the loop over {source}");
    ms
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("batch.zu1");
    let t = Instant::now();
    let edges = edges();
    build(&path, &edges);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "batch: {NODES} persons, {} edges, {KEYS} keys, load {:.1}s",
        edges.len(),
        t.elapsed().as_secs_f64()
    );

    let keys = keys();
    let ids: Vec<Value> = keys.iter().map(|&k| Value::Int(k as i64)).collect();
    let adj = Adj::of(edges);
    let full = wants(&keys, &adj);
    let small = wants(&keys[..OLD_KEYS], &adj);
    println!("batch: {} rows off the hop", full[1].rows);

    let cases: [(&str, &str); 5] = [
        (
            "read",
            "UNWIND $ids AS id MATCH (p:person {id: id}) RETURN id AS i, p.score AS score",
        ),
        (
            "hop",
            "UNWIND $ids AS id MATCH (p:person {id: id})-[:knows]->(f) RETURN id AS i, f.id AS f",
        ),
        (
            "counted hop",
            "UNWIND $ids AS id MATCH (p:person {id: id})-[:knows]->(f) RETURN count(f) AS n",
        ),
        (
            "counted per key",
            "UNWIND $ids AS id MATCH (p:person {id: id})-[:knows]->(f) \
             RETURN id AS i, count(f) AS n",
        ),
        (
            "counted two hop",
            "UNWIND $ids AS id MATCH (p:person {id: id})-[:knows]->(f)-[:knows]->(g) \
             RETURN count(g) AS n",
        ),
    ];

    let mut read_mkeys = 0.0;
    for (i, (what, source)) in cases.into_iter().enumerate() {
        let new = measure(&mut db, source, &ids, &full[i], 9);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, source, &ids[..OLD_KEYS], &small[i], 3);
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mkeys = KEYS as f64 / 1e3 / new;
        let us = new * 1e3 / KEYS as f64;
        let old_us = old * 1e3 / OLD_KEYS as f64;
        println!(
            "batch {what}: {new:.1} ms {mkeys:.1} M keys/s {us:.2} us/key, old engine {old_us:.0} \
             us/key over {OLD_KEYS} keys, {:.0}x per key, crosschecked",
            old_us / us
        );
        if what == "read" {
            read_mkeys = mkeys;
        }
    }

    // The same page of ids read the way a client reads one today, one
    // prepared point query at a time. Both sides run on the pipeline
    // here, so the gap is the per query cost and nothing else.
    let loop_ms = one_by_one(
        &mut db,
        "MATCH (p:person {id: $id}) RETURN p.id AS i, p.score AS score",
        &keys,
        &full[0],
    );
    println!(
        "batch read one query per key: {loop_ms:.1} ms {:.2} us/key",
        loop_ms * 1e3 / KEYS as f64
    );

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_batch_mkeys_s_core")
    {
        assert!(
            read_mkeys >= floor,
            "batch read at {read_mkeys:.1} M keys/s under the {floor} M floor"
        );
        println!("gate: batch floor met");
    }
}
