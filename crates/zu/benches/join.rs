//! The value join (perf/05 section 3, zu#76): two patterns that share
//! no variable, tied by an equality on a property.
//!
//! The old engine answers this with a nested loop. It picks one pattern
//! to drive, and for every row of it scans the whole of the other and
//! compares. That is rows times rows, and on a table of any size it is
//! not a plan, it is a way of not answering.
//!
//! What the pipeline does instead is read the held pattern into a hash
//! table once, at compile time, and probe it a row at a time from the
//! driving side. The build is one pass over one column and the probe is
//! a hash and a tag compare, so the cost stops being quadratic and
//! becomes a pass over each side plus the rows that actually match.
//!
//! The build happens per execution, since nothing caches a compiled
//! plan yet, so every number here pays for it. That is deliberate: it
//! is what a caller pays today, and hiding it behind a warm table would
//! measure something nobody runs.
//!
//! The table hands the rows for a key back in build order, which is
//! table order, and that is what makes the answer match the old engine
//! row for row rather than just as a set.
//!
//! Nine shapes. A unique build key, where every probe finds exactly
//! one row and the table is at its widest. A key with a thousand rows
//! under it, where the probe side is small and the cost is streaming
//! payload. A probe that misses everything, which is the tag doing its
//! job and the payload never being touched. A group by a column of the
//! joined side, which reads the level the join built. A hop off that
//! level, which is the join feeding a walk. Two left joins, an
//! OPTIONAL MATCH tied by an equality, one where every probe hits and
//! one where every other probe misses and the outer row goes on with a
//! null bound to it. The miss case is the one that would halve its own
//! answer if a miss were dropped instead of kept.
//!
//! The last two are the sideways pass of perf/13 section 1, and each
//! is timed twice, once with the join publishing what it knows about
//! its keys to the level it probes and once with ZU_SIP=0 holding it
//! back. One is the membership test: the build keys are eight apart,
//! which is dense enough for the exact filter, so seven probe rows in
//! eight are dropped before the probe reads them. The other is the
//! range going down into the scan: the build side's keys are city
//! numbers and the probe's climb with the row, so every chunk but the
//! first sits outside the range and is skipped without being decoded.
//!
//! What those two ratios do not show yet is the pass at its best. The
//! join builds the side the optimizer estimated dearer and drives the
//! cheaper one, so the filter is published from the big side onto the
//! small one, which is the direction with the least to gain, and the
//! build of the big side is what is left in the number either way.
//! Turning that around is the join side choice, not this, and it is
//! the next thing on zu#76. The other half of it is the scan taking
//! the filter rather than an operator reading rows the scan has
//! already decoded, which is what would make an inexact filter worth
//! running at all.
//!
//! Every case is crosschecked against the generators, so a join that
//! loses rows, repeats them or lands them on the wrong key fails here.
//!
//! The old engine is timed on the same query with the driving side cut
//! to a few hundred rows, and reported per probe row, since the full
//! shape is a trillion comparisons and would not finish.
//!
//! Everything runs at one worker, so the rate is per core.
//!
//! exec_join_mprobes_s_core floors the unique key case in millions of
//! probe rows a second, build included. exec_sip_range_x and
//! exec_sip_mask_x floor the two sideways cases against their own
//! filter off runs; both sit under one, because neither half wins on
//! every host and those two only have to catch a filter that started
//! costing. exec_sip_best_x is the one above one: the better of the
//! two shapes on the host in front of it, which is the claim that the
//! pass is worth running at all.
//!
//! Run: ZU_GATE=1 cargo bench -p zu --bench join

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
/// Rows of the driving side the old engine is timed over. Each one
/// costs it a scan of the whole table, so this is already seconds.
const OLD_PROBES: u64 = 200;

/// A permutation of the ids: 7919 is prime and the table size is a
/// product of twos and fives, so every probe lands on exactly one row
/// and none of them lands twice.
fn pair(i: u64) -> u64 {
    (i * 7919) % NODES
}

fn city(i: u64) -> u64 {
    (i * 31) % CITIES
}

/// How far apart the sparse build keys sit, and how many of the probe
/// side's rows therefore land on one. Eight is inside the point where
/// a bitmap is still the cheaper filter, so this build side publishes
/// an exact one and one probe row in eight survives it.
const SPREAD: u64 = 8;

/// A key sparse enough that a filter over it rejects nearly every probe
/// row, and unique, so a survivor matches exactly one build row and the
/// work after the filter is the answer rather than the payload.
fn sparse(i: u64) -> u64 {
    i * SPREAD
}

/// A key no row carries, so every probe of it is a miss.
fn miss(i: u64) -> u64 {
    i + 2 * NODES
}

