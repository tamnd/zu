//! Engine conversion round trips: the same graph converted zu1 to
//! sqlite to zu1 and sqlite to zu1 to sqlite, compared logically at
//! both hops. Losslessness means every node table, property value,
//! and adjacency list survives, and a query answers identically on
//! the original and the twice-converted store.

use zu::convert::{sqlite_to_zu1, zu1_to_sqlite};
use zu::query::run as run_zu1;
use zu::sqlite::run as run_sqlite;
use zu_sqlite::{ColumnType, SqliteStore, Value as SqlValue};
use zu_storage::Direction;
use zu_zu1::catalog::Catalog;
use zu_zu1::file::Zu1File;
use zu_zu1::graph::{Direction as Zu1Direction, GraphReader, bulk_load_as};
use zu_zu1::props::{PropValues, PropsReader, load_props, store_props};

const NAMES: [&str; 6] = ["ada", "bob", "cat", "dan", "eve", "fay"];
const AGES: [u64; 6] = [20, 30, 30, 40, 50, 25];
const EDGES: [(u32, u32); 7] = [(0, 1), (0, 2), (1, 3), (2, 3), (2, 5), (3, 4), (4, 0)];

fn seed_zu1(path: &std::path::Path) {
    let mut zu = Zu1File::create(path).unwrap();
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
}

fn seed_sqlite(path: &std::path::Path) {
    let mut sq = SqliteStore::open(path).unwrap();
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
}

/// Asserts the zu1 file at `path` holds exactly the fixture graph.
fn assert_zu1_matches(path: &std::path::Path) {
    let mut zu = Zu1File::open(path).unwrap();
    let catalog = Catalog::load(&mut zu).unwrap();
    let person = catalog.node_by_name("person").expect("person survives");
    assert_eq!(person.node_count, 6);
    let knows = catalog.rel_by_name("knows").expect("knows survives");
    assert_eq!(knows.edge_count, EDGES.len() as u64);
    assert_eq!(knows.from, person.id);
    assert_eq!(knows.to, person.id);

    let mut g = GraphReader::load_table(&mut zu, "knows").unwrap();
    for node in 0..6u64 {
        let fwd: Vec<u64> = g
            .neighbors_dir(&mut zu, node, Zu1Direction::Fwd)
            .unwrap()
            .to_vec();
        let want: Vec<u64> = EDGES
            .iter()
            .filter(|&&(s, _)| u64::from(s) == node)
            .map(|&(_, d)| u64::from(d))
            .collect();
        assert_eq!(fwd, want, "forward neighbors of {node}");
        let bwd: Vec<u64> = g
            .neighbors_dir(&mut zu, node, Zu1Direction::Bwd)
            .unwrap()
            .to_vec();
        let mut want: Vec<u64> = EDGES
            .iter()
            .filter(|&&(_, d)| u64::from(d) == node)
            .map(|&(s, _)| u64::from(s))
            .collect();
        want.sort_unstable();
        assert_eq!(bwd, want, "backward neighbors of {node}");
    }

    let props = load_props(&mut zu, person.id)
        .unwrap()
        .expect("props survive");
    let mut reader = PropsReader::new(props);
    let age = reader.col("age").expect("age column");
    let name = reader.col("name").expect("name column");
    let mut buf = Vec::new();
    for row in 0..6u64 {
        assert_eq!(
            reader.read_int(&mut zu, age, row).unwrap(),
            AGES[row as usize]
        );
        buf.clear();
        reader.read_str(&mut zu, name, row, &mut buf).unwrap();
        assert_eq!(buf, NAMES[row as usize].as_bytes());
    }
}

