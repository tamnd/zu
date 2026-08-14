//! The openCypher TCK subset scoreboard. Each scenario is a query in
//! the v0 grammar with its expected rows worked out by hand on a six
//! node fixture small enough to check by eye. Every scenario runs on
//! zu1 and on sqlite through the shared facades; an engine passes a
//! scenario when its rows match the expectation verbatim, and the two
//! engines hold parity when they return identical rows whether or not
//! either matches. The rendered scoreboard is checked into the repo at
//! `docs/tck-scoreboard.md`; this test regenerates it and fails on
//! drift, and `ZU_UPDATE_SCOREBOARD=1` rewrites the file when the
//! subset grows.
//!
//! The fixture: persons 0..5 named ada, bob, cat, dan, eve, fay with
//! ages 20, 30, 30, 40, 50, 25, and knows edges (0,1) (0,2) (1,3)
//! (2,3) (2,5) (3,4) (4,0).

use zu::query::run as run_zu1;
use zu::sqlite::run as run_sqlite;
use zu_query::exec::Value;
use zu_sqlite::{ColumnType, SqliteStore, Value as SqlValue};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;
use zu_zu1::props::{PropValues, store_props};

const NAMES: [&str; 6] = ["ada", "bob", "cat", "dan", "eve", "fay"];
const AGES: [u64; 6] = [20, 30, 30, 40, 50, 25];
const EDGES: [(u32, u32); 7] = [(0, 1), (0, 2), (1, 3), (2, 3), (2, 5), (3, 4), (4, 0)];

fn seeded(dir: &std::path::Path) -> (Zu1File, SqliteStore) {
    let mut zu = Zu1File::create(&dir.join("tck.zu1")).unwrap();
    bulk_load_as(&mut zu, "person", "knows", 6, &EDGES).unwrap();
    let names: Vec<&[u8]> = NAMES.iter().map(|n| n.as_bytes()).collect();
    store_props(
        &mut zu,
        "person",
        &[
            ("age", PropValues::Int(&AGES)),
            ("name", PropValues::Str(&names)),
        ],
    )
    .unwrap();

    let mut sq = SqliteStore::open(dir.join("tck.db")).unwrap();
    sq.create_node_table(
        "person",
        &[("age", ColumnType::Integer), ("name", ColumnType::Text)],
    )
    .unwrap();
    sq.create_rel_table("knows", "person", "person", &[])
        .unwrap();
    sq.begin().unwrap();
    for row in 0..6usize {
        sq.insert_node_at(
            "person",
            row as i64,
            &[
                SqlValue::Int(AGES[row] as i64),
                SqlValue::Text(NAMES[row].to_owned()),
            ],
        )
        .unwrap();
    }
    for &(src, dst) in &EDGES {
        sq.insert_rel("knows", i64::from(src), i64::from(dst), &[])
            .unwrap();
    }
    sq.commit().unwrap();
    (zu, sq)
}

struct Scenario {
    category: &'static str,
    name: &'static str,
    query: &'static str,
    expected: Vec<Vec<Value>>,
}

fn int_rows(ids: &[i64]) -> Vec<Vec<Value>> {
    ids.iter().map(|&i| vec![Value::Int(i)]).collect()
}

fn str_rows(names: &[&str]) -> Vec<Vec<Value>> {
    names.iter().map(|&n| vec![Value::Str(n.into())]).collect()
}

