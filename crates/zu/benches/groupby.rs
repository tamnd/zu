//! P3 grouping gate (Spec/2064g/perf/05 section 4, zu#76): GROUP BY and
//! DISTINCT run through the open-addressing group table, and the spec
//! asks for 100 M rows per second per core on a ten million row scan
//! grouped on an integer key into a hundred thousand groups.
//!
//! The table is ten million people with three columns: `grp` is the
//! hundred thousand group key, `few` is a twelve group key so the same
//! scan can be measured with the whole table in L1, and `name` is a
//! string key over a thousand distinct values so the heap and the byte
//! compare are measured too. There are no edges; this bench is about
//! the sink, and an expand in front of it would only add noise.
//!
//! Every query runs at one worker for the per-core number and again at
//! eight, and the group count and the total row count are crosschecked
//! against the generator on every run, so a table that loses or merges
//! a group fails here rather than showing up as a fast number.
//!
//! exec_group_mrows_s_core gates the hundred thousand group query at
//! one worker and exec_group_str_mrows_s_core gates the string key
//! beside it. Both are whole-query numbers, decode and scan and sink
//! and the final sort of the groups together, because that is what a
//! user waits for.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench groupby

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

const NODES: u64 = 10_000_000;
const GROUPS: u64 = 100_000;
const FEW: u64 = 12;
const NAMES: u64 = 1000;

/// Group keys are strided rather than sequential so the probe order
/// does not walk the slots in order, which is the easy case no real
/// query gets.
fn grp_of(i: u64) -> u64 {
    (i * 7919) % GROUPS
}

fn build(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &[]).expect("load");
    let grp: Vec<u64> = (0..NODES).map(grp_of).collect();
    let few: Vec<u64> = (0..NODES).map(|i| i % FEW).collect();
    let names: Vec<Vec<u8>> = (0..NODES)
        .map(|i| format!("name{}", i % NAMES).into_bytes())
        .collect();
    let name_refs: Vec<&[u8]> = names.iter().map(|v| v.as_slice()).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("grp", PropValues::Int(&grp)),
            ("few", PropValues::Int(&few)),
            ("name", PropValues::Str(&name_refs)),
        ],
    )
    .expect("props");
}

/// Rows returned and the total of the count column, which must be every
/// scanned row whatever the key was.
fn shape(r: &zu::query::QueryResult) -> (usize, i64) {
    let total = r
        .rows
        .iter()
        .map(|row| match row[1] {
            Value::Int(n) => n,
            ref other => panic!("expected a count, got {other:?}"),
        })
        .sum();
    (r.rows.len(), total)
}

/// Median ms of `source` at a fixed worker count, with the group count
/// and row total checked on every run.
fn measure(db: &mut Zu1File, source: &str, threads: usize, groups: usize, runs: usize) -> f64 {
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", threads.to_string()) };
    let warm = query::run(source, db, &[]).expect("warmup");
    assert_eq!(shape(&warm), (groups, NODES as i64), "warmup shape");
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            assert_eq!(shape(&r), (groups, NODES as i64), "answer changed");
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("groupby.zu1");
    let t = Instant::now();
    build(&path);
    let mut db = Zu1File::open(&path).expect("open");
    println!(
        "groupby: {NODES} persons, {GROUPS} groups, load {:.1}s",
        t.elapsed().as_secs_f64()
    );

    let run = |db: &mut Zu1File, source: &str, groups: usize, what: &str| {
        let mut one = 0.0;
        let mut line = format!("groupby {what} ({groups} groups):");
        for threads in [1usize, 8] {
            let ms = measure(db, source, threads, groups, 7);
            let mrows = NODES as f64 / 1e3 / ms;
            line.push_str(&format!(" {threads}t {ms:.1} ms {mrows:.0} M rows/s,"));
            if threads == 1 {
                one = mrows;
            }
        }
        println!("{line} crosschecked");
        one
    };

    let wide_q = "MATCH (p:person) RETURN p.grp AS g, count(p) AS n";
    let wide = run(&mut db, wide_q, GROUPS as usize, "int key");
    run(
        &mut db,
        "MATCH (p:person) RETURN p.few AS g, count(p) AS n",
        FEW as usize,
        "int key",
    );
    let strkey = run(
        &mut db,
        "MATCH (p:person) RETURN p.name AS g, count(p) AS n",
        NAMES as usize,
        "string key",
    );

    // The same query on the old BTreeMap sink, which is what the
    // hashed table replaced. Three runs, it is slow enough that the
    // spread does not matter.
    // SAFETY: same as measure, nothing else is running here.
    unsafe { std::env::set_var("ZU_EXEC2", "0") };
    let old = NODES as f64 / 1e3 / measure(&mut db, wide_q, 1, GROUPS as usize, 3);
    unsafe { std::env::remove_var("ZU_EXEC2") };
    println!(
        "groupby int key old sink: 1t {old:.0} M rows/s, hashed sink {:.1}x faster",
        wide / old
    );

    if std::env::var("ZU_GATE").as_deref() == Ok("1") {
        if let Some(floor) = budget("exec_group_mrows_s_core") {
            assert!(
                wide >= floor,
                "group by at {wide:.0} M rows/s/core under the {floor} M floor"
            );
        }
        if let Some(floor) = budget("exec_group_str_mrows_s_core") {
            assert!(
                strkey >= floor,
                "group by on a string key at {strkey:.0} M rows/s/core under the {floor} M floor"
            );
        }
        println!("gate: grouping floors met");
    }
}