/// Asserts the sqlite store at `path` holds exactly the fixture graph.
fn assert_sqlite_matches(path: &std::path::Path) {
    let sq = SqliteStore::open(path).unwrap();
    let tables = sq.tables().unwrap();
    let person = tables
        .iter()
        .find(|t| t.kind == "node" && t.name == "person")
        .expect("person survives");
    let knows = tables
        .iter()
        .find(|t| t.kind == "rel" && t.name == "knows")
        .expect("knows survives");
    assert_eq!(knows.src_table.as_deref(), Some("person"));
    assert_eq!(knows.dst_table.as_deref(), Some("person"));
    assert_eq!(person.kind, "node");
    assert_eq!(sq.node_count("person").unwrap(), 6);
    assert_eq!(sq.rel_count("knows").unwrap(), EDGES.len() as i64);
    assert_eq!(sq.node_columns("person").unwrap(), ["age", "name"]);

    for row in 0..6i64 {
        assert_eq!(
            sq.read_node_prop("person", row, "age").unwrap(),
            SqlValue::Int(AGES[row as usize] as i64)
        );
        assert_eq!(
            sq.read_node_prop("person", row, "name").unwrap(),
            SqlValue::Text(NAMES[row as usize].to_owned())
        );
        let fwd = sq.neighbors("knows", row, Direction::Fwd).unwrap();
        let want: Vec<i64> = EDGES
            .iter()
            .filter(|&&(s, _)| i64::from(s) == row)
            .map(|&(_, d)| i64::from(d))
            .collect();
        assert_eq!(fwd, want, "forward neighbors of {row}");
    }
}

#[test]
fn zu1_round_trips_through_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a.zu1"),
        dir.path().join("b.db"),
        dir.path().join("c.zu1"),
    );
    seed_zu1(&a);
    zu1_to_sqlite(&a, &b).unwrap();
    assert_sqlite_matches(&b);
    sqlite_to_zu1(&b, &c).unwrap();
    assert_zu1_matches(&c);

    // The twice-converted store answers a query exactly as the source.
    let q = "MATCH (a:person)-[:knows]->(b) WHERE a.age < b.age \
             RETURN a.name AS name, b.id AS to ORDER BY name, to";
    let mut src = Zu1File::open(&a).unwrap();
    let mut back = Zu1File::open(&c).unwrap();
    let want = run_zu1(q, &mut src, &[]).unwrap();
    let mid = run_sqlite(q, &SqliteStore::open(&b).unwrap(), &[]).unwrap();
    let got = run_zu1(q, &mut back, &[]).unwrap();
    assert!(!want.rows.is_empty());
    assert_eq!(want.rows, mid.rows);
    assert_eq!(want.rows, got.rows);
}

#[test]
fn sqlite_round_trips_through_zu1() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a.db"),
        dir.path().join("b.zu1"),
        dir.path().join("c.db"),
    );
    seed_sqlite(&a);
    sqlite_to_zu1(&a, &b).unwrap();
    assert_zu1_matches(&b);
    zu1_to_sqlite(&b, &c).unwrap();
    assert_sqlite_matches(&c);
}

#[test]
fn distinct_endpoint_rels_error_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("a.db");
    let mut sq = SqliteStore::open(&db).unwrap();
    sq.create_node_table("person", &[]).unwrap();
    sq.create_node_table("city", &[]).unwrap();
    sq.create_rel_table("lives_in", "person", "city", &[])
        .unwrap();
    drop(sq);
    let err = sqlite_to_zu1(&db, &dir.path().join("b.zu1")).unwrap_err();
    assert!(
        format!("{err}").contains("one node table"),
        "unexpected error: {err}"
    );
}

