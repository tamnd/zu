//! LDBC SNB SF1 gates for B1, B2, B4, and the IS/IC subset (docs/11,
//! docs/12 M2).
//!
//! Runs against the real SF1 person-knows-person graph, preprocessed by
//! scripts/prep-ldbc.sh into three plain text files under ZU_DATA:
//! ldbc-sf1-person-keys.txt (one person id per line),
//! ldbc-sf1-knows.txt (src dst id pairs), and
//! ldbc-sf1-person-props.txt (pipe-delimited person profiles). The ids
//! exceed u32, so the load goes through densify_keyed and the
//! primary-key index carries the original ids, exactly the path a
//! keyed COPY takes; the profiles land as props columns on the person
//! table, so the query path reads them the way any client would.
//!
//! B1 is warm 1-hop expands at deg <= 100 (the T1 workload shape), p50
//! in us. B2 is warm primary-key lookups, p50 in us. B4 is the 2-hop
//! factorized count over the whole graph through the query engine, p50
//! in ms. Triangle is the unseeded directed triangle count over the
//! whole graph, the shape the optimizer closes with AspJoin, p50 in
//! ms. Ordered is the same cycle with the id ordering written on it,
//! the shape every triangle benchmark asks for, where the predicate
//! lands between the expand and the close and the close has to keep
//! its fusion anyway, p50 in ms. Close is the same triangle walked
//! undirected, the shape that keeps the binary probe and so runs the
//! semijoin folded into the expand, p50 in ms. IS is the IS1-shaped
//! profile read by original
//! id, all eight properties through zu::query::run, gated at the T2 1
//! ms warm p50.
//! IC is an IC-shaped 2-hop friends-of-friends read with DISTINCT,
//! ORDER BY, and LIMIT, p50 in ms. Distinct two-hop is the same
//! seeded walk asked as a number rather than as rows, how many
//! different people a person reaches in two steps, which is the shape
//! every k-hop benchmark counts with and the one place the cost of the
//! distinct set shows up on its own, p50 in ms. Every phase crosschecks against a
//! reference computed from the raw files, so a number cannot come from
//! a broken reader or a wrong plan.
//!
//! Cardinality is the odd one out: it times nothing. It runs a twelve
//! query corpus profiled and pools the q-error of every operator whose
//! estimate the optimizer committed to, which is the perf/12 section 4
//! measurement. The numbers come out of the data and the estimator
//! alone, so they are the same on every host and a move in them is a
//! change in the optimizer, not weather. With ZU_GATE=1 the process exits
//! nonzero when a ceiling in bench/budgets.toml is missed, and missing
//! data fails the gate instead of skipping it.
//!
//! Run: ZU_GATE=1 ZU_DATA=~/data/zu cargo bench -p zu --bench ldbc

use std::collections::HashMap;
use std::time::Instant;

use zu::zu1::file::Zu1File;
use zu::zu1::graph::{
    Direction, GraphReader, bulk_load_keyed, densify_keyed, read_key_edge_list, read_key_list,
};
use zu::zu1::props::{PropValues, store_props};
use zu_query::exec::{Engine, Options, Value, Wcoj};

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

fn xorshift(rng: &mut u64) -> u64 {
    *rng ^= *rng << 13;
    *rng ^= *rng >> 7;
    *rng ^= *rng << 17;
    *rng
}

/// The eight person profile fields in query order: firstName,
/// lastName, gender, birthday, locationIP, browserUsed, cityId,
/// creationDate. One entry per dense row, the reference the IS and IC
/// phases assert against.
type ProfileRows = Vec<[String; 8]>;

/// Loads the preprocessed SF1 files and bulk loads them into a fresh
/// zu1 file as person/knows with the id key index and the person
/// profile props columns. Returns the dense sorted edge list for
/// reference computations, the key of every row, and the profile
/// fields of every row.
fn load(data: &str, path: &std::path::Path) -> (Vec<(u32, u32)>, Vec<u64>, ProfileRows) {
    let started = Instant::now();
    let keys = read_key_list(std::path::Path::new(&format!(
        "{data}/ldbc-sf1-person-keys.txt"
    )))
    .expect("person keys");
    let edges = read_key_edge_list(std::path::Path::new(&format!("{data}/ldbc-sf1-knows.txt")))
        .expect("knows edges");
    let (mut dense, by_row) = densify_keyed(&keys, &edges).expect("densify");
    dense.sort_unstable();
    dense.dedup();
    let text =
        std::fs::read_to_string(format!("{data}/ldbc-sf1-person-props.txt")).expect("person props");
    let mut by_key: HashMap<u64, [&str; 8]> = HashMap::with_capacity(by_row.len());
    for line in text.lines() {
        let fields: Vec<&str> = line.split('|').collect();
        let [id, rest @ ..] = fields.as_slice() else {
            panic!("empty props line");
        };
        let profile: [&str; 8] = rest.try_into().expect("nine pipe-delimited fields");
        let id: u64 = id.parse().expect("numeric person id");
        assert!(
            by_key.insert(id, profile).is_none(),
            "duplicate person {id}"
        );
    }
    let profiles: ProfileRows = by_row
        .iter()
        .map(|key| by_key[key].map(str::to_string))
        .collect();
    let parsed = started.elapsed();
    let load_started = Instant::now();
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_keyed(
        &mut db,
        "person",
        "knows",
        by_row.len() as u64,
        &dense,
        Some(&by_row),
    )
    .expect("bulk load");
    let cities: Vec<u64> = profiles
        .iter()
        .map(|p| p[6].parse().expect("numeric city id"))
        .collect();
    let strs = |i: usize| -> Vec<&[u8]> { profiles.iter().map(|p| p[i].as_bytes()).collect() };
    let (first, last) = (strs(0), strs(1));
    let (gender, birthday) = (strs(2), strs(3));
    let (ip, browser, created) = (strs(4), strs(5), strs(7));
    store_props(
        &mut db,
        "person",
        &[
            ("id", PropValues::Int(&by_row)),
            ("firstName", PropValues::Str(&first)),
            ("lastName", PropValues::Str(&last)),
            ("gender", PropValues::Str(&gender)),
            ("birthday", PropValues::Str(&birthday)),
            ("locationIP", PropValues::Str(&ip)),
            ("browserUsed", PropValues::Str(&browser)),
            ("cityId", PropValues::Int(&cities)),
            ("creationDate", PropValues::Str(&created)),
        ],
    )
    .expect("store props");
    let analyze_started = Instant::now();
    zu::zu1::colors::analyze(&mut db).expect("analyze");
    println!(
        "sf1: {} persons, {} knows edges, 9 props columns, parse {:.2}s, load {:.2}s, analyze {:.2}s",
        by_row.len(),
        dense.len(),
        parsed.as_secs_f64(),
        load_started.elapsed().as_secs_f64(),
        analyze_started.elapsed().as_secs_f64()
    );
    (dense, by_row, profiles)
}