/// Half the rows carry a key that is there and half carry one that is
/// not, so a left join over this misses every other outer row.
fn half(i: u64) -> u64 {
    if i.is_multiple_of(2) { i } else { miss(i) }
}

/// The row whose pair is `i`, which is what a probe on the dense id
/// lands on.
fn inverse() -> Vec<u64> {
    let mut inv = vec![0u64; NODES as usize];
    for i in 0..NODES {
        inv[pair(i) as usize] = i;
    }
    inv
}

/// Out degree zero to seven by row, the same skew the other exec
/// benches walk.
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

/// One case: the query the pipeline runs, the cut down one the old
/// engine runs, and what each has to answer.
struct Case {
    what: &'static str,
    new: String,
    old: String,
    want: Want,
    old_want: Want,
    probes: u64,
    out: u64,
    /// Which half of the sideways pass this shape is here to measure,
    /// if it is here for that at all. A shape carrying one of the two
    /// runs a second time with the filter held back, so the line says
    /// what the filter was worth against the same plan. On the rest the
    /// two runs are the same plan and the line would say nothing.
    sip: Sip,
}

/// The two halves get their own floors because they are worth
/// different things. The range is a win on every host, since a chunk
/// it rules out is a chunk nobody decodes. The mask saves one probe
/// per rejected row, so it is a win where a random read costs
/// something and a wash where the whole table sits in cache.
#[derive(PartialEq)]
enum Sip {
    No,
    Mask,
    Range,
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("join.zu1");
    let t = Instant::now();
    let edges = edges();
    let mut db = Zu1File::create(&path).expect("create");
    bulk_load_as(&mut db, "person", "knows", NODES, &edges).expect("load");
    let pairs: Vec<u64> = (0..NODES).map(pair).collect();
    let cities: Vec<u64> = (0..NODES).map(city).collect();
    let misses: Vec<u64> = (0..NODES).map(miss).collect();
    let halves: Vec<u64> = (0..NODES).map(half).collect();
    let sparses: Vec<u64> = (0..NODES).map(sparse).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("pair", PropValues::Int(&pairs)),
            ("city", PropValues::Int(&cities)),
            ("far", PropValues::Int(&misses)),
            ("half", PropValues::Int(&halves)),
            ("sparse", PropValues::Int(&sparses)),
        ],
    )
    .expect("props");
    drop(db);
    let mut db = Zu1File::open(&path).expect("open");
    // SAFETY: the bench main is single threaded here; workers only
    // spawn inside the timed calls below.
    unsafe { std::env::set_var("ZU_THREADS", "1") };

    // The answers, off the generators. The unique key is a
    // permutation of the ids, so it is one row a probe and every row
    // of the held side is landed on exactly once. The city key is a
    // thousand rows a key, and the driving side is cut to the first
    // thousand ids, whose cities are themselves a permutation of the
    // thousand.
    let inv = inverse();
    let mut per_city: Vec<u64> = vec![0; CITIES as usize];
    for i in 0..NODES {
        per_city[city(i) as usize] += 1;
    }
    let mut out_degree: Vec<u64> = vec![0; NODES as usize];
    for &(s, _) in &edges {
        out_degree[s as usize] += 1;
    }
    let unique_out = NODES;
    let city_probes = CITIES;
    let city_out: u64 = (0..city_probes).map(|i| per_city[city(i) as usize]).sum();
    // The hop walks off the joined side, and the probe lands on every
    // row of it, so its rows are the whole edge list.
    let hop_out = edges.len() as u64;
    let cities_hit = per_city.iter().filter(|&&n| n > 0).count() as u64;
    // The same three, over the slice of the driving side the old
    // engine is timed on.
    let old_city_out: u64 = (0..OLD_PROBES).map(|i| per_city[city(i) as usize]).sum();
    let old_hop_out: u64 = (0..OLD_PROBES)
        .map(|i| out_degree[inv[i as usize] as usize])
        .sum();
    // The sparse key is one build row per key, a spread apart, so a
    // probe row survives exactly when its pair is a multiple of the
    // spread.
    let sparse_hit = |i: u64| pair(i).is_multiple_of(SPREAD);
    let sparse_out = (0..NODES).filter(|&i| sparse_hit(i)).count() as u64;
    let old_sparse_out = (0..OLD_PROBES).filter(|&i| sparse_hit(i)).count() as u64;
    // The sparse key against the city key: a probe row matches when its
    // own key is a city, and the keys climb a spread at a time, so the
    // only rows low enough are the first few of the table. Each of
    // them matches every row carrying that city. Both engines answer
    // the same number, since the old engine's cut down driving side
    // already holds all the rows that match.
    let zone_out: u64 = (0..NODES)
        .filter(|&i| sparse(i) < CITIES)
        .map(|i| per_city[sparse(i) as usize])
        .sum();
    let old_towns = (0..OLD_PROBES)
        .map(|i| city(inv[i as usize]))
        .collect::<std::collections::BTreeSet<_>>()
        .len() as u64;
    println!(
        "join: {NODES} persons, {} edges, {unique_out} unique key rows, {city_out} city key rows, \
         {hop_out} hop rows, load {:.1}s",
        edges.len(),
        t.elapsed().as_secs_f64()
    );

    let cases = [
        Case {
            what: "unique key",
            new: "MATCH (a:person), (b:person) WHERE a.id = b.pair RETURN count(*) AS n".into(),
            old: format!(
                "MATCH (a:person), (b:person) WHERE a.id < {OLD_PROBES} AND a.id = b.pair \
                 RETURN count(*) AS n"
            ),
            want: Want {
                rows: 1,
                total: unique_out as i64,
            },
            old_want: Want {
                rows: 1,
                total: OLD_PROBES as i64,
            },
            probes: NODES,
            out: unique_out,
            sip: Sip::No,
        },
        Case {
            what: "thousand a key",
            new: format!(
                "MATCH (a:person), (b:person) WHERE a.id < {city_probes} AND a.city = b.city \
                 RETURN count(*) AS n"
            ),
            old: format!(
                "MATCH (a:person), (b:person) WHERE a.id < {OLD_PROBES} AND a.city = b.city \
                 RETURN count(*) AS n"
            ),
            want: Want {
                rows: 1,
                total: city_out as i64,
            },
            old_want: Want {
                rows: 1,
                total: old_city_out as i64,
            },
            probes: city_probes,
            out: city_out,
            sip: Sip::No,
        },
        Case {
            what: "every probe misses",
            new: "MATCH (a:person), (b:person) WHERE a.far = b.pair RETURN count(*) AS n".into(),
            old: format!(
                "MATCH (a:person), (b:person) WHERE a.id < {OLD_PROBES} AND a.far = b.pair \
                 RETURN count(*) AS n"
            ),
            want: Want { rows: 1, total: 0 },
            old_want: Want { rows: 1, total: 0 },
            probes: NODES,
            out: 0,
            sip: Sip::No,
        },
        Case {
            what: "grouped by the joined side",
            new: "MATCH (a:person), (b:person) WHERE a.id = b.pair \
                  RETURN b.city AS town, count(*) AS n"
                .into(),
            old: format!(
                "MATCH (a:person), (b:person) WHERE a.id < {OLD_PROBES} AND a.id = b.pair \
                 RETURN b.city AS town, count(*) AS n"
            ),
            want: Want {
                rows: cities_hit,
                total: unique_out as i64,
            },
            old_want: Want {
                rows: old_towns,
                total: OLD_PROBES as i64,
            },
            probes: NODES,
            out: unique_out,
            sip: Sip::No,
        },
        Case {
            what: "hop off the joined side",
            new: "MATCH (a:person), (b:person)-[:knows]->(c) WHERE a.id = b.pair \
                  RETURN count(*) AS n"
                .into(),
            old: format!(
                "MATCH (a:person), (b:person)-[:knows]->(c) WHERE a.id < {OLD_PROBES} \
                 AND a.id = b.pair RETURN count(*) AS n"
            ),
            want: Want {
                rows: 1,
                total: hop_out as i64,
            },
            old_want: Want {
                rows: 1,
                total: old_hop_out as i64,
            },
            probes: NODES,
            out: hop_out,
            sip: Sip::No,
        },
        Case {
            what: "left join, every probe hits",
            new: "MATCH (a:person) OPTIONAL MATCH (b:person) WHERE b.pair = a.id \
                  RETURN count(*) AS n"
                .into(),
            old: format!(
                "MATCH (a:person) WHERE a.id < {OLD_PROBES} OPTIONAL MATCH (b:person) \
                 WHERE b.pair = a.id RETURN count(*) AS n"
            ),
            want: Want {
                rows: 1,
                total: NODES as i64,
            },
            old_want: Want {
                rows: 1,
                total: OLD_PROBES as i64,
            },
            probes: NODES,
            out: NODES,
            sip: Sip::No,
        },
        Case {
            what: "left join, every other probe misses",
            new: "MATCH (a:person) OPTIONAL MATCH (b:person) WHERE b.pair = a.half \
                  RETURN count(*) AS n"
                .into(),
            old: format!(
                "MATCH (a:person) WHERE a.id < {OLD_PROBES} OPTIONAL MATCH (b:person) \
                 WHERE b.pair = a.half RETURN count(*) AS n"
            ),
            // The outer rows survive either way, so a miss that got
            // dropped would halve this.
            want: Want {
                rows: 1,
                total: NODES as i64,
            },
            old_want: Want {
                rows: 1,
                total: OLD_PROBES as i64,
            },
            probes: NODES,
            out: NODES,
            sip: Sip::No,
        },
        Case {
            what: "one probe row in eight survives",
            new: "MATCH (a:person), (b:person) WHERE a.pair = b.sparse RETURN count(*) AS n".into(),
            old: format!(
                "MATCH (a:person), (b:person) WHERE a.id < {OLD_PROBES} AND a.pair = b.sparse \
                 RETURN count(*) AS n"
            ),
            want: Want {
                rows: 1,
                total: sparse_out as i64,
            },
            old_want: Want {
                rows: 1,
                total: old_sparse_out as i64,
            },
            probes: NODES,
            out: sparse_out,
            sip: Sip::Mask,
        },
        Case {
            what: "the build side's keys fit in one chunk of the probe",
            new: "MATCH (a:person), (b:person) WHERE a.sparse = b.city RETURN count(*) AS n".into(),
            old: format!(
                "MATCH (a:person), (b:person) WHERE a.id < {OLD_PROBES} AND a.sparse = b.city \
                 RETURN count(*) AS n"
            ),
            want: Want {
                rows: 1,
                total: zone_out as i64,
            },
            old_want: Want {
                rows: 1,
                total: zone_out as i64,
            },
            probes: NODES,
            out: zone_out,
            sip: Sip::Range,
        },
    ];

    let mut unique = 0.0;
    let mut worst_mask = f64::MAX;
    let mut worst_range = f64::MAX;
    for case in cases {
        let new = measure(&mut db, &case.new, &case.want, 5);
        // The same plan with the join's filter withheld, which is the
        // baseline the sideways pass is worth measuring against: same
        // rows, same order, one less thing known.
        let off = (case.sip != Sip::No).then(|| {
            // SAFETY: same as the worker count above.
            unsafe { std::env::set_var("ZU_SIP", "0") };
            let ms = measure(&mut db, &case.new, &case.want, 5);
            unsafe { std::env::remove_var("ZU_SIP") };
            ms
        });
        // SAFETY: same as the worker count above.
        unsafe { std::env::set_var("ZU_EXEC2", "0") };
        let old = measure(&mut db, &case.old, &case.old_want, 1);
        unsafe { std::env::remove_var("ZU_EXEC2") };
        let mprobes = case.probes as f64 / 1e3 / new;
        let new_us = new * 1e3 / case.probes as f64;
        let old_us = old * 1e3 / OLD_PROBES as f64;
        let sip = match off {
            Some(off) => {
                let x = off / new;
                match case.sip {
                    Sip::Mask => worst_mask = worst_mask.min(x),
                    Sip::Range => worst_range = worst_range.min(x),
                    Sip::No => unreachable!("only a sip case times twice"),
                }
                format!(", filter off {off:.1} ms {x:.1}x")
            }
            None => String::new(),
        };
        println!(
            "join {}: {new:.1} ms {mprobes:.2} M probes/s {:.1} M rows out, {new_us:.3} us/probe, \
             old engine {old_us:.1} us/probe over {OLD_PROBES} probes, {:.0}x per probe{sip}, \
             crosschecked",
            case.what,
            case.out as f64 / 1e6,
            old_us / new_us
        );
        if case.what == "unique key" {
            unique = mprobes;
        }
    }

    if std::env::var("ZU_GATE").as_deref() != Ok("1") {
        return;
    }
    if let Some(floor) = budget("exec_join_mprobes_s_core") {
        assert!(
            unique >= floor,
            "the unique key join probes {unique:.2} M rows/s, under the {floor} M floor"
        );
        println!("gate: join floor met");
    }
    if let Some(floor) = budget("exec_sip_range_x") {
        assert!(
            worst_range >= floor,
            "the published range leaves the query {worst_range:.2}x, \
             under the {floor}x floor"
        );
        println!("gate: sip range floor met");
    }
    if let Some(floor) = budget("exec_sip_mask_x") {
        assert!(
            worst_mask >= floor,
            "the membership test leaves the query {worst_mask:.2}x, \
             under the {floor}x floor"
        );
        println!("gate: sip mask floor met");
    }
    // Neither half wins on every host, but one of them always does, and
    // that is the claim worth gating. Which one it is says where the
    // host's time goes: the range wins where decoding costs, the mask
    // wins where a random read costs.
    if let Some(floor) = budget("exec_sip_best_x") {
        let best = worst_range.max(worst_mask);
        assert!(
            best >= floor,
            "the better half of the sideways pass leaves the query {best:.2}x, \
             under the {floor}x floor"
        );
        println!("gate: sip best floor met");
    }
}