/// A null crosses the hop as a null and not as the placeholder the row
/// holds underneath it, including when it is the first row of the column
/// and the type has to come from a row further down.
#[test]
fn null_properties_survive_the_hop_and_read_back_as_null() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("a.db");
    let mut sq = SqliteStore::open(&db).unwrap();
    sq.create_node_table(
        "person",
        &[("age", ColumnType::Integer), ("name", ColumnType::Text)],
    )
    .unwrap();
    sq.create_rel_table("knows", "person", "person", &[])
        .unwrap();
    sq.begin().unwrap();
    sq.insert_node_at(
        "person",
        0,
        &[SqlValue::Null, SqlValue::Text("ada".to_owned())],
    )
    .unwrap();
    sq.insert_node_at(
        "person",
        1,
        &[SqlValue::Int(31), SqlValue::Text("bob".to_owned())],
    )
    .unwrap();
    sq.insert_node_at("person", 2, &[SqlValue::Null, SqlValue::Null])
        .unwrap();
    sq.insert_rel("knows", 0, 1, &[]).unwrap();
    sq.commit().unwrap();
    drop(sq);

    let out = dir.path().join("b.zu1");
    sqlite_to_zu1(&db, &out).unwrap();
    let mut zu = Zu1File::open(&out).unwrap();

    let q = "MATCH (p:person) RETURN p.id AS id, p.age AS age, p.name AS name ORDER BY id";
    let got = run_zu1(q, &mut zu, &[]).unwrap();
    assert_eq!(got.rows.len(), 3);
    assert_eq!(format!("{:?}", got.rows[0][1]), "Null");
    assert_eq!(format!("{:?}", got.rows[2][1]), "Null");
    assert_eq!(format!("{:?}", got.rows[2][2]), "Null");
    assert_ne!(format!("{:?}", got.rows[1][1]), "Null");

    // The predicates a null answers are the two that ask about it, and a
    // comparison against one is unknown rather than false, so neither the
    // equality nor its negation picks up the rows holding nothing.
    let counts = |q: &str, zu: &mut Zu1File| -> usize { run_zu1(q, zu, &[]).unwrap().rows.len() };
    assert_eq!(
        counts("MATCH (p:person) WHERE p.age IS NULL RETURN p.id", &mut zu),
        2
    );
    assert_eq!(
        counts(
            "MATCH (p:person) WHERE p.age IS NOT NULL RETURN p.id",
            &mut zu
        ),
        1
    );
    assert_eq!(
        counts("MATCH (p:person) WHERE p.age = 31 RETURN p.id", &mut zu),
        1
    );
    assert_eq!(
        counts("MATCH (p:person) WHERE p.age <> 31 RETURN p.id", &mut zu),
        0
    );

    // A set function drops the nulls before it counts, so COUNT over the
    // column is not COUNT over the rows.
    let agg = run_zu1(
        "MATCH (p:person) RETURN count(p.age) AS have, count(*) AS rows",
        &mut zu,
        &[],
    )
    .unwrap();
    assert_eq!(format!("{:?}", agg.rows[0][0]), "Int(1)");
    assert_eq!(format!("{:?}", agg.rows[0][1]), "Int(3)");

    // And back the other way, where writing the placeholder out would
    // turn the null into a zero and an empty string.
    let back = dir.path().join("c.db");
    zu1_to_sqlite(&out, &back).unwrap();
    let sq = SqliteStore::open(&back).unwrap();
    assert_eq!(
        sq.read_node_prop("person", 0, "age").unwrap(),
        SqlValue::Null
    );
    assert_eq!(
        sq.read_node_prop("person", 1, "age").unwrap(),
        SqlValue::Int(31)
    );
    assert_eq!(
        sq.read_node_prop("person", 2, "name").unwrap(),
        SqlValue::Null
    );
}

/// A column that is null on every row still has a type, because the
/// sqlite table declared one, so it stores as an empty column of that
/// type with no row set in its validity words.
#[test]
fn a_column_of_nothing_but_nulls_takes_its_type_from_the_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("a.db");
    let mut sq = SqliteStore::open(&db).unwrap();
    sq.create_node_table("person", &[("age", ColumnType::Integer)])
        .unwrap();
    sq.create_rel_table("knows", "person", "person", &[])
        .unwrap();
    sq.begin().unwrap();
    sq.insert_node_at("person", 0, &[SqlValue::Null]).unwrap();
    sq.insert_node_at("person", 1, &[SqlValue::Null]).unwrap();
    sq.insert_rel("knows", 0, 1, &[]).unwrap();
    sq.commit().unwrap();
    drop(sq);
    let out = dir.path().join("b.zu1");
    sqlite_to_zu1(&db, &out).unwrap();
    let mut zu = Zu1File::open(&out).unwrap();
    let got = run_zu1(
        "MATCH (p:person) WHERE p.age IS NULL RETURN p.id",
        &mut zu,
        &[],
    )
    .unwrap();
    assert_eq!(got.rows.len(), 2);
}

