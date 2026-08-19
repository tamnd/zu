//! A dataset of node files and rel files, loaded as the tables it names.
//!
//! The fixture is the finbench shape in miniature: accounts, people who
//! own them, and transfers between them, with the ids in one flat space
//! the way an LDBC style export writes them, so no label's ids start at
//! zero and none of them are its row numbers. That is the whole point of
//! the load: what comes back out has to be the ids that went in.

use std::path::{Path, PathBuf};

use zu::dataset::{NodeFile, RelFile, load_dataset};
use zu::query::run;
use zu_query::exec::Value;
use zu_zu1::file::Zu1File;

/// Accounts 10, 11 and 12; people 100 and 101. Neither range starts at
/// zero and neither is a row number, so a load that answers with the
/// offset answers wrong and the tests below say so.
fn write(dir: &Path) -> (Vec<NodeFile>, Vec<RelFile>) {
    let at = |name: &str, body: &str| -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    };
    let account = at(
        "Account.csv",
        "id:ID,name:STRING,balance:FLOAT64\n\
         10,checking,100.5\n\
         11,savings,20.25\n\
         12,brokerage,0\n",
    );
    let person = at("Person.csv", "id:ID,name:STRING\n100,ann\n101,bo\n");
    let transfer = at(
        "transfer.csv",
        ":START_ID,:END_ID,:TYPE,ts:INT64,amount:FLOAT64\n\
         10,11,transfer,1,5\n\
         11,12,transfer,9,7\n\
         10,12,transfer,7,1.5\n",
    );
    let own = at(
        "own.csv",
        ":START_ID,:END_ID,:TYPE\n100,10,own\n101,12,own\n",
    );
    let node = |table: &str, path: PathBuf| NodeFile {
        table: table.to_string(),
        path,
    };
    let rel = |table: &str, from: &str, to: &str, path: PathBuf| RelFile {
        table: table.to_string(),
        from: from.to_string(),
        to: to.to_string(),
        path,
        undirected: false,
    };
    (
        vec![node("Account", account), node("Person", person)],
        vec![
            rel("transfer", "Account", "Account", transfer),
            rel("own", "Person", "Account", own),
        ],
    )
}

fn graph(dir: &Path) -> Zu1File {
    let (nodes, rels) = write(dir);
    let out = dir.join("fixture.zu1");
    let stats = load_dataset(&nodes, &rels, &out).expect("load");
    assert_eq!(
        stats.nodes,
        vec![("Account".to_string(), 3), ("Person".to_string(), 2)]
    );
    assert_eq!(
        stats.rels,
        vec![("transfer".to_string(), 3), ("own".to_string(), 2)]
    );
    Zu1File::open(&out).unwrap()
}

fn rows(db: &mut Zu1File, source: &str) -> Vec<Vec<Value>> {
    run(source, db, &[])
        .unwrap_or_else(|e| panic!("{source}: {e}"))
        .rows
        .into_vec()
}

fn ints(db: &mut Zu1File, source: &str) -> Vec<i64> {
    rows(db, source)
        .into_iter()
        .map(|row| match row[0] {
            Value::Int(v) => v,
            ref other => panic!("{source} answered {other:?}"),
        })
        .collect()
}

/// Each label is its own table, and a row of it answers with the id the
/// file gave it rather than with where the row landed.
#[test]
fn every_label_loads_as_its_own_table_and_keeps_its_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        ints(&mut db, "MATCH (a:Account) RETURN a.id AS id ORDER BY id"),
        vec![10, 11, 12]
    );
    assert_eq!(
        ints(&mut db, "MATCH (p:Person) RETURN p.id AS id ORDER BY id"),
        vec![100, 101]
    );
}