/// B1: warm 1-hop expands over rows with out-degree at most 100, the T1
/// workload shape docs/11 defines the budget on. The gated number goes
/// through the cached-group path, the read the executor's Expand makes
/// once a query is warm; the storage point path prints alongside as
/// information, its regression floors live with the LiveJournal keys.
/// A sample crosschecks against the reference adjacency built from the
/// raw edge list.
fn run_one_hop(path: &std::path::Path, edges: &[(u32, u32)], node_count: u64) -> f64 {
    let mut outdeg = vec![0u32; node_count as usize];
    for &(s, _) in edges {
        outdeg[s as usize] += 1;
    }
    let eligible: Vec<u64> = (0..node_count)
        .filter(|&n| outdeg[n as usize] <= 100)
        .collect();
    let mut db = Zu1File::open(path).expect("open");
    let mut reader = GraphReader::load_table(&mut db, "knows").expect("load knows");
    let lookups = 200_000usize;
    let mut rng = 0x9E3779B97F4A7C15u64;
    let mut nbrs = Vec::new();
    for _ in 0..10_000 {
        let node = eligible[(xorshift(&mut rng) as usize) % eligible.len()];
        reader
            .neighbors_dir(&mut db, node, Direction::Fwd)
            .expect("warmup read");
    }
    // A single warm expand sits at or below timer resolution, so the
    // clock samples batches of 100 and the quoted latencies are batch
    // means per expand. That is still a latency gate, not a throughput
    // one: a regression that makes expands decode per call moves the
    // p50 three orders of magnitude.
    let batch = 100usize;
    let batches = lookups / batch;
    let mut lat = Vec::with_capacity(batches);
    let mut edges_read = 0u64;
    let started = Instant::now();
    for _ in 0..batches {
        let t = Instant::now();
        for _ in 0..batch {
            let node = eligible[(xorshift(&mut rng) as usize) % eligible.len()];
            nbrs.clear();
            let hop = reader
                .neighbors_dir(&mut db, node, Direction::Fwd)
                .expect("expand read");
            nbrs.extend_from_slice(hop);
            edges_read += nbrs.len() as u64;
        }
        lat.push(t.elapsed() / batch as u32);
    }
    let secs = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let p50 = lat[batches / 2].as_secs_f64() * 1e6;
    println!(
        "sf1 1-hop expand (deg <= 100, {} of {} rows): p50 {p50:.3} us, p99 {:.3} us per expand over batches of {batch}, {:.0} K expands/s, {edges_read} edges read",
        eligible.len(),
        node_count,
        lat[batches * 99 / 100].as_secs_f64() * 1e6,
        lookups as f64 / secs / 1e3
    );
    let mut point_lat = Vec::with_capacity(lookups / 10);
    for _ in 0..lookups / 10 {
        let node = eligible[(xorshift(&mut rng) as usize) % eligible.len()];
        nbrs.clear();
        let t = Instant::now();
        reader
            .neighbors_dir_into(&mut db, node, Direction::Fwd, &mut nbrs)
            .expect("point read");
        point_lat.push(t.elapsed());
    }
    point_lat.sort_unstable();
    println!(
        "sf1 1-hop point path: p50 {:.2} us, p99 {:.2} us (information, floors on LiveJournal)",
        point_lat[point_lat.len() / 2].as_secs_f64() * 1e6,
        point_lat[point_lat.len() * 99 / 100].as_secs_f64() * 1e6
    );
    for _ in 0..100 {
        let node = eligible[(xorshift(&mut rng) as usize) % eligible.len()];
        let want: Vec<u64> = edges
            .iter()
            .filter(|&&(s, _)| u64::from(s) == node)
            .map(|&(_, d)| u64::from(d))
            .collect();
        let hop = reader
            .neighbors_dir(&mut db, node, Direction::Fwd)
            .expect("expand read");
        assert_eq!(hop, want, "expand of {node} disagrees with the edge list");
        nbrs.clear();
        reader
            .neighbors_dir_into(&mut db, node, Direction::Fwd, &mut nbrs)
            .expect("point read");
        assert_eq!(
            nbrs, want,
            "point neighbors of {node} disagree with the edge list"
        );
    }
    println!("sf1 1-hop crosscheck: 100 nodes match the edge list on both paths");
    p50
}