/// A float column and a byte string column survive both hops, and the
/// query layer reads the float back as a float rather than as the word
/// the lane holds it in.
#[test]
fn float_and_byte_columns_survive_both_hops() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a.db"),
        dir.path().join("b.zu1"),
        dir.path().join("c.db"),
    );
    let scores = [1.5f64, -0.25, 3.75, 0.0, 1e300, -1e-300];
    let raw: [&[u8]; 6] = [b"\x00\xff", b"", b"\x01", b"\xfe\xed", b"zz", b"\x7f"];
    let mut sq = SqliteStore::open(&a).unwrap();
    sq.create_node_table(
        "person",
        &[("score", ColumnType::Real), ("tag", ColumnType::Blob)],
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
                SqlValue::Real(scores[row]),
                SqlValue::Blob(raw[row].to_vec()),
            ],
        )
        .unwrap();
    }
    for &(src, dst) in &EDGES {
        sq.insert_rel("knows", i64::from(src), i64::from(dst), &[])
            .unwrap();
    }
    sq.commit().unwrap();
    drop(sq);

    sqlite_to_zu1(&a, &b).unwrap();
    let mut zu = Zu1File::open(&b).unwrap();
    let person = Catalog::load(&mut zu)
        .unwrap()
        .node_by_name("person")
        .unwrap()
        .id;
    let mut reader = PropsReader::new(load_props(&mut zu, person).unwrap().unwrap());
    let score = reader.col("score").unwrap();
    let tag = reader.col("tag").unwrap();
    let mut buf = Vec::new();
    for row in 0..6u64 {
        let word = reader.read_int(&mut zu, score, row).unwrap();
        assert_eq!(f64::from_bits(word), scores[row as usize]);
        buf.clear();
        reader.read_str(&mut zu, tag, row, &mut buf).unwrap();
        assert_eq!(buf, raw[row as usize]);
    }
    let got = run_zu1(
        "MATCH (p:person) RETURN p.score AS s ORDER BY s",
        &mut zu,
        &[],
    )
    .unwrap();
    let mut want = scores;
    want.sort_by(f64::total_cmp);
    let read: Vec<f64> = got
        .rows
        .iter()
        .map(|r| match r[0] {
            zu::query::Value::Float(f) => f,
            ref other => panic!("expected a float, got {other:?}"),
        })
        .collect();
    assert_eq!(read, want);

    zu1_to_sqlite(&b, &c).unwrap();
    let back = SqliteStore::open(&c).unwrap();
    for row in 0..6i64 {
        assert_eq!(
            back.read_node_prop("person", row, "score").unwrap(),
            SqlValue::Real(scores[row as usize])
        );
        assert_eq!(
            back.read_node_prop("person", row, "tag").unwrap(),
            SqlValue::Blob(raw[row as usize].to_vec())
        );
    }
}

