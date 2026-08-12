//! The second pattern branch (perf/05 section 2, zu#76): two patterns
//! that share a variable and read the far end of both.
//!
//! The pipeline is a chain of levels, each expand walking off the newest
//! one, so a hop off a level it had already walked past used to compile
//! into nothing and the whole query went back to the old row at a time
//! engine. That is the shape every hub query is written in: count what a
//! person knows and what they like, group their friends by the city they
//! live in, filter one branch and read the other.
//!
//! What the branch does is pin the level it walks off, read its list
//! once, and pair every row of the newest level with the whole of it.
//! The pairing is why the newest level is pinned a row at a time under
//! it: everything below reads the levels above at their pins.
//!
//! A hub whose branches nothing reads is not this. That one is already a
//! degree product, a weight per source row with no walk at all, and it
//! stays that way. So every case here reads the far end of the branch,
//! which is what takes the weight fusion off the table.
//!
//! The graph is a million people over a thousand cities with out degrees
//! zero to seven, so the pairs a source row contributes go from none to
//! forty nine and the work is skewed the way a real hub is.
//!
//! A branch only survives compilation when the far ends of both
//! patterns are read and neither of them is the better place to start
//! from. Put a selective filter on one end and the optimizer roots the
//! scan there and walks back, which is a chain and not this. So every
//! case here reads both ends and none of them filters.
//!
//! Four shapes. The branch's far end grouped by city, which is the plain
//! one. Both far ends grouped, which is the one that can never turn into
//! anything else. A predicate across the two, the shape that has to see
//! both ends at the same time. And a branch off the head of a two hop
//! chain, where the pinned level sits two below the newest.
//!
//! Every case is crosschecked against the edge list, so a pairing that
//! loses or repeats rows fails here.
//!
//! Everything runs at one worker, so the rate is per core.
//!
//! exec_branch_mpairs_s_core floors the paired predicate in millions of
//! pairs a second, the pairs the cross product has to walk. That case
//! reads a column off each end, so nothing can fold it away later and
//! the floor keeps measuring the branch itself.
//!
//! exec_hub_mpairs_s_core floors the grouped branch, the case where one
//! end is read by nobody and turns into a weight. That is the one that
//! notices a fallback, since the walk it skips is most of the work.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench branch

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
const CITIES: u64 = 1_000;
/// The slice of the scan the two hop case runs over. Its fanout is the
/// square of everything else's, so it takes one person in fifty and
/// still walks millions of pairs.
const HOP_CITIES: u64 = 20;

fn city(i: u64) -> u64 {
    (i * 31) % CITIES
}

fn score(i: u64) -> u64 {
    (i * 2_654_435_761) % 1_000_003
}

/// Out degree zero to seven by row, so a source row contributes between
/// no pairs and forty nine.
fn degree(i: u64) -> u64 {
    i % 8
}