/// B2: warm primary-key lookups through the sealed sorted index, all
/// hits, each asserted against the row the key was loaded at.
fn run_key_lookups(path: &std::path::Path, by_row: &[u64]) -> f64 {
    let mut db = Zu1File::open(path).expect("open");
    let mut reader = GraphReader::load_table(&mut db, "knows").expect("load knows");
    let lookups = 200_000usize;
    let mut rng = 0x853C_49E6_748F_EA9Bu64;
    let n = by_row.len() as u64;
    for _ in 0..10_000 {
        let row = xorshift(&mut rng) % n;
        reader
            .lookup_key(&mut db, by_row[row as usize])
            .expect("warmup lookup");
    }
    let set: Vec<(u64, u64)> = (0..lookups)
        .map(|_| {
            let row = xorshift(&mut rng) % n;
            (by_row[row as usize], row)
        })
        .collect();
    let mut lat = Vec::with_capacity(lookups);
    let started = Instant::now();
    for &(key, want) in &set {
        let t = Instant::now();
        let got = reader.lookup_key(&mut db, key).expect("lookup");
        lat.push(t.elapsed());
        assert_eq!(got, Some(want), "key {key}");
    }
    let secs = started.elapsed().as_secs_f64();
    lat.sort_unstable();
    let p50 = lat[lookups / 2].as_secs_f64() * 1e6;
    println!(
        "sf1 key-lookup: p50 {p50:.2} us, p99 {:.2} us, {:.0} K lookups/s, all hits",
        lat[lookups * 99 / 100].as_secs_f64() * 1e6,
        lookups as f64 / secs / 1e3
    );
    assert_eq!(
        reader
            .lookup_key(&mut db, by_row[n as usize - 1] + 1)
            .expect("miss"),
        None,
        "a key above the domain must miss"
    );
    p50
}

/// B4: the 2-hop factorized count over the whole graph through the
/// query engine, parse to result. The count is asserted against the
/// reference sum over the edge list, so the number cannot come from a
/// plan that drops or duplicates paths.
fn run_two_hop(path: &std::path::Path, edges: &[(u32, u32)], node_count: u64) -> f64 {
    let mut outdeg = vec![0u64; node_count as usize];
    for &(s, _) in edges {
        outdeg[s as usize] += 1;
    }
    let expected: i64 = edges.iter().map(|&(_, d)| outdeg[d as usize] as i64).sum();
    let mut db = Zu1File::open(path).expect("open");
    let source = "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN count(c) AS walks";
    let runs = 50usize;
    for _ in 0..5 {
        zu::query::run(source, &mut db, &[]).expect("warmup run");
    }
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[]).expect("two-hop count");
        lat.push(t.elapsed());
        assert_eq!(
            r.rows,
            [[Value::Int(expected)]],
            "2-hop count disagrees with the edge list reference"
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64() * 1e3;
    println!(
        "sf1 2-hop count: {expected} paths, p50 {p50:.3} ms, p99 {:.3} ms over {runs} runs",
        lat[runs * 99 / 100].as_secs_f64() * 1e3
    );
    p50
}

/// Triangle: the unseeded directed triangle count over the whole
/// graph, the shape the optimizer upgrades to AspJoin because closing
/// every 2-path against storage would cost more probes than the graph
/// has edges. The reference recomputes the count from the raw edge
/// list, one adjacency walk with binary search on the sorted dense
/// pairs, so the number cannot come from a join that drops or invents
/// closures.
fn run_triangle_count(path: &std::path::Path, edges: &[(u32, u32)], node_count: u64) -> f64 {
    let mut adj = vec![Vec::new(); node_count as usize];
    for &(s, d) in edges {
        adj[s as usize].push(d);
    }
    let mut expected = 0i64;
    for &(a, b) in edges {
        for &c in &adj[b as usize] {
            if edges.binary_search(&(a, c)).is_ok() {
                expected += 1;
            }
        }
    }
    let mut db = Zu1File::open(path).expect("open");
    let source = "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
                  RETURN count(*) AS triangles";
    let runs = 15usize;
    for _ in 0..3 {
        zu::query::run(source, &mut db, &[]).expect("warmup run");
    }
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[]).expect("triangle count");
        lat.push(t.elapsed());
        assert_eq!(
            r.rows,
            [[Value::Int(expected)]],
            "triangle count disagrees with the edge list reference"
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64() * 1e3;
    println!(
        "sf1 triangle count: {expected} triangles, p50 {p50:.3} ms, max {:.3} ms over {runs} runs",
        lat[runs - 1].as_secs_f64() * 1e3
    );

    // The default path above is the WCOJ intersection now that the
    // optimizer marks the cyclic close, so the gate rides it. The
    // binary join stays measured as the regression reference, pinned
    // per call rather than through ZU_WCOJ: what a bench measures
    // should not depend on what else is in the environment when it
    // runs, and the two numbers printed here differ by the join and
    // nothing else (#513).
    let pinned = Options {
        engine: Engine::Pipeline,
        wcoj: Wcoj::Off,
        ..Options::default()
    };
    let binary = |db: &mut _| zu::query::run_with(source, db, &[], &pinned);
    for _ in 0..3 {
        binary(&mut db).expect("binary warmup run");
    }
    let mut blat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let r = binary(&mut db).expect("binary triangle count");
        blat.push(t.elapsed());
        assert_eq!(
            r.rows,
            [[Value::Int(expected)]],
            "binary triangle count disagrees with the edge list reference"
        );
    }
    blat.sort_unstable();
    println!(
        "sf1 triangle count (binary): p50 {:.3} ms, max {:.3} ms over {runs} runs",
        blat[runs / 2].as_secs_f64() * 1e3,
        blat[runs - 1].as_secs_f64() * 1e3
    );
    p50
}

