//! End-to-end tests over real temporary database files.

use std::path::PathBuf;

use rusqlite::Connection;
use tempfile::TempDir;
use zu_common::ZuError;
use zu_sqlite::{APPLICATION_ID, ColumnType, Direction, SCHEMA_VERSION, SqliteStore, Value};

fn temp_db() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.db");
    (dir, path)
}

const EDGES: [(i64, i64); 8] = [
    (1, 2),
    (1, 3),
    (2, 3),
    (2, 5),
    (3, 4),
    (4, 5),
    (5, 6),
    (6, 1),
];

/// Six people, eight `knows` edges, ids checked to be sequential.
fn small_graph(store: &mut SqliteStore) {
    store
        .create_node_table("person", &[("name", ColumnType::Text)])
        .unwrap();
    store
        .create_rel_table(
            "knows",
            "person",
            "person",
            &[("since", ColumnType::Integer)],
        )
        .unwrap();
    for i in 1..=6i64 {
        let id = store
            .insert_node("person", &[Value::Text(format!("p{i}"))])
            .unwrap();
        assert_eq!(id, i);
    }
    for (n, (src, dst)) in EDGES.iter().enumerate() {
        let year = 2000 + i64::try_from(n).unwrap();
        let id = store
            .insert_rel("knows", *src, *dst, &[Value::Int(year)])
            .unwrap();
        assert_eq!(id, i64::try_from(n).unwrap() + 1);
    }
}

#[test]
fn open_sets_wal_application_id_and_user_version() {
    let (_dir, path) = temp_db();
    drop(SqliteStore::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let app_id: i32 = conn
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .unwrap();
    assert_eq!(app_id, APPLICATION_ID);
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
}

#[test]
fn rejects_foreign_application_id() {
    let (_dir, path) = temp_db();
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "application_id", 0x1234_5678)
        .unwrap();
    conn.execute_batch("CREATE TABLE alien (x INTEGER)")
        .unwrap();
    drop(conn);
    let err = SqliteStore::open(&path).unwrap_err();
    assert!(matches!(
        err,
        ZuError::Corrupt {
            what: "sqlite application_id",
            ..
        }
    ));
}

#[test]
fn rejects_non_database_file() {
    let (_dir, path) = temp_db();
    std::fs::write(&path, vec![0x42u8; 1024]).unwrap();
    let err = SqliteStore::open(&path).unwrap_err();
    assert!(matches!(err, ZuError::Corrupt { .. }));
}

#[test]
fn create_tables_populate_catalog() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    let entries = store.catalog_entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, "node");
    assert_eq!(entries[0].name, "person");
    assert!(entries[0].sql.contains("CREATE TABLE n_person"));
    assert!(entries[0].sql.contains("p_name TEXT"));
    assert_eq!(entries[1].kind, "rel");
    assert_eq!(entries[1].name, "knows");
    assert!(entries[1].sql.contains("CREATE TABLE r_knows"));
    assert!(entries[1].sql.contains("CREATE INDEX r_knows_fwd"));
    assert!(entries[1].sql.contains("CREATE INDEX r_knows_bwd"));
}

#[test]
fn neighbors_forward() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    let fwd = |n| store.neighbors("knows", n, Direction::Fwd).unwrap();
    assert_eq!(fwd(1), vec![2, 3]);
    assert_eq!(fwd(2), vec![3, 5]);
    assert_eq!(fwd(3), vec![4]);
    assert_eq!(fwd(4), vec![5]);
    assert_eq!(fwd(5), vec![6]);
    assert_eq!(fwd(6), vec![1]);
}

#[test]
fn neighbors_backward() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    let bwd = |n| store.neighbors("knows", n, Direction::Bwd).unwrap();
    assert_eq!(bwd(1), vec![6]);
    assert_eq!(bwd(2), vec![1]);
    assert_eq!(bwd(3), vec![1, 2]);
    assert_eq!(bwd(4), vec![3]);
    assert_eq!(bwd(5), vec![2, 4]);
    assert_eq!(bwd(6), vec![5]);
}

#[test]
fn neighbors_of_unconnected_node_are_empty() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    assert!(
        store
            .neighbors("knows", 42, Direction::Fwd)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .neighbors("knows", 42, Direction::Bwd)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn counts_match_inserted_graph() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    assert_eq!(store.node_count("person").unwrap(), 6);
    assert_eq!(store.rel_count("knows").unwrap(), 8);
}

#[test]
fn reopen_preserves_catalog_and_data() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    drop(store);
    let store = SqliteStore::open(&path).unwrap();
    let entries = store.catalog_entries().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "person");
    assert_eq!(entries[1].name, "knows");
    assert_eq!(store.node_count("person").unwrap(), 6);
    assert_eq!(store.rel_count("knows").unwrap(), 8);
    assert_eq!(
        store.neighbors("knows", 2, Direction::Fwd).unwrap(),
        vec![3, 5]
    );
    assert_eq!(
        store.neighbors("knows", 5, Direction::Bwd).unwrap(),
        vec![2, 4]
    );
}