fn scenarios() -> Vec<Scenario> {
    let s = |category, name, query, expected| Scenario {
        category,
        name,
        query,
        expected,
    };
    vec![
        s(
            "match",
            "count all nodes",
            "MATCH (a:person) RETURN count(a) AS n",
            int_rows(&[6]),
        ),
        s(
            "match",
            "scan ordered by property",
            "MATCH (a:person) RETURN a.name AS name ORDER BY name",
            str_rows(&NAMES),
        ),
        s(
            "match",
            "expand along direction",
            "MATCH (a:person {id: 0})-[:knows]->(b) RETURN b.id AS id ORDER BY id",
            int_rows(&[1, 2]),
        ),
        s(
            "match",
            "expand against direction",
            "MATCH (a:person {id: 3})<-[:knows]-(b) RETURN b.id AS id ORDER BY id",
            int_rows(&[1, 2]),
        ),
        s(
            "match",
            "undirected expand",
            "MATCH (a:person {id: 0})-[:knows]-(b) RETURN b.id AS id ORDER BY id",
            int_rows(&[1, 2, 4]),
        ),
        s(
            "match",
            "two hop paths",
            "MATCH (a:person {id: 0})-[:knows]->(b)-[:knows]->(c) \
             RETURN c.id AS id ORDER BY id",
            int_rows(&[3, 3, 5]),
        ),
        s(
            "match",
            "property through expand",
            "MATCH (a:person {id: 4})-[:knows]->(b) RETURN b.name AS name",
            str_rows(&["ada"]),
        ),
        s(
            "where",
            "equality on a property",
            "MATCH (a:person) WHERE a.age = 30 RETURN a.id AS id ORDER BY id",
            int_rows(&[1, 2]),
        ),
        s(
            "where",
            "range filter descending",
            "MATCH (a:person) WHERE a.age >= 30 RETURN a.id AS id ORDER BY id DESC",
            int_rows(&[4, 3, 2, 1]),
        ),
        s(
            "where",
            "conjunction",
            "MATCH (a:person) WHERE a.age >= 25 AND a.age < 40 \
             RETURN a.name AS name ORDER BY name",
            str_rows(&["bob", "cat", "fay"]),
        ),
        s(
            "where",
            "disjunction",
            "MATCH (a:person) WHERE a.name = 'ada' OR a.age = 50 \
             RETURN a.id AS id ORDER BY id",
            int_rows(&[0, 4]),
        ),
        s(
            "where",
            "compare across the pattern",
            "MATCH (a:person)-[:knows]->(b) WHERE a.age < b.age RETURN count(*) AS n",
            int_rows(&[5]),
        ),
        s(
            "where",
            "not equals",
            "MATCH (a:person) WHERE a.age <> 30 RETURN count(a) AS n",
            int_rows(&[4]),
        ),
        s(
            "optional",
            "unmatched row keeps null",
            "MATCH (a:person {id: 5}) OPTIONAL MATCH (a)-[:knows]->(b) \
             RETURN a.id AS id, b.id AS friend",
            vec![vec![Value::Int(5), Value::Null]],
        ),
        s(
            "optional",
            "counts over optional rows",
            "MATCH (a:person) OPTIONAL MATCH (a)-[:knows]->(b) \
             RETURN count(a) AS people, count(b) AS friends",
            vec![vec![Value::Int(8), Value::Int(7)]],
        ),
        s(
            "exists",
            "pattern predicate keeps the rows that match",
            "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) WHERE b.age > 35 } \
             RETURN a.id AS id ORDER BY id",
            int_rows(&[1, 2, 3]),
        ),
        s(
            "exists",
            "a matching row arrives once however many matched",
            "MATCH (a:person) WHERE EXISTS { MATCH (a)-[:knows]->(b) } \
             RETURN count(a) AS n",
            int_rows(&[5]),
        ),
        s(
            "exists",
            "negated pattern predicate keeps the rest",
            "MATCH (a:person) WHERE NOT EXISTS { MATCH (a)-[:knows]->(b) } \
             RETURN a.id AS id ORDER BY id",
            int_rows(&[5]),
        ),
        s(
            "aggregation",
            "grouped degree",
            "MATCH (a:person)-[:knows]->(b) \
             RETURN a.id AS id, count(*) AS deg ORDER BY deg DESC, id",
            vec![
                vec![Value::Int(0), Value::Int(2)],
                vec![Value::Int(2), Value::Int(2)],
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(3), Value::Int(1)],
                vec![Value::Int(4), Value::Int(1)],
            ],
        ),
        s(
            "aggregation",
            "sum",
            "MATCH (a:person) RETURN sum(a.age) AS total",
            int_rows(&[195]),
        ),
        s(
            "aggregation",
            "min and max",
            "MATCH (a:person) RETURN min(a.age) AS lo, max(a.age) AS hi",
            vec![vec![Value::Int(20), Value::Int(50)]],
        ),
        s(
            "aggregation",
            "avg",
            "MATCH (a:person) RETURN avg(a.age) AS mean",
            vec![vec![Value::Float(32.5)]],
        ),
        s(
            "aggregation",
            "distinct ages over expand",
            "MATCH (a:person)-[:knows]->(b) RETURN count(DISTINCT b.age) AS n",
            int_rows(&[5]),
        ),
        s(
            "orderby",
            "compound order with limit",
            "MATCH (a:person) RETURN a.name AS name ORDER BY a.age DESC, name LIMIT 3",
            str_rows(&["eve", "dan", "bob"]),
        ),
        s(
            "orderby",
            "skip past a prefix",
            "MATCH (a:person) RETURN a.name AS name ORDER BY name SKIP 4",
            str_rows(&["eve", "fay"]),
        ),
        s(
            "varlength",
            "one to two hops",
            "MATCH (a:person {id: 0})-[:knows*1..2]->(b) RETURN count(b) AS n",
            int_rows(&[5]),
        ),
        s(
            "varlength",
            "exactly two hops distinct",
            "MATCH (a:person {id: 0})-[:knows*2..2]->(b) \
             RETURN DISTINCT b.id AS id ORDER BY id",
            int_rows(&[3, 5]),
        ),
        s(
            "with",
            "aggregate then filter",
            "MATCH (a:person)-[:knows]->(b) WITH a, count(b) AS deg WHERE deg > 1 \
             RETURN a.id AS id ORDER BY id",
            int_rows(&[0, 2]),
        ),
        s(
            "with",
            "distinct passthrough",
            "MATCH (a:person) WITH a.age AS age RETURN DISTINCT age ORDER BY age",
            int_rows(&[20, 25, 30, 40, 50]),
        ),
        s(
            "unwind",
            "literal list",
            "UNWIND [3, 1, 2] AS x RETURN x AS x ORDER BY x",
            int_rows(&[1, 2, 3]),
        ),
        s(
            "null",
            "is null projects a boolean",
            "MATCH (a:person {id: 5}) OPTIONAL MATCH (a)-[:knows]->(b) \
             RETURN b.id IS NULL AS missing",
            vec![vec![Value::Bool(true)]],
        ),
    ]
}