/// A node property column comes back typed as the header declared it,
/// which is what a query over this dataset is for.
#[test]
fn a_node_carries_the_columns_its_header_named() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let row = rows(
        &mut db,
        "MATCH (a:Account) WHERE a.id = 11 RETURN a.name AS name, a.balance AS balance",
    );
    assert_eq!(row.len(), 1, "one account is 11: {row:?}");
    assert_eq!(row[0][0], Value::Str("savings".into()));
    assert_eq!(row[0][1], Value::Float(20.25));
}

/// An id names a row without a scan, through the key index the load
/// wrote, so an anchored pattern starts where it was told to.
#[test]
fn an_anchor_finds_the_row_the_dataset_id_names() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        ints(
            &mut db,
            "MATCH (a:Account {id: 10})-[:transfer]->(b:Account) RETURN b.id AS id ORDER BY id",
        ),
        vec![11, 12]
    );
}

/// A rel table between two labels reads back as that: a person's own
/// lands in the account table rather than in a row of the person one.
#[test]
fn a_rel_between_two_labels_lands_in_the_table_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let pairs = rows(
        &mut db,
        "MATCH (p:Person)-[:own]->(a:Account) RETURN p.id AS p, a.id AS a ORDER BY p",
    );
    assert_eq!(
        pairs,
        vec![
            vec![Value::Int(100), Value::Int(10)],
            vec![Value::Int(101), Value::Int(12)],
        ]
    );
}

/// The edge columns survive the sort a load puts the edges through, so
/// an edge's time is the one written beside it and not another edge's.
#[test]
fn an_edge_keeps_the_columns_written_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    let seen = rows(
        &mut db,
        "MATCH (a:Account)-[t:transfer]->(b:Account) \
         RETURN a.id AS a, b.id AS b, t.ts AS ts, t.amount AS amount ORDER BY a, b",
    );
    assert_eq!(
        seen,
        vec![
            vec![
                Value::Int(10),
                Value::Int(11),
                Value::Int(1),
                Value::Float(5.0)
            ],
            vec![
                Value::Int(10),
                Value::Int(12),
                Value::Int(7),
                Value::Float(1.5)
            ],
            vec![
                Value::Int(11),
                Value::Int(12),
                Value::Int(9),
                Value::Float(7.0)
            ],
        ]
    );
}

/// Which is what the path predicates are waiting on: a gate on a walk
/// needs an edge column to ask about, and only a load that kept the rel
/// tables has one. Ungated 10 reaches both 11 and 12; gated on the
/// window the first hop to 11 is at time 1 and does not happen, so the
/// walk through it does not either.
#[test]
fn a_gated_walk_over_the_dataset_reads_the_edge_column() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = graph(dir.path());
    assert_eq!(
        ints(
            &mut db,
            "MATCH (a:Account)-[t:transfer*1..2]->(b:Account) WHERE a.id = 10 \
             RETURN b.id AS id ORDER BY id",
        ),
        vec![11, 12, 12],
        "10 to 11, 10 to 12, and 10 to 11 to 12"
    );
    assert_eq!(
        ints(
            &mut db,
            "MATCH (a:Account)-[t:transfer*1..2 WHERE t.ts >= 5]->(b:Account) WHERE a.id = 10 \
             RETURN b.id AS id ORDER BY id",
        ),
        vec![12],
        "only the direct transfer at time 7 is inside the window"
    );
}

/// An endpoint no node file declared is named rather than loaded, since
/// inventing a row for it would put an account in the graph that the
/// dataset never said was there.
#[test]
fn an_endpoint_no_node_file_declared_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (nodes, mut rels) = write(dir.path());
    let stray = dir.path().join("stray.csv");
    std::fs::write(&stray, ":START_ID,:END_ID,:TYPE\n10,99,transfer\n").unwrap();
    rels[0].path = stray;
    let err = load_dataset(&nodes, &rels, &dir.path().join("bad.zu1")).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("99"), "the id belongs in the error: {text}");
    assert!(
        text.contains("Account"),
        "so does the table that has no such row: {text}"
    );
}