/// Ordered triangle: the same directed cycle with the id ordering
/// written on it, which is how every benchmark asks for triangles when
/// it wants each one counted once instead of once per rotation.
///
/// The ordering is what makes the shape interesting. Filter placement
/// puts `b.id < c.id` where c binds, straight between the expand and
/// the close, and the physical compiler only fuses a close into the
/// expand under it when the two are adjacent, so the predicate used to
/// cost the whole intersection and the plan went back to one storage
/// probe per candidate row. The reference walks the same order over the
/// raw pairs, so a plan that counts a triple twice or drops one fails
/// here rather than showing up as a fast wrong number.
fn run_ordered_triangle(
    path: &std::path::Path,
    edges: &[(u32, u32)],
    by_row: &[u64],
    node_count: u64,
) -> f64 {
    let mut adj = vec![Vec::new(); node_count as usize];
    for &(s, d) in edges {
        adj[s as usize].push(d);
    }
    let key = |row: u32| by_row[row as usize];
    let mut expected = 0i64;
    for &(a, b) in edges {
        if key(a) >= key(b) {
            continue;
        }
        for &c in &adj[b as usize] {
            if key(b) < key(c) && edges.binary_search(&(a, c)).is_ok() {
                expected += 1;
            }
        }
    }
    let mut db = Zu1File::open(path).expect("open");
    let source = "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
                  WHERE a.id < b.id AND b.id < c.id RETURN count(*) AS triangles";
    let runs = 15usize;
    for _ in 0..3 {
        zu::query::run(source, &mut db, &[]).expect("warmup run");
    }
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[]).expect("ordered triangle");
        lat.push(t.elapsed());
        assert_eq!(
            r.rows,
            [[Value::Int(expected)]],
            "ordered triangle disagrees with the edge list reference"
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64() * 1e3;
    println!(
        "sf1 ordered triangle: {expected} triangles, p50 {p50:.3} ms, max {:.3} ms over {runs} runs",
        lat[runs - 1].as_secs_f64() * 1e3
    );
    p50
}

/// Close: the same triangle walked undirected, which is the shape that
/// costs the intersection the most. Every end reads two stored lists,
/// so the probe side is a union built for the vector and the seed side
/// is two walks instead of one, and the undirected graph has twice the
/// edges to walk. That makes this the number that moves furthest when
/// the closing kernel changes. The reference walks the undirected
/// adjacency and binary searches the sorted pair list, counting the
/// same ordered triples the query counts.
fn run_undirected_close(path: &std::path::Path, edges: &[(u32, u32)], node_count: u64) -> f64 {
    let mut both: Vec<(u32, u32)> = Vec::with_capacity(edges.len() * 2);
    for &(s, d) in edges {
        both.push((s, d));
        both.push((d, s));
    }
    both.sort_unstable();
    both.dedup();
    let mut adj = vec![Vec::new(); node_count as usize];
    for &(s, d) in &both {
        adj[s as usize].push(d);
    }
    let mut expected = 0i64;
    for &(a, b) in &both {
        for &c in &adj[b as usize] {
            if both.binary_search(&(a, c)).is_ok() {
                expected += 1;
            }
        }
    }
    let mut db = Zu1File::open(path).expect("open");
    let source = "MATCH (a:person)-[:knows]-(b)-[:knows]-(c), (a)-[:knows]-(c) \
                  RETURN count(*) AS closed";
    let runs = 15usize;
    for _ in 0..3 {
        zu::query::run(source, &mut db, &[]).expect("warmup run");
    }
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[]).expect("undirected close");
        lat.push(t.elapsed());
        assert_eq!(
            r.rows,
            [[Value::Int(expected)]],
            "undirected close disagrees with the edge list reference"
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64() * 1e3;
    println!(
        "sf1 undirected close: {expected} closed paths, p50 {p50:.3} ms, max {:.3} ms over {runs} runs",
        lat[runs - 1].as_secs_f64() * 1e3
    );
    p50
}

/// IS: the IS1-shaped profile read, all eight person properties by
/// original id through zu::query::run, parse to result. Every measured
/// run is asserted field by field against the raw props file, so the
/// number cannot come from a reader that returns the wrong row or a
/// column stored out of order.
fn run_is_reads(path: &std::path::Path, by_row: &[u64], profiles: &ProfileRows) -> f64 {
    let mut db = Zu1File::open(path).expect("open");
    let source = "MATCH (p:person {id: $id}) \
                  RETURN p.firstName AS firstName, p.lastName AS lastName, \
                         p.gender AS gender, p.birthday AS birthday, \
                         p.locationIP AS locationIP, p.browserUsed AS browserUsed, \
                         p.cityId AS cityId, p.creationDate AS creationDate";
    let n = by_row.len() as u64;
    let mut rng = 0xD1B5_4A32_D192_ED03u64;
    for _ in 0..200 {
        let row = (xorshift(&mut rng) % n) as usize;
        let id = Value::Int(by_row[row] as i64);
        zu::query::run(source, &mut db, &[("id", id)]).expect("warmup profile read");
    }
    let runs = 2_000usize;
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let row = (xorshift(&mut rng) % n) as usize;
        let id = Value::Int(by_row[row] as i64);
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[("id", id)]).expect("profile read");
        lat.push(t.elapsed());
        let p = &profiles[row];
        let want = vec![
            Value::Str(p[0].clone()),
            Value::Str(p[1].clone()),
            Value::Str(p[2].clone()),
            Value::Str(p[3].clone()),
            Value::Str(p[4].clone()),
            Value::Str(p[5].clone()),
            Value::Int(p[6].parse().expect("numeric city id")),
            Value::Str(p[7].clone()),
        ];
        assert_eq!(
            r.rows,
            [want],
            "profile of person {} disagrees with the props file",
            by_row[row]
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64() * 1e3;
    println!(
        "sf1 IS profile read: p50 {p50:.3} ms, p99 {:.3} ms over {runs} runs, all rows crosschecked",
        lat[runs * 99 / 100].as_secs_f64() * 1e3
    );
    p50
}