struct Outcome {
    category: &'static str,
    name: &'static str,
    zu1: bool,
    sqlite: bool,
    parity: bool,
}

fn render(outcomes: &[Outcome]) -> String {
    let mut out = String::from(
        "# openCypher TCK subset scoreboard\n\n\
         Generated by `crates/zu/tests/tck.rs`, which fails on drift; run it with\n\
         `ZU_UPDATE_SCOREBOARD=1` to rewrite this file when the subset grows.\n\
         Every scenario runs on both engines through the shared query facades;\n\
         parity means the engines returned identical rows.\n\n\
         | category | scenario | zu1 | sqlite | parity |\n\
         |---|---|---|---|---|\n",
    );
    let mark = |ok: bool| if ok { "pass" } else { "fail" };
    for o in outcomes {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            o.category,
            o.name,
            mark(o.zu1),
            mark(o.sqlite),
            mark(o.parity)
        ));
    }
    let count = |f: fn(&Outcome) -> bool| outcomes.iter().filter(|o| f(o)).count();
    out.push_str(&format!(
        "\n{} scenarios: zu1 {}/{0}, sqlite {}/{0}, parity {}/{0}.\n",
        outcomes.len(),
        count(|o| o.zu1),
        count(|o| o.sqlite),
        count(|o| o.parity),
    ));
    out
}

#[test]
fn tck_subset_scoreboard_holds() {
    let dir = tempfile::tempdir().unwrap();
    let (mut zu, sq) = seeded(dir.path());
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();
    for sc in scenarios() {
        let z = run_zu1(sc.query, &mut zu, &[]).unwrap();
        let s = run_sqlite(sc.query, &sq, &[]).unwrap();
        let outcome = Outcome {
            category: sc.category,
            name: sc.name,
            zu1: z.rows == sc.expected,
            sqlite: s.rows == sc.expected,
            parity: z.rows == s.rows,
        };
        if !(outcome.zu1 && outcome.sqlite && outcome.parity) {
            failures.push(format!(
                "{} / {}: zu1 {:?}, sqlite {:?}, expected {:?}",
                sc.category, sc.name, z.rows, s.rows, sc.expected
            ));
        }
        outcomes.push(outcome);
    }
    let rendered = render(&outcomes);
    assert!(
        failures.is_empty(),
        "scoreboard regressions:\n{}",
        failures.join("\n")
    );

    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/tck-scoreboard.md");
    if std::env::var_os("ZU_UPDATE_SCOREBOARD").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    // A Windows checkout may carry CRLF from git's autocrlf; the
    // comparison is about content, not line endings.
    let checked_in = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        checked_in, rendered,
        "docs/tck-scoreboard.md is stale; rerun with ZU_UPDATE_SCOREBOARD=1"
    );
}