#[test]
fn property_values_roundtrip_through_sqlite() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    store
        .create_node_table(
            "thing",
            &[
                ("i", ColumnType::Integer),
                ("r", ColumnType::Real),
                ("t", ColumnType::Text),
                ("b", ColumnType::Blob),
            ],
        )
        .unwrap();
    store
        .insert_node(
            "thing",
            &[
                Value::Int(7),
                Value::Real(1.5),
                Value::Text("zu".to_owned()),
                Value::Blob(vec![1, 2, 3]),
            ],
        )
        .unwrap();
    store
        .insert_node(
            "thing",
            &[Value::Null, Value::Null, Value::Null, Value::Null],
        )
        .unwrap();
    drop(store);
    let conn = Connection::open(&path).unwrap();
    let row: (i64, f64, String, Vec<u8>) = conn
        .query_row(
            "SELECT p_i, p_r, p_t, p_b FROM n_thing WHERE zrow = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(row.0, 7);
    assert!((row.1 - 1.5).abs() < f64::EPSILON);
    assert_eq!(row.2, "zu");
    assert_eq!(row.3, vec![1, 2, 3]);
    let null_rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM n_thing WHERE zrow = 2 AND p_i IS NULL AND p_t IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(null_rows, 1);
}

#[test]
fn rejects_invalid_identifiers() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    let err = store.create_node_table("bad name", &[]).unwrap_err();
    assert!(matches!(err, ZuError::InvalidArgument(_)));
    let err = store
        .create_node_table("ok", &[("bad-col", ColumnType::Text)])
        .unwrap_err();
    assert!(matches!(err, ZuError::InvalidArgument(_)));
    let err = store
        .neighbors("k; DROP TABLE zu_catalog", 1, Direction::Fwd)
        .unwrap_err();
    assert!(matches!(err, ZuError::InvalidArgument(_)));
    assert!(store.catalog_entries().unwrap().is_empty());
}

#[test]
fn duplicate_table_name_fails() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    store.create_node_table("person", &[]).unwrap();
    assert!(store.create_node_table("person", &[]).is_err());
    assert_eq!(store.catalog_entries().unwrap().len(), 1);
}

/// A schema version 1 file, built byte for byte the way the old code
/// left it, migrates on open: entries keep their rowids as ids, the
/// endpoint columns arrive null, and new rel tables record endpoints.
#[test]
fn version_one_files_migrate_on_open() {
    let (_dir, path) = temp_db();
    let conn = Connection::open(&path).unwrap();
    conn.pragma_update(None, "application_id", APPLICATION_ID)
        .unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute_batch(
        "CREATE TABLE zu_catalog (\
           kind TEXT NOT NULL, name TEXT NOT NULL, sql TEXT NOT NULL, \
           PRIMARY KEY (kind, name));\
         INSERT INTO zu_catalog VALUES ('node', 'person', 'CREATE TABLE n_person (zrow INTEGER PRIMARY KEY)');\
         INSERT INTO zu_catalog VALUES ('rel', 'knows', 'CREATE TABLE r_knows (zrel INTEGER PRIMARY KEY, src INTEGER NOT NULL, dst INTEGER NOT NULL)');\
         CREATE TABLE n_person (zrow INTEGER PRIMARY KEY);\
         CREATE TABLE r_knows (zrel INTEGER PRIMARY KEY, src INTEGER NOT NULL, dst INTEGER NOT NULL);",
    )
    .unwrap();
    let old_ids: Vec<(i64, String)> = conn
        .prepare("SELECT rowid, name FROM zu_catalog ORDER BY rowid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    drop(conn);

    let mut store = SqliteStore::open(&path).unwrap();
    let version: i32 = store
        .raw()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let tables = store.tables().unwrap();
    let ids: Vec<(i64, String)> = tables
        .iter()
        .map(|t| (i64::from(t.id), t.name.clone()))
        .collect();
    assert_eq!(ids, old_ids, "migration must keep table ids");
    for t in &tables {
        assert_eq!(t.src_table, None);
        assert_eq!(t.dst_table, None);
    }
    store
        .create_rel_table("likes", "person", "person", &[])
        .unwrap();
    let likes = store
        .tables()
        .unwrap()
        .into_iter()
        .find(|t| t.name == "likes")
        .unwrap();
    assert_eq!(likes.src_table.as_deref(), Some("person"));
    assert_eq!(likes.dst_table.as_deref(), Some("person"));
    // A file written before the column existed holds directed edges,
    // which is the default the column arrives with (GH02).
    assert!(tables.iter().all(|t| !t.undirected));
    assert!(!likes.undirected);
}

/// A schema version 2 file predates the direction column alone, so it
/// migrates with an ALTER and keeps everything else where it was.
#[test]
fn version_two_files_gain_the_direction_column() {
    let (_dir, path) = temp_db();
    {
        let mut store = SqliteStore::open(&path).unwrap();
        store.create_node_table("peer", &[]).unwrap();
        store
            .create_rel_table("friend", "peer", "peer", &[])
            .unwrap();
    }
    // Back to what version 2 left behind: the same catalogue without
    // the column, and the pragma that says so.
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE zu_catalog DROP COLUMN undirected;         PRAGMA user_version = 2;",
    )
    .unwrap();
    drop(conn);

    let mut store = SqliteStore::open(&path).unwrap();
    let version: i32 = store
        .raw()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let tables = store.tables().unwrap();
    assert_eq!(tables.len(), 2);
    assert!(tables.iter().all(|t| !t.undirected));
    let friend = tables.iter().find(|t| t.name == "friend").unwrap();
    assert_eq!(friend.src_table.as_deref(), Some("peer"));

    // And a table created after the migration can say it has no
    // direction, which is what the column is for.
    store
        .create_rel_table_as("near", "peer", "peer", &[], true)
        .unwrap();
    let near = store
        .tables()
        .unwrap()
        .into_iter()
        .find(|t| t.name == "near")
        .unwrap();
    assert!(near.undirected);
}

