//! zuQL parity: one seeded graph loaded into zu1 and sqlite, one
//! corpus of queries run through both facades, every result compared
//! row for row. The raw differential corpus compares what readers see;
//! this layer compares what queries answer, so a divergence anywhere
//! in either engine's scan, expand, filter, or property path surfaces
//! as two different result sets for the same text. Both engines keep
//! the dense row contract, node offsets from zero, so ids compare
//! verbatim with no translation.

use zu::query::run as run_zu1;
use zu::sqlite::run as run_sqlite;
use zu_query::exec::Value;
use zu_sqlite::{ColumnType, SqliteStore, Value as SqlValue};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;
use zu_zu1::props::{PropValues, store_props};

/// splitmix64: deterministic, seedable, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const PERSONS: u64 = 200;

/// The same seeded graph in both engines: `PERSONS` people with an
/// age and a name, and a few edges per person.
fn seeded(dir: &std::path::Path) -> (Zu1File, SqliteStore) {
    let mut rng = Rng(0x5EED_0057);
    let ages: Vec<u64> = (0..PERSONS).map(|_| 18 + rng.below(60)).collect();
    let names: Vec<String> = (0..PERSONS).map(|i| format!("p{i:03}")).collect();
    let mut edges: Vec<(u32, u32)> = (0..PERSONS * 6)
        .map(|_| (rng.below(PERSONS) as u32, rng.below(PERSONS) as u32))
        .collect();
    edges.sort_unstable();
    edges.dedup();

    let mut zu = Zu1File::create(&dir.join("parity.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", PERSONS, &edges).unwrap();
    let name_refs: Vec<&[u8]> = names.iter().map(|n| n.as_bytes()).collect();
    store_props(
        &mut zu,
        "person",
        &[
            ("age", PropValues::Int(&ages)),
            ("name", PropValues::Str(&name_refs)),
        ],
    )
    .unwrap();

    let mut sq = SqliteStore::open(dir.join("parity.db")).unwrap();
    sq.create_node_table(
        "person",
        &[("age", ColumnType::Integer), ("name", ColumnType::Text)],
    )
    .unwrap();
    sq.create_rel_table("knows", "person", "person", &[])
        .unwrap();
    sq.begin().unwrap();
    for row in 0..PERSONS {
        sq.insert_node_at(
            "person",
            row as i64,
            &[
                SqlValue::Int(ages[row as usize] as i64),
                SqlValue::Text(names[row as usize].clone()),
            ],
        )
        .unwrap();
    }
    for &(src, dst) in &edges {
        sq.insert_rel("knows", i64::from(src), i64::from(dst), &[])
            .unwrap();
    }
    sq.commit().unwrap();
    (zu, sq)
}

#[test]
fn both_engines_answer_the_corpus_identically() {
    let dir = tempfile::tempdir().unwrap();
    let (mut zu, sq) = seeded(dir.path());
    let corpus: &[(&str, &[(&str, Value)])] = &[
        (
            "MATCH (a:person {id: $src})-[:knows]->(b) \
             RETURN b.id AS id, b.name AS name ORDER BY id",
            &[("src", Value::Int(7))],
        ),
        (
            "MATCH (a:person {id: $src})<-[:knows]-(b) RETURN b.id AS id ORDER BY id",
            &[("src", Value::Int(7))],
        ),
        (
            "MATCH (a:person {id: $src})-[:knows]-(b) RETURN count(b) AS n",
            &[("src", Value::Int(42))],
        ),
        (
            "MATCH (a:person)-[:knows]->(b)-[:knows]->(c) RETURN count(c) AS paths",
            &[],
        ),
        (
            "MATCH (a:person {id: $src})-[:knows*1..2]->(b) RETURN count(b) AS n",
            &[("src", Value::Int(3))],
        ),
        (
            "MATCH (a:person) WHERE a.age = $age RETURN a.id AS id ORDER BY id",
            &[("age", Value::Int(30))],
        ),
        (
            "MATCH (a:person) WHERE a.age >= $lo \
             RETURN count(a) AS n",
            &[("lo", Value::Int(60))],
        ),
        (
            "MATCH (a:person)-[:knows]->(b) \
             RETURN a.id AS a, count(*) AS deg ORDER BY deg DESC, a LIMIT 10",
            &[],
        ),
        (
            "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) WHERE b.age < $t \
             RETURN count(a) AS people, count(b) AS young",
            &[("t", Value::Int(25))],
        ),
        (
            "MATCH (a:person {id: $src})-[:knows]->(b) \
             RETURN b.name AS name, b.age AS age ORDER BY age, name",
            &[("src", Value::Int(11))],
        ),
        (
            "MATCH WALK (a:person {id: $src})-[:knows*3..3]->(b) RETURN count(b) AS n",
            &[("src", Value::Int(3))],
        ),
        (
            "MATCH ACYCLIC (a:person {id: $src})-[:knows*1..3]->(b) RETURN count(b) AS n",
            &[("src", Value::Int(3))],
        ),
        (
            "MATCH ANY SHORTEST (a:person {id: $src})-[r:knows*]->(b) \
             RETURN count(b) AS n, sum(size(r)) AS hops",
            &[("src", Value::Int(3))],
        ),
        (
            "MATCH ALL SHORTEST (a:person {id: $src})-[r:knows*]->(b) \
             RETURN count(b) AS paths, min(size(r)) AS nearest",
            &[("src", Value::Int(3))],
        ),
        (
            "MATCH p = ANY SHORTEST (a:person {id: $src})-[:knows*]->(b) \
             RETURN size(p) AS len, b.id AS id ORDER BY id LIMIT 20",
            &[("src", Value::Int(3))],
        ),
        // Raw node and rel values carry engine-assigned table ids, so
        // path comparisons go through sizes and endpoint ids.
        (
            "MATCH p = ACYCLIC (a:person {id: $src})-[:knows*1..2]->(b) \
             RETURN size(p) AS len, count(*) AS n ORDER BY len",
            &[("src", Value::Int(3))],
        ),
    ];
    for (source, params) in corpus {
        let z = run_zu1(source, &mut zu, params).unwrap();
        let s = run_sqlite(source, &sq, params).unwrap();
        assert_eq!(z.columns, s.columns, "columns diverged on: {source}");
        assert_eq!(z.rows, s.rows, "rows diverged on: {source}");
        assert!(
            !z.rows.is_empty(),
            "corpus query answered nothing, no coverage: {source}"
        );
    }
}