fn edges() -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(NODES as usize * 4);
    for i in 0..NODES {
        for k in 0..degree(i) {
            out.push((i as u32, ((i * 7919 + k * 104_729) % NODES) as u32));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// What a query has to answer: rows out and the sum of the last column.
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
    let path = dir.path().join("branch.zu1");
    let t = Instant::now();
    let edges = edges();
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &edges).expect("load");
    let cities: Vec<u64> = (0..NODES).map(city).collect();
    let scores: Vec<u64> = (0..NODES).map(score).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("city", PropValues::Int(&cities)),
            ("score", PropValues::Int(&scores)),
        ],
    )
    .expect("props");
    drop(db);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };

    // The answers, straight off the edge list. Neighbor lists first,
    // then what each shape pairs off them.
    let mut lists: Vec<Vec<u32>> = vec![Vec::new(); NODES as usize];
    for &(s, d) in &edges {
        lists[s as usize].push(d);
    }
    let mut pairs: u64 = 0;
    let mut ordered: u64 = 0;
    let mut hop_pairs: u64 = 0;
    let mut per_city: Vec<u64> = vec![0; CITIES as usize];
    let mut per_hop_city: Vec<u64> = vec![0; CITIES as usize];
    let mut per_town_pair: Vec<bool> = vec![false; (CITIES * CITIES) as usize];
    for a in 0..NODES as usize {
        let list = &lists[a];
        let deg = list.len() as u64;
        if deg == 0 {
            continue;
        }
        pairs += deg * deg;
        for &c in list {
            per_city[city(u64::from(c)) as usize] += deg;
        }
        for &b in list {
            let bt = city(u64::from(b));
            let bs = score(u64::from(b));
            for &c in list {
                per_town_pair[(bt * CITIES + city(u64::from(c))) as usize] = true;
                if bs > score(u64::from(c)) {
                    ordered += 1;
                }
            }
        }
        // The two hop case walks a slice of the scan, so its answer is
        // over the same slice.
        if city(a as u64) >= HOP_CITIES {
            continue;
        }
        let two: u64 = list.iter().map(|&b| lists[b as usize].len() as u64).sum();
        if two == 0 {
            continue;
        }
        hop_pairs += two * deg;
        for &d in list {
            per_hop_city[city(u64::from(d)) as usize] += two;
        }
    }
    let groups = per_city.iter().filter(|&&n| n > 0).count() as u64;
    let hop_groups = per_hop_city.iter().filter(|&&n| n > 0).count() as u64;
    let town_pairs = per_town_pair.iter().filter(|&&hit| hit).count() as u64;
    println!(
        "branch: {NODES} persons, {} edges, {pairs} pairs over {groups} cities, \
         {town_pairs} city pairs, {hop_pairs} two hop pairs, load {:.1}s",
        edges.len(),
        t.elapsed().as_secs_f64()
    );

    let cases: [(&str, &str, Want, u64); 4] = [
        (
            "grouped branch",
            "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
             RETURN c.city AS town, count(*) AS n",
            Want {
                rows: groups,
                total: pairs as i64,
            },
            pairs,
        ),
        (
            "both ends grouped",
            "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) \
             RETURN b.city AS x, c.city AS y, count(*) AS n",
            Want {
                rows: town_pairs,
                total: pairs as i64,
            },
            pairs,
        ),
        (
            "paired predicate",
            "MATCH (a:person)-[:knows]->(b) MATCH (a)-[:knows]->(c) WHERE b.score > c.score \
             RETURN count(*) AS n",
            Want {
                rows: 1,
                total: ordered as i64,
            },
            pairs,
        ),
        (
            "two hop branch",
            "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) MATCH (a)-[:knows]->(d) \
             WHERE a.city < 20 RETURN d.city AS town, count(*) AS n",
            Want {
                rows: hop_groups,
                total: hop_pairs as i64,
            },
            hop_pairs,
        ),
    ];

    let (mut paired, mut hub) = (0.0, 0.0);
    for (what, source, want, rows) in cases {
        let new = measure(&mut db, source, &want, 5);
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        // The old engine is the reference here, not the number under
        // the floor, so a case it spends seconds on is timed once.
        let old = measure(&mut db, source, &want, if new > 100.0 { 1 } else { 3 });
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mpairs = rows as f64 / 1e3 / new;
        println!(
            "branch {what}: {new:.1} ms {mpairs:.1} M pairs/s, old engine {old:.1} ms, {:.1}x, \
             crosschecked",
            old / new
        );
        match what {
            "paired predicate" => paired = mpairs,
            "grouped branch" => hub = mpairs,
            _ => {}
        }
    }

    if std::env::var("ZU_GATE").as_deref() != Ok("1") {
        return;
    }
    if let Some(floor) = budget("exec_branch_mpairs_s_core") {
        assert!(
            paired >= floor,
            "the paired predicate reads {paired:.1} M pairs/s, under the {floor} M floor"
        );
        println!("gate: branch floor met");
    }
    if let Some(floor) = budget("exec_hub_mpairs_s_core") {
        assert!(
            hub >= floor,
            "the grouped branch reads {hub:.1} M pairs/s, under the {floor} M floor"
        );
        println!("gate: hub floor met");
    }
}
