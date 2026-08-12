//! P3 table function gate (docs/07, zu#76): a leading CALL yields one
//! row per node of the graph, and the pipeline now takes that whole
//! shape instead of handing it back to the old row at a time engine.
//!
//! The graph is a million people in a thousand blocks, each person
//! knowing two others inside their own block, so the components are the
//! blocks and a group by on the yielded value has a thousand groups.
//! wcc is the kernel here because it is the cheap one: what this bench
//! is about is what happens to the million rows it answers with, not
//! how fast the labels are found.
//!
//! The kernel is timed on its own first so every case can say how much
//! of itself is the algorithm and how much is the rows, and that share
//! is printed next to each case. It is commentary and not the gate: the
//! count case runs the whole query in less time than the standalone
//! kernel reads, so the share there comes out at or under zero and a
//! floor on it would be a floor on timing noise. The gate is the end to
//! end rate, which both engines pay the same kernel into, so a fallback
//! still shows: the old engine reads about a sixth of what the pipeline
//! does on the same query.
//!
//! Five queries run over it. The count is the source and nothing else,
//! a million rows arriving from the kernel. The summed one reads the
//! yielded value on every row. The grouped one puts it in a group table
//! as a key. The hop walks an edge off every yielded node, and the
//! filtered hop puts the yielded value over that walk, which is the
//! shape that has to carry a value from the level below into the
//! comparison.
//!
//! Every run is crosschecked against a union-find over the edge list,
//! so a kernel or a pipeline that loses rows fails here rather than
//! printing a fast number.
//!
//! Everything runs at one worker, so the rate is per core and the
//! fleet's core counts stay out of the number.
//!
//! exec_call_mrows_s_core floors the count case in millions of yielded
//! rows a second, end to end.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench call

use std::time::Instant;

use zu::query::{self, Value};
use zu::zu1::algo;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::{GraphReader, bulk_load_as};

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
/// Nodes per block. The blocks are the components, so this is also what
/// decides how many groups a group by on the yielded label has.
const BLOCK: u64 = 1_000;
/// The label below which the filtered case keeps its rows: a tenth of
/// the blocks, so the predicate throws most of the walk away.
const CUT: u64 = NODES / BLOCK / 10 * BLOCK;

/// Two friends each, both inside the node's own block, so every block
/// closes into one component and no edge crosses between them.
fn edges() -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(NODES as usize * 2);
    for i in 0..NODES {
        let base = i / BLOCK * BLOCK;
        out.push((i as u32, (base + (i * 7919 + 13) % BLOCK) as u32));
        out.push((i as u32, (base + (i * 104_729 + 7) % BLOCK) as u32));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The component label of every node, by union-find over the edge list.
/// wcc labels a component with its smallest row, and a block's smallest
/// row is its base, so this is also what the group keys have to be.
fn components(edges: &[(u32, u32)]) -> Vec<u64> {
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            parent[x as usize] = parent[parent[x as usize] as usize];
            x = parent[x as usize];
        }
        x
    }
    let mut parent: Vec<u32> = (0..NODES as u32).collect();
    for &(s, d) in edges {
        let (a, b) = (find(&mut parent, s), find(&mut parent, d));
        if a != b {
            parent[a.max(b) as usize] = a.min(b);
        }
    }
    (0..NODES as u32)
        .map(|i| u64::from(find(&mut parent, i)))
        .collect()
}

/// What the generator says a query has to answer: rows out and the sum
/// of the last column. The sum is what catches a run that pairs a node
/// with the wrong label.
struct Want {
    rows: u64,
    total: i64,
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
    let path = dir.path().join("call.zu1");
    let t = Instant::now();
    let edges = edges();
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &edges).expect("load");
    drop(db);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };
    println!(
        "call: {NODES} persons, {} edges, {} blocks, load {:.1}s",
        edges.len(),
        NODES / BLOCK,
        t.elapsed().as_secs_f64()
    );

    let labels = components(&edges);
    let hops: u64 = edges.len() as u64;
    let cut_hops: u64 = edges
        .iter()
        .filter(|&&(s, _)| labels[s as usize] < CUT)
        .count() as u64;
    let groups = labels
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len() as u64;
    let label_sum: i64 = labels.iter().map(|&c| c as i64).sum();

    // The kernel on its own, the part of every case below that is the
    // algorithm rather than the pipeline. Timed the same way the engine
    // runs it, through a reader on the same open file.
    let mut reader = GraphReader::load_table(&mut db, "knows").expect("reader");
    let t = Instant::now();
    let kernel = algo::wcc(&mut db, &mut reader).expect("wcc");
    let mut kernel_ms = t.elapsed().as_secs_f64() * 1e3;
    for _ in 0..2 {
        let t = Instant::now();
        let again = algo::wcc(&mut db, &mut reader).expect("wcc again");
        kernel_ms = kernel_ms.min(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(again, kernel, "wcc is not deterministic");
    }
    assert_eq!(kernel, labels, "wcc disagrees with the union-find");
    println!("call: wcc kernel {kernel_ms:.1} ms, {groups} components, {hops} hops off the yield");

    let cases: [(&str, &str, Want); 5] = [
        (
            "yield count",
            "CALL wcc('knows') YIELD node, component RETURN count(node) AS n",
            Want {
                rows: 1,
                total: NODES as i64,
            },
        ),
        (
            "yield summed",
            "CALL wcc('knows') YIELD node, component RETURN count(node) AS n, sum(component) AS s",
            Want {
                rows: 1,
                total: label_sum,
            },
        ),
        (
            "yield grouped",
            "CALL wcc('knows') YIELD node, component RETURN component AS c, count(node) AS n",
            Want {
                rows: groups,
                total: NODES as i64,
            },
        ),
        (
            "yield hop",
            "CALL wcc('knows') YIELD node, component MATCH (node)-[:knows]->(f) \
             RETURN count(f) AS n",
            Want {
                rows: 1,
                total: hops as i64,
            },
        ),
        (
            "yield filtered hop",
            "CALL wcc('knows') YIELD node, component MATCH (node)-[:knows]->(f) \
             WHERE component < 100000 RETURN count(f) AS n",
            Want {
                rows: 1,
                total: cut_hops as i64,
            },
        ),
    ];
    assert_eq!(CUT, 100_000, "the filtered case's cut is written into it");

    let mut yield_mrows = 0.0;
    for (what, source, want) in cases {
        let new = measure(&mut db, source, &want, 5);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, source, &want, 3);
        unsafe { std::env::remove_var("ZU_EXEC2") };
        // What the rows cost on top of the kernel both engines run. At
        // or under zero means the query finished inside the noise of
        // the standalone kernel timing, which is the answer for a case
        // that does nothing but count what the kernel yielded.
        let mrows = NODES as f64 / 1e3 / new;
        println!(
            "call {what}: {new:.1} ms {mrows:.1} M rows/s, {:.1} ms of that outside the kernel, \
             old engine {old:.1} ms, {:.1}x, crosschecked",
            new - kernel_ms,
            old / new
        );
        if what == "yield count" {
            yield_mrows = mrows;
        }
    }

    if std::env::var("ZU_GATE").as_deref() == Ok("1")
        && let Some(floor) = budget("exec_call_mrows_s_core")
    {
        assert!(
            yield_mrows >= floor,
            "the count case reads {yield_mrows:.1} M rows/s, under the {floor} M floor"
        );
        println!("gate: call floor met");
    }
}