/// A boolean has no storage class of its own in sqlite, so the only
/// thing that can carry it across the hop is the declared type. This
/// checks it does, and that the query layer answers with truth values
/// rather than the integers they are stored as.
#[test]
fn a_boolean_column_survives_the_sqlite_hop_on_its_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a.db"),
        dir.path().join("b.zu1"),
        dir.path().join("c.db"),
    );
    let active = [true, false, false, true, true, false];
    let mut sq = SqliteStore::open(&a).unwrap();
    sq.create_node_table("person", &[("active", ColumnType::Boolean)])
        .unwrap();
    sq.create_rel_table("knows", "person", "person", &[])
        .unwrap();
    sq.begin().unwrap();
    for (row, &on) in active.iter().enumerate() {
        sq.insert_node_at("person", row as i64, &[SqlValue::Int(i64::from(on))])
            .unwrap();
    }
    for &(src, dst) in &EDGES {
        sq.insert_rel("knows", i64::from(src), i64::from(dst), &[])
            .unwrap();
    }
    sq.commit().unwrap();
    drop(sq);

    sqlite_to_zu1(&a, &b).unwrap();
    let mut zu = Zu1File::open(&b).unwrap();
    let person = Catalog::load(&mut zu)
        .unwrap()
        .node_by_name("person")
        .unwrap()
        .id;
    let props = load_props(&mut zu, person).unwrap().unwrap();
    assert_eq!(props.columns[0].ty, zu_common::LogicalType::Bool);

    let got = run_zu1("MATCH (p:person) RETURN p.active AS a", &mut zu, &[]).unwrap();
    let read: Vec<bool> = got
        .rows
        .iter()
        .map(|r| match r[0] {
            zu::query::Value::Bool(v) => v,
            ref other => panic!("expected a boolean, got {other:?}"),
        })
        .collect();
    assert_eq!(read, active);

    // Back through sqlite the declaration has to survive too, or the
    // next conversion reads the column as a count of nothing.
    zu1_to_sqlite(&b, &c).unwrap();
    let back = SqliteStore::open(&c).unwrap();
    assert_eq!(
        back.node_column_types("person").unwrap(),
        vec![("active".to_string(), ColumnType::Boolean)]
    );
    for row in 0..6i64 {
        assert_eq!(
            back.read_node_prop("person", row, "active").unwrap(),
            SqlValue::Int(i64::from(active[row as usize]))
        );
    }
}

/// A list column crosses sqlite as a JSON array in a text column, so
/// the declaration carries the element type and the text carries the
/// elements. Both have to survive, in both directions, or a fixture
/// staged with a list loads as a column of strings that happen to look
/// like arrays.
#[test]
fn a_list_column_survives_both_hops_with_its_element_type() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a.db"),
        dir.path().join("b.zu1"),
        dir.path().join("c.db"),
    );
    // The empty list, one holding a quote and a backslash, and one
    // holding a float whose shortest spelling is not its literal, are
    // the three the encoding is most likely to be wrong about.
    let xs = [
        "[1,2,3]",
        "[]",
        "[-9223372036854775808]",
        "[0]",
        "[7,7]",
        "[42]",
    ];
    let tags = [
        r#"["a","b"]"#,
        r#"[]"#,
        r#"["say \"hi\"","back\\slash"]"#,
        r#"["héllo"]"#,
        r#"[""]"#,
        r#"["z"]"#,
    ];
    let scores = ["[0.1]", "[]", "[1.5,-0.25]", "[1e300]", "[0.0]", "[-0.0]"];
    let mut sq = SqliteStore::open(&a).unwrap();
    sq.create_node_table(
        "person",
        &[
            ("xs", ColumnType::IntegerList),
            ("tags", ColumnType::TextList),
            ("scores", ColumnType::RealList),
        ],
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
                SqlValue::Text(xs[row].to_string()),
                SqlValue::Text(tags[row].to_string()),
                SqlValue::Text(scores[row].to_string()),
            ],
        )
        .unwrap();
    }
    for &(src, dst) in &EDGES {
        sq.insert_rel("knows", i64::from(src), i64::from(dst), &[])
            .unwrap();
    }
    sq.commit().unwrap();
    drop(sq);

    sqlite_to_zu1(&a, &b).unwrap();
    let mut zu = Zu1File::open(&b).unwrap();
    let got = run_zu1(
        "MATCH (p:person) RETURN CARDINALITY(p.xs) + CARDINALITY(p.tags) AS n ORDER BY n",
        &mut zu,
        &[],
    )
    .unwrap();
    let read: Vec<i64> = got
        .rows
        .iter()
        .map(|r| match r[0] {
            zu::query::Value::Int(v) => v,
            ref other => panic!("expected a count, got {other:?}"),
        })
        .collect();
    assert_eq!(read, vec![0, 2, 2, 3, 3, 5]);
    let got = run_zu1(
        "MATCH (p:person) WHERE p.id = 2 RETURN p.tags AS v",
        &mut zu,
        &[],
    )
    .unwrap();
    assert_eq!(
        got.rows[0][0],
        zu::query::Value::List(vec![
            zu::query::Value::Str("say \"hi\"".into()),
            zu::query::Value::Str("back\\slash".into()),
        ])
    );

    // Back out again the declarations have to come back as they went
    // in, and the arrays have to be the arrays that were staged rather
    // than a reformatting of them.
    zu1_to_sqlite(&b, &c).unwrap();
    let back = SqliteStore::open(&c).unwrap();
    assert_eq!(
        back.node_column_types("person").unwrap(),
        vec![
            ("xs".to_string(), ColumnType::IntegerList),
            ("tags".to_string(), ColumnType::TextList),
            ("scores".to_string(), ColumnType::RealList),
        ]
    );
    for row in 0..6i64 {
        let SqlValue::Text(got) = back.read_node_prop("person", row, "scores").unwrap() else {
            panic!("a list column comes back as text");
        };
        // The float column is compared by value rather than by spelling,
        // because a shortest round trip spelling is not the spelling the
        // fixture happened to write.
        let want: Vec<f64> = scores[row as usize]
            .trim_matches(['[', ']'])
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap())
            .collect();
        let read: Vec<f64> = got
            .trim_matches(['[', ']'])
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(read.len(), want.len(), "row {row}: {got}");
        for (r, w) in read.iter().zip(&want) {
            assert_eq!(r.to_bits(), w.to_bits(), "row {row}: {got}");
        }
        assert_eq!(
            back.read_node_prop("person", row, "xs").unwrap(),
            SqlValue::Text(xs[row as usize].to_string())
        );
        assert_eq!(
            back.read_node_prop("person", row, "tags").unwrap(),
            SqlValue::Text(tags[row as usize].to_string())
        );
    }
}