/// The seeded 2-hop asked as a number rather than as rows: how many
/// different people a person reaches in two steps. It is the shape
/// every k-hop benchmark counts with, and the only thing between the
/// expand and the answer is the distinct set, so this phase is where
/// the cost of that set shows up unmixed with a projection or a sort.
/// The reference is the same walk over the raw edge list.
fn run_distinct_two_hop(path: &std::path::Path, edges: &[(u32, u32)], by_row: &[u64]) -> f64 {
    let n = by_row.len();
    let mut adj = vec![Vec::new(); n];
    for &(s, d) in edges {
        adj[s as usize].push(d as usize);
    }
    let seeds: Vec<usize> = (0..n).filter(|&r| !adj[r].is_empty()).collect();
    let reference = |seed: usize| -> i64 {
        let mut hits: Vec<usize> = adj[seed]
            .iter()
            .flat_map(|&f| adj[f].iter().copied())
            .collect();
        hits.sort_unstable();
        hits.dedup();
        hits.len() as i64
    };
    let mut db = Zu1File::open(path).expect("open");
    let source = "MATCH (p:person {id: $id})-[:knows]->(f)-[:knows]->(ff) \
                  RETURN count(DISTINCT ff) AS n";
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..50 {
        let seed = seeds[(xorshift(&mut rng) as usize) % seeds.len()];
        let id = Value::Int(by_row[seed] as i64);
        zu::query::run(source, &mut db, &[("id", id)]).expect("warmup distinct two-hop");
    }
    let runs = 500usize;
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let seed = seeds[(xorshift(&mut rng) as usize) % seeds.len()];
        let id = Value::Int(by_row[seed] as i64);
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[("id", id)]).expect("distinct two-hop");
        lat.push(t.elapsed());
        assert_eq!(
            r.rows,
            [[Value::Int(reference(seed))]],
            "distinct two-hop out of person {} disagrees with the reference",
            by_row[seed]
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64() * 1e3;
    println!(
        "sf1 distinct two-hop: p50 {p50:.3} ms, p99 {:.3} ms over {runs} runs, all counts crosschecked",
        lat[runs * 99 / 100].as_secs_f64() * 1e3
    );
    p50
}

/// IC: an IC-shaped friends-of-friends read, 2 hops out of one person
/// with DISTINCT, a property projection, ORDER BY, and LIMIT, parse to
/// result. The reference recomputes each seed's answer from the raw
/// edge list and props file.
fn run_ic_friends_of_friends(
    path: &std::path::Path,
    edges: &[(u32, u32)],
    by_row: &[u64],
    profiles: &ProfileRows,
) -> f64 {
    let n = by_row.len();
    let mut adj = vec![Vec::new(); n];
    for &(s, d) in edges {
        adj[s as usize].push(d as usize);
    }
    let seeds: Vec<usize> = (0..n).filter(|&r| !adj[r].is_empty()).collect();
    let reference = |seed: usize| -> Vec<Vec<Value>> {
        let mut hits: Vec<usize> = adj[seed]
            .iter()
            .flat_map(|&f| adj[f].iter().copied())
            .filter(|&ff| ff != seed)
            .collect();
        hits.sort_unstable_by_key(|&ff| by_row[ff]);
        hits.dedup();
        hits.truncate(20);
        hits.into_iter()
            .map(|ff| {
                vec![
                    Value::Int(by_row[ff] as i64),
                    Value::Str(profiles[ff][0].clone()),
                    Value::Str(profiles[ff][1].clone()),
                ]
            })
            .collect()
    };
    let mut db = Zu1File::open(path).expect("open");
    let source = "MATCH (p:person {id: $id})-[:knows]->(f)-[:knows]->(ff) \
                  WHERE ff.id <> $id \
                  RETURN DISTINCT ff.id AS id, ff.firstName AS firstName, \
                         ff.lastName AS lastName \
                  ORDER BY id LIMIT 20";
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..50 {
        let seed = seeds[(xorshift(&mut rng) as usize) % seeds.len()];
        let id = Value::Int(by_row[seed] as i64);
        zu::query::run(source, &mut db, &[("id", id)]).expect("warmup fof read");
    }
    let runs = 500usize;
    let mut lat = Vec::with_capacity(runs);
    for _ in 0..runs {
        let seed = seeds[(xorshift(&mut rng) as usize) % seeds.len()];
        let id = Value::Int(by_row[seed] as i64);
        let t = Instant::now();
        let r = zu::query::run(source, &mut db, &[("id", id)]).expect("fof read");
        lat.push(t.elapsed());
        assert_eq!(
            r.rows,
            reference(seed),
            "friends of friends of person {} disagree with the reference",
            by_row[seed]
        );
    }
    lat.sort_unstable();
    let p50 = lat[runs / 2].as_secs_f64() * 1e3;
    println!(
        "sf1 IC friends-of-friends: p50 {p50:.3} ms, p99 {:.3} ms over {runs} runs, all rows crosschecked",
        lat[runs * 99 / 100].as_secs_f64() * 1e3
    );
    p50
}