/// Rel endpoints must name existing node tables.
#[test]
fn rel_table_rejects_unknown_endpoints() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    store.create_node_table("person", &[]).unwrap();
    let err = store
        .create_rel_table("knows", "person", "city", &[])
        .unwrap_err();
    assert!(matches!(err, ZuError::InvalidArgument(_)));
    assert_eq!(store.tables().unwrap().len(), 1);
}

/// The labels a node carries beyond its table's name are written per
/// row and read back per table, dense so the loader on the other side
/// has a word to write for every row.
#[test]
fn extra_labels_are_written_per_row_and_read_per_table() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    store.create_node_table("person", &[]).unwrap();
    let rows: Vec<i64> = (0..4)
        .map(|_| store.insert_node("person", &[]).unwrap())
        .collect();
    // A table nobody labelled costs nothing to read.
    assert!(store.node_labels("person").unwrap().is_empty());

    store
        .set_node_labels("person", rows[0], &["Employee"])
        .unwrap();
    store
        .set_node_labels("person", rows[2], &["Manager", "Employee"])
        .unwrap();
    // The table's own name is true of the row and is not news, so it
    // is accepted and dropped rather than refused.
    store
        .set_node_labels("person", rows[3], &["person"])
        .unwrap();
    // Row zero is the one no insert took, because a sqlite rowid
    // starts at one; the vector is indexed by row all the same.
    assert_eq!(
        store.node_labels("person").unwrap(),
        [
            vec![],
            vec!["Employee".to_string()],
            vec![],
            vec!["Employee".to_string(), "Manager".to_string()],
        ]
    );

    // Setting replaces rather than adds, and writing the same label
    // twice is one label.
    store
        .set_node_labels("person", rows[0], &["Manager", "Manager"])
        .unwrap();
    assert_eq!(
        store.node_labels("person").unwrap()[rows[0] as usize],
        ["Manager".to_string()]
    );

    // A label sits on a row, so deleting the row takes it with it: the
    // next row to take the number must not read it.
    store.delete_node("person", rows[0]).unwrap();
    assert!(store.node_labels("person").unwrap()[rows[0] as usize].is_empty());
}

/// Labels are held to the shape a name has, and to a row that exists.
#[test]
fn labels_are_checked_against_the_row_and_the_name() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    store.create_node_table("person", &[]).unwrap();
    let row = store.insert_node("person", &[]).unwrap();
    assert!(matches!(
        store
            .set_node_labels("person", 7, &["Employee"])
            .unwrap_err(),
        ZuError::InvalidArgument(_)
    ));
    assert!(matches!(
        store
            .set_node_labels("person", row, &["drop table"])
            .unwrap_err(),
        ZuError::InvalidArgument(_)
    ));
    assert!(store.node_labels("person").unwrap().is_empty());
}

/// A version 3 file has no label table, and reads as the file it is:
/// every node carrying the one label its table is called.
#[test]
fn version_three_files_gain_the_label_table() {
    let (_dir, path) = temp_db();
    {
        let mut store = SqliteStore::open(&path).unwrap();
        store.create_node_table("peer", &[]).unwrap();
        store.insert_node("peer", &[]).unwrap();
    }
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE zu_labels; PRAGMA user_version = 3;")
        .unwrap();
    drop(conn);

    let store = SqliteStore::open(&path).unwrap();
    let version: i32 = store
        .raw()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert!(store.node_labels("peer").unwrap().is_empty());
    store.set_node_labels("peer", 1, &["Admin"]).unwrap();
    assert_eq!(store.node_labels("peer").unwrap()[1], ["Admin".to_string()]);
}