/// GH02. Whether a rel table's edges have a direction is part of what
/// the table is, so it crosses both hops: the sqlite catalogue records
/// it, the zu1 catalog records it, and a round trip through either
/// engine leaves it saying the same thing.
#[test]
fn the_undirected_flag_survives_both_hops() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a.db"),
        dir.path().join("b.zu1"),
        dir.path().join("c.db"),
    );
    let mut sq = SqliteStore::open(&a).unwrap();
    sq.create_node_table("peer", &[]).unwrap();
    sq.create_rel_table_as("friend", "peer", "peer", &[], true)
        .unwrap();
    sq.begin().unwrap();
    for row in 0..2i64 {
        sq.insert_node_at("peer", row, &[]).unwrap();
    }
    sq.insert_rel("friend", 0, 1, &[]).unwrap();
    sq.commit().unwrap();
    drop(sq);

    sqlite_to_zu1(&a, &b).unwrap();
    let mut zu = Zu1File::open(&b).unwrap();
    let catalog = Catalog::load(&mut zu).unwrap();
    let friend = catalog.rel_by_name("friend").expect("friend survives");
    assert!(friend.undirected, "the flag crosses into zu1");
    assert_eq!(friend.edge_count, 1, "an undirected edge is stored once");
    drop(zu);

    // And the pattern that admits it finds it from either end, which is
    // the storage half doing its job on a converted file.
    let mut zu = Zu1File::open(&b).unwrap();
    let rows = run_zu1(
        "MATCH (a:peer)~[:friend]~(b:peer) RETURN b.id AS id",
        &mut zu,
        &[],
    )
    .unwrap()
    .rows;
    assert_eq!(rows.len(), 2, "once from each end: {rows:?}");
    drop(zu);

    zu1_to_sqlite(&b, &c).unwrap();
    let back = SqliteStore::open(&c).unwrap();
    let table = back
        .tables()
        .unwrap()
        .into_iter()
        .find(|t| t.name == "friend")
        .expect("friend survives the way back");
    assert!(table.undirected, "the flag crosses back into sqlite");
}