/// One entry of the cardinality corpus: a name for the printout, the
/// query text, and the parameters it wants bound.
type CardQuery = (&'static str, &'static str, Vec<(&'static str, Value)>);

/// Cardinality quality over the SF1 corpus (perf/12 §4): every query
/// below runs profiled, and each operator whose measured row count is
/// a real output cardinality contributes one q-error,
/// `max(est/act, act/est)`. The corpus is the shapes this file already
/// gates plus the property-predicate shapes, because the property
/// selectivities are the part of the estimator that is still a table
/// of constants and the numbers should say so out loud.
///
/// Every operator that carries a pessimistic ceiling is also held
/// against it. A ceiling is a promise and not a guess, so real rows
/// above one is a bug in the bound and perf/12 §6 makes it a hard
/// fail whatever the percentiles say.
///
/// Profiled runs are sequential by construction, so this phase is
/// about estimate quality and says nothing about speed. Returns the
/// p50, p90, p99, and worst of the pooled q-errors, plus the number of
/// violated ceilings.
fn run_cardinality(
    path: &std::path::Path,
    by_row: &[u64],
    profiles: &ProfileRows,
) -> (f64, f64, f64, f64, usize) {
    let mut db = Zu1File::open(path).expect("open");

    // Literals lifted out of the loaded data so every predicate below
    // matches something. `gender` has two values, `browserUsed` a
    // handful, `firstName` thousands: three very different true
    // selectivities against one assumed constant.
    let seed = Value::Int(by_row[by_row.len() / 3] as i64);
    let gender = Value::Str(profiles[0][2].clone());
    let browser = Value::Str(profiles[0][5].clone());
    let first = Value::Str(profiles[0][0].clone());
    let birthday = Value::Str(profiles[by_row.len() / 2][3].clone());

    let corpus: Vec<CardQuery> = vec![
        ("scan", "MATCH (p:person) RETURN count(p) AS n", vec![]),
        (
            "hop",
            "MATCH (a:person)-[:knows]->(b) RETURN count(b) AS n",
            vec![],
        ),
        (
            "two-hop",
            "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN count(c) AS n",
            vec![],
        ),
        (
            "triangle",
            "MATCH (a:person)-[:knows]->(b)-[:knows]->(c), (a)-[:knows]->(c) \
             RETURN count(*) AS n",
            vec![],
        ),
        (
            "seeded-hop",
            "MATCH (p:person {id: $id})-[:knows]->(f) RETURN count(f) AS n",
            vec![("id", seed.clone())],
        ),
        (
            "IC-fof",
            "MATCH (p:person {id: $id})-[:knows]->(f)-[:knows]->(ff) \
             WHERE ff.id <> $id \
             RETURN DISTINCT ff.id AS id ORDER BY id LIMIT 20",
            vec![("id", seed.clone())],
        ),
        (
            "eq-gender",
            "MATCH (p:person) WHERE p.gender = $v RETURN count(p) AS n",
            vec![("v", gender)],
        ),
        (
            "eq-browser",
            "MATCH (p:person) WHERE p.browserUsed = $v RETURN count(p) AS n",
            vec![("v", browser)],
        ),
        (
            "eq-firstname",
            "MATCH (p:person) WHERE p.firstName = $v RETURN count(p) AS n",
            vec![("v", first.clone())],
        ),
        (
            "range-birthday",
            "MATCH (p:person) WHERE p.birthday < $v RETURN count(p) AS n",
            vec![("v", birthday)],
        ),
        (
            "eq-then-hop",
            "MATCH (p:person)-[:knows]->(f) WHERE p.firstName = $v RETURN count(f) AS n",
            vec![("v", first)],
        ),
        (
            "seeded-hop-eq",
            "MATCH (p:person {id: $id})-[:knows]->(f) WHERE f.gender = $v \
             RETURN count(f) AS n",
            vec![("id", seed), ("v", Value::Str(profiles[0][2].clone()))],
        ),
    ];

    let mut all = Vec::new();
    let mut violations = 0usize;
    for (name, source, params) in &corpus {
        let borrowed: Vec<(&str, Value)> = params.iter().map(|(k, v)| (*k, v.clone())).collect();
        let profile = zu::query::profile(source, &mut db, &borrowed).expect("profile");
        let mut worst: Option<(f64, String, f64, u64)> = None;
        for stage in &profile.stages {
            for op in &stage.ops {
                if op.bound_violation() {
                    violations += 1;
                    println!(
                        "sf1 cardinality {name}: BOUND VIOLATION at {}, ceiling {:.0} vs {} actual",
                        op.name(),
                        op.bnd.unwrap_or_default(),
                        op.flat
                    );
                }
                let (Some(q), Some(est)) = (op.qerror(), op.est) else {
                    continue;
                };
                all.push(q);
                if worst.as_ref().is_none_or(|(w, ..)| q > *w) {
                    worst = Some((q, op.name(), est, op.flat));
                }
            }
        }
        let (q, op, est, act) = worst.expect("every query has at least a scan");
        println!(
            "sf1 cardinality {name}: worst q {q:.1} at {op}, est {:.0} vs {act} actual",
            est
        );
    }

    all.sort_by(f64::total_cmp);
    let pick = |pct: usize| all[(all.len() - 1) * pct / 100];
    let (p50, p90, p99, max) = (pick(50), pick(90), pick(99), all[all.len() - 1]);
    println!(
        "sf1 cardinality: {} operators over {} queries, q-error p50 {p50:.2}, p90 {p90:.2}, p99 {p99:.2}, max {max:.2}, {violations} bound violations",
        all.len(),
        corpus.len(),
    );
    (p50, p90, p99, max, violations)
}

/// The M4 table function kernels over the loaded file (docs/07 §4):
/// pagerank, wcc, sssp, louvain timed as direct kernel calls, each
/// crosschecked against an independent computation over the raw edge
/// list, plus one CALL through zuQL so the query surface runs too.
/// Returns kernel seconds in that order.
fn run_table_functions(
    path: &std::path::Path,
    edges: &[(u32, u32)],
    by_row: &[u64],
    node_count: u64,
) -> (f64, f64, f64, f64) {
    use zu::zu1::algo;
    let n = node_count as usize;
    let mut db = Zu1File::open(path).expect("open");
    let mut reader = GraphReader::load_table(&mut db, "knows").expect("reader");

    let t = Instant::now();
    let ranks = algo::pagerank(&mut db, &mut reader, algo::PAGERANK_ITERATIONS).expect("pagerank");
    let pagerank_s = t.elapsed().as_secs_f64();
    let sum: f64 = ranks.iter().sum();
    assert!((sum - 1.0).abs() < 1e-9, "ranks sum to {sum}");
    let mut reference = vec![1.0 / n as f64; n];
    let mut outdeg = vec![0u64; n];
    for &(s, _) in edges {
        outdeg[s as usize] += 1;
    }
    for _ in 0..algo::PAGERANK_ITERATIONS {
        let dangling: f64 = (0..n)
            .filter(|&v| outdeg[v] == 0)
            .map(|v| reference[v])
            .sum();
        let base = (1.0 - algo::PAGERANK_DAMPING + algo::PAGERANK_DAMPING * dangling) / n as f64;
        let mut next = vec![base; n];
        for &(s, d) in edges {
            next[d as usize] +=
                algo::PAGERANK_DAMPING * reference[s as usize] / outdeg[s as usize] as f64;
        }
        reference = next;
    }
    let drift = ranks
        .iter()
        .zip(&reference)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f64, f64::max);
    assert!(drift < 1e-12, "pagerank drifts {drift} from the reference");
    println!(
        "sf1 pagerank: {} iterations in {pagerank_s:.3} s, ranks sum {sum:.6}, matches the edge-list reference",
        algo::PAGERANK_ITERATIONS
    );

    let t = Instant::now();
    let labels = algo::wcc(&mut db, &mut reader).expect("wcc");
    let wcc_s = t.elapsed().as_secs_f64();
    let mut parent: Vec<u32> = (0..n as u32).collect();
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            parent[x as usize] = parent[parent[x as usize] as usize];
            x = parent[x as usize];
        }
        x
    }
    for &(s, d) in edges {
        let (a, b) = (find(&mut parent, s), find(&mut parent, d));
        if a < b {
            parent[b as usize] = a;
        } else {
            parent[a as usize] = b;
        }
    }
    // The label of a component is the smallest person id in it, not
    // the smallest row: this file is keyed and its rows are in load
    // order, so the two are different numbers.
    let roots: Vec<u32> = (0..n as u32).map(|v| find(&mut parent, v)).collect();
    let mut smallest = vec![u64::MAX; n];
    for (row, &root) in roots.iter().enumerate() {
        let slot = &mut smallest[root as usize];
        *slot = (*slot).min(by_row[row]);
    }
    let reference: Vec<u64> = roots.iter().map(|&r| smallest[r as usize]).collect();
    assert_eq!(labels, reference, "wcc labels disagree with the reference");
    let components = roots
        .iter()
        .enumerate()
        .filter(|(v, r)| **r == *v as u32)
        .count();
    println!(
        "sf1 wcc: {components} components in {wcc_s:.3} s, labels match the edge-list reference"
    );

    let t = Instant::now();
    let dist = algo::sssp(&mut db, &mut reader, 0).expect("sssp");
    let sssp_s = t.elapsed().as_secs_f64();
    let mut adj = vec![Vec::new(); n];
    for &(s, d) in edges {
        adj[s as usize].push(d);
        adj[d as usize].push(s);
    }
    let mut reference = vec![u64::MAX; n];
    reference[0] = 0;
    let mut frontier = vec![0u32];
    let mut depth = 0u64;
    while !frontier.is_empty() {
        depth += 1;
        let mut next = Vec::new();
        for &v in &frontier {
            for &w in &adj[v as usize] {
                if reference[w as usize] == u64::MAX {
                    reference[w as usize] = depth;
                    next.push(w);
                }
            }
        }
        frontier = next;
    }
    assert_eq!(
        dist, reference,
        "sssp distances disagree with the BFS reference"
    );
    let reached = dist.iter().filter(|&&d| d != u64::MAX).count();
    println!(
        "sf1 sssp: reached {reached} of {n} from row 0 in {sssp_s:.3} s, matches the edge-list BFS"
    );

    let t = Instant::now();
    let communities = algo::louvain(&mut db, &mut reader).expect("louvain");
    let louvain_s = t.elapsed().as_secs_f64();
    let again = algo::louvain(&mut db, &mut reader).expect("louvain again");
    assert_eq!(communities, again, "louvain is not deterministic");
    let count = communities
        .iter()
        .enumerate()
        .filter(|(v, c)| **c == *v as u64)
        .count();
    println!("sf1 louvain: {count} communities in {louvain_s:.3} s, deterministic across two runs");

    let t = Instant::now();
    let r = zu::query::run(
        "CALL pagerank('knows') YIELD node, rank RETURN count(node) AS n, sum(rank) AS total",
        &mut db,
        &[],
    )
    .expect("CALL pagerank");
    assert_eq!(r.rows[0][0], Value::Int(node_count as i64));
    let Value::Float(total) = r.rows[0][1] else {
        panic!("expected a float rank sum");
    };
    assert!((total - 1.0).abs() < 1e-9, "CALL rank sum {total}");
    println!(
        "sf1 CALL pagerank through zuQL: {node_count} rows aggregated in {:.3} s",
        t.elapsed().as_secs_f64()
    );

    (pagerank_s, wcc_s, sssp_s, louvain_s)
}

