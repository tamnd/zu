//! P3 EXISTS gate (Spec/2064g/docs/07, zu#76): a block over a bare
//! pattern is answered by the degree of the row it was written on, and
//! one with a predicate in it by a bracket whose group stops at the
//! first match, instead of the whole query going back to the old
//! engine either way.
//!
//! The table is a million people and a knows graph with three kinds of
//! person in it: every fourth is isolated and can only be a miss, one
//! in eight of the rest is a hub with thirty three friends, and the
//! others have two. The hubs are the point of the shape. A block over
//! a bare pattern is a question about degree, and a degree is two
//! offsets whatever the node's list holds, so a hub answers as fast as
//! a leaf; a block with a predicate in it has to walk until one friend
//! passes, and there the hubs are where the walk stops early.
//!
//! Seven queries run over it. The counted semi is the bare block with
//! nothing built above it, so it is the degree read and nothing else,
//! and that is the shape the floor sits on. The counted anti is the
//! same read with the answer taken the other way round. The third puts
//! the block's own WHERE inside it, which is the shape that keeps the
//! bracket: the group walks per outer row, the predicate decides what
//! counts as a match, and the walk stops at the first one that passes.
//! The fourth projects the outer rows the semi kept, which is the sink
//! reading a level the block never touched. The fifth writes the block
//! on a level the pipeline has already walked off, asking about the
//! near end of a hop rather than about the rows in hand, which is one
//! degree read for a whole vector instead of one per row. The sixth
//! writes the block under an OR, which is the mark: the row survives
//! whatever the block answered and carries the answer as a column the
//! predicate reads, so the degree read is the same one the counted
//! semi makes and what changes is that nothing is dropped by it. The
//! seventh is the same pattern written as a plain required walk, which
//! answers a different question, one row per edge, and is here because
//! it is the other shape that reads degrees alone.
//!
//! Each one runs twice, once through the pipeline and once with
//! ZU_EXEC2=0, which is where the five block shapes ran before: an
//! EXISTS block had no compiled shape at all.
//!
//! Every run is crosschecked against the edge list the graph was built
//! from, so a bracket that keeps a row it should have dropped, or
//! hands the same row up twice, fails here rather than printing a fast
//! number.
//!
//! Everything runs at one worker, so the rate is per core and the
//! fleet's core counts stay out of the number.
//!
//! exec_exists_mrows_s_core floors the counted semi in millions of
//! outer rows a second.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench exists

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

const NODES: u64 = 1_000_000;
/// How many friends a hub has. Big enough that walking one costs
/// something a stop at the first edge does not.
const HUB: u64 = 33;
/// The score cut the third query puts inside the block. Scores are
/// strided over a hundred thousand, so this keeps about a fifth of the
/// friends and most of the hits have to be looked for.
const CUT: i64 = 80_000;

fn score_of(i: u64) -> i64 {
    ((i * 7919) % 100_000) as i64
}

/// Every fourth person is isolated. Of the rest, one in eight is a hub
/// with thirty three friends and the others have two, all picked far
/// apart in row order so the walk does not read one neighborhood over
/// and over.
fn edges() -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(NODES as usize * 5);
    for i in 0..NODES {
        if i % 4 == 0 {
            continue;
        }
        let n = if i % 8 == 1 { HUB } else { 2 };
        for k in 0..n {
            out.push((i as u32, ((i * 7919 + 13 + k * 104_729) % NODES) as u32));
        }
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

/// What the generator says a query has to answer: how many rows come
/// out and what the ids in them add up to. The total is what catches a
/// bracket that kept the wrong people rather than the wrong number of
/// them.
struct Want {
    rows: u64,
    id_total: i64,
}

/// The people with at least one friend the block would take.
fn matched(edges: &[(u32, u32)], keep: impl Fn(u32) -> bool) -> Vec<bool> {
    let mut hit = vec![false; NODES as usize];
    for &(src, dst) in edges {
        if keep(dst) {
            hit[src as usize] = true;
        }
    }
    hit
}

/// The outer rows a bracket of `negated` keeps, one row each.
fn expected(hit: &[bool], negated: bool) -> Want {
    let mut rows = 0;
    let mut id_total = 0i64;
    for (i, &h) in hit.iter().enumerate() {
        if h != negated {
            rows += 1;
            id_total += i as i64;
        }
    }
    Want { rows, id_total }
}

/// Reads a result the way the crosscheck needs it. A counted shape
/// carries its rows in the number it answers with and has no ids in it
/// to add up.
fn shape(r: &query::QueryResult) -> (u64, Option<i64>) {
    if r.columns.len() == 1 && r.columns[0] == "n" {
        let Value::Int(n) = r.rows[0][0] else {
            panic!("the count is an int");
        };
        return (n as u64, None);
    }
    let mut id_total = 0i64;
    for row in &r.rows {
        let Value::Int(n) = row[0] else {
            panic!("expected an id, got {:?}", row[0]);
        };
        id_total += n;
    }
    (r.rows.len() as u64, Some(id_total))
}

fn check(r: &query::QueryResult, want: &Want, source: &str) {
    let (rows, id_total) = shape(r);
    assert_eq!(rows, want.rows, "rows for {source}");
    if let Some(total) = id_total {
        assert_eq!(total, want.id_total, "id total for {source}");
    }
}

/// Median ms of `source`, with the answer checked on every run.
fn measure(db: &mut Zu1File, source: &str, want: &Want, runs: usize) -> f64 {
    let warm = query::run(source, db, &[]).expect("warmup");
    check(&warm, want, source);
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let r = query::run(source, db, &[]).expect("timed run");
            let ms = t.elapsed().as_secs_f64() * 1e3;
            check(&r, want, source);
            ms
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("exists.zu1");
    let t = Instant::now();
    let edges = edges();
    build(&path, &edges);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "exists: {NODES} persons, {} edges, load {:.1}s",
        edges.len(),
        t.elapsed().as_secs_f64()
    );

    let any = matched(&edges, |_| true);
    let cut = matched(&edges, |dst| score_of(u64::from(dst)) > CUT);
    let semi = expected(&any, false);
    let anti = expected(&any, true);
    let filtered = expected(&cut, false);
    println!(
        "exists: {} people with a friend, {} without, {} with one over the cut",
        semi.rows, anti.rows, filtered.rows
    );
    // The required walk answers one row per edge instead of one per
    // person, which is the reading it does that the block stops.
    let walk = Want {
        rows: edges.len() as u64,
        id_total: 0,
    };
    // The block on a walked level asks whether anyone knows the near
    // end of the hop, which is a question the walk itself does not
    // answer, so it keeps the edges whose source has an in edge.
    let mut known = vec![false; NODES as usize];
    for &(_, dst) in &edges {
        known[dst as usize] = true;
    }
    let walked = Want {
        rows: edges.iter().filter(|(src, _)| known[*src as usize]).count() as u64,
        id_total: 0,
    };

    // The mark keeps a row when either side of the OR takes it, so the
    // block's answer is one column of the decision rather than the
    // whole of it.
    let mark = Want {
        rows: (0..NODES)
            .filter(|&i| score_of(i) > CUT || any[i as usize])
            .count() as u64,
        id_total: 0,
    };

    let mark_src = format!(
        "MATCH (p:person) WHERE p.score > {CUT} \
         OR EXISTS {{ MATCH (p)-[:knows]->(f) }} RETURN count(p) AS n"
    );
    let filtered_src = format!(
        "MATCH (p:person) WHERE EXISTS {{ MATCH (p)-[:knows]->(f) WHERE f.score > {CUT} }} \
         RETURN count(p) AS n"
    );
    let cases: [(&str, &str, &Want, usize); 7] = [
        (
            "semi count",
            "MATCH (p:person) WHERE EXISTS { MATCH (p)-[:knows]->(f) } RETURN count(p) AS n",
            &semi,
            9,
        ),
        (
            "anti count",
            "MATCH (p:person) WHERE NOT EXISTS { MATCH (p)-[:knows]->(f) } RETURN count(p) AS n",
            &anti,
            9,
        ),
        ("filtered count", &filtered_src, &filtered, 5),
        (
            "semi rows",
            "MATCH (p:person) WHERE EXISTS { MATCH (p)-[:knows]->(f) } RETURN p.id AS p",
            &semi,
            5,
        ),
        (
            "block on a walked level",
            "MATCH (p:person)-[:knows]->(f) WHERE EXISTS { MATCH (p)<-[:knows]-(g) } \
             RETURN count(*) AS n",
            &walked,
            5,
        ),
        ("mark under an or", &mark_src, &mark, 9),
        (
            "required count",
            "MATCH (p:person)-[:knows]->(f) RETURN count(p) AS n",
            &walk,
            5,
        ),
    ];

    let mut semi_mrows = 0.0;
    for (what, source, want, runs) in cases {
        let new = measure(&mut db, source, want, runs);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, source, want, runs.min(3));
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mrows = NODES as f64 / 1e3 / new;
        println!(
            "exists {what}: {new:.1} ms {mrows:.0} M outer rows/s, old engine {old:.1} ms, \
             {:.2}x, crosschecked",
            old / new
        );
        if what == "semi count" {
            semi_mrows = mrows;
        }
    }

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_exists_mrows_s_core")
    {
        assert!(
            semi_mrows >= floor,
            "counted semi at {semi_mrows:.0} M outer rows/s under the {floor} M floor"
        );
        println!("gate: exists floor met");
    }
}