/// Whether a phase runs. `ZU_ONLY=triangle` loads the graph and runs
/// that phase and nothing else, which is the difference between waiting
/// a minute and waiting five seconds between two attempts at a plan.
/// The names are hop, key, two-hop, triangle, ordered, close, is, ic,
/// distinct, cardinality and call, comma separated.
fn only(name: &str) -> bool {
    match std::env::var("ZU_ONLY") {
        Ok(want) => want.split(',').any(|w| w.trim() == name),
        Err(_) => true,
    }
}

fn main() {
    let gate = std::env::var("ZU_GATE").is_ok_and(|v| v == "1");
    // A run of one phase is not the gate, and a gate that quietly
    // measured one of eleven things is worse than no gate at all.
    assert!(
        !(gate && std::env::var("ZU_ONLY").is_ok()),
        "ZU_ONLY runs a phase at a time, so it cannot stand as the gate"
    );
    let data = match std::env::var("ZU_DATA") {
        Ok(d)
            if std::path::Path::new(&format!("{d}/ldbc-sf1-person-keys.txt")).exists()
                && std::path::Path::new(&format!("{d}/ldbc-sf1-knows.txt")).exists()
                && std::path::Path::new(&format!("{d}/ldbc-sf1-person-props.txt")).exists() =>
        {
            d
        }
        _ => {
            println!(
                "ldbc: SF1 files not found under ZU_DATA, run scripts/prep-ldbc.sh on this host"
            );
            // A gate that silently skips is not a gate.
            std::process::exit(i32::from(gate));
        }
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sf1.zu1");
    let (edges, by_row, profiles) = load(&data, &path);
    let node_count = by_row.len() as u64;

    let hop_p50 = only("hop").then(|| run_one_hop(&path, &edges, node_count));
    let key_p50 = only("key").then(|| run_key_lookups(&path, &by_row));
    let two_hop_p50 = only("two-hop").then(|| run_two_hop(&path, &edges, node_count));
    let triangle_p50 = only("triangle").then(|| run_triangle_count(&path, &edges, node_count));
    let ordered_p50 =
        only("ordered").then(|| run_ordered_triangle(&path, &edges, &by_row, node_count));
    let close_p50 = only("close").then(|| run_undirected_close(&path, &edges, node_count));
    let is_p50 = only("is").then(|| run_is_reads(&path, &by_row, &profiles));
    let ic_p50 = only("ic").then(|| run_ic_friends_of_friends(&path, &edges, &by_row, &profiles));
    let distinct_p50 = only("distinct").then(|| run_distinct_two_hop(&path, &edges, &by_row));
    let cardinality = only("cardinality").then(|| run_cardinality(&path, &by_row, &profiles));
    let kernels = only("call").then(|| run_table_functions(&path, &edges, &by_row, node_count));

    let (q50, q90, q99, qmax, violations) = cardinality.unwrap_or_default();
    let (pagerank_s, wcc_s, sssp_s, louvain_s) = kernels.unwrap_or_default();

    // A phase the filter skipped has no number to hold against its
    // ceiling, and the run it was left out of is not the gate anyway.
    let over = |label: &str, got: Option<f64>, key: &str, unit: &str| -> bool {
        let (Some(got), Some(ceiling)) = (got, budget(key)) else {
            return false;
        };
        if got <= ceiling {
            return false;
        }
        println!("GATE FAIL {label}: p50 {got:.3} {unit} > ceiling {ceiling}");
        true
    };
    let mut failed = over("B1 1-hop", hop_p50, "ldbc_hop_p50_us", "us");
    failed |= over("B2 key-lookup", key_p50, "ldbc_key_p50_us", "us");
    failed |= over("B4 2-hop", two_hop_p50, "ldbc_two_hop_p50_ms", "ms");
    failed |= over("triangle count", triangle_p50, "ldbc_triangle_p50_ms", "ms");
    failed |= over(
        "ordered triangle",
        ordered_p50,
        "ldbc_ordered_triangle_p50_ms",
        "ms",
    );
    failed |= over("undirected close", close_p50, "ldbc_close_p50_ms", "ms");
    failed |= over("IS profile read", is_p50, "ldbc_is_p50_ms", "ms");
    failed |= over("IC friends-of-friends", ic_p50, "ldbc_ic_p50_ms", "ms");
    failed |= over(
        "distinct two-hop",
        distinct_p50,
        "ldbc_distinct_two_hop_p50_ms",
        "ms",
    );
    // No budget line for this one. A ceiling the data walks straight
    // through is wrong, and there is no number of wrong ceilings worth
    // writing down as acceptable.
    if violations > 0 {
        println!("GATE FAIL cardinality: {violations} bound violations, ceilings must hold");
        failed = true;
    }
    for (name, got, key) in [
        ("p50", q50, "card_qerror_p50"),
        ("p90", q90, "card_qerror_p90"),
        ("p99", q99, "card_qerror_p99"),
        ("max", qmax, "card_qerror_max"),
    ] {
        if let Some(ceiling) = budget(key)
            && got > ceiling
        {
            println!("GATE FAIL cardinality q-error {name}: {got:.2} > ceiling {ceiling}");
            failed = true;
        }
    }
    for (name, secs, key) in [
        ("pagerank", pagerank_s, "ldbc_pagerank_s"),
        ("wcc", wcc_s, "ldbc_wcc_s"),
        ("sssp", sssp_s, "ldbc_sssp_s"),
        ("louvain", louvain_s, "ldbc_louvain_s"),
    ] {
        if let Some(ceiling) = budget(key)
            && secs > ceiling
        {
            println!("GATE FAIL {name}: {secs:.3} s > ceiling {ceiling}");
            failed = true;
        }
    }
    if gate && failed {
        std::process::exit(1);
    }
    if failed {
        println!("gate: informational run, set ZU_GATE=1 to enforce");
    } else {
        println!("gate: all ceilings met");
    }
}
