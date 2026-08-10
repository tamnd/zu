//! The docs/05 sections 3 through 5 surface over real database files:
//! the pragma profile, the lazy CSR cache with targeted invalidation,
//! IMMEDIATE transactions, the epoch marker, and the WAL truncating
//! checkpoint.

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use zu_sqlite::{ColumnType, Direction, GROUP_ROWS, SqliteStore, Value};

fn temp_db() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.db");
    (dir, path)
}

/// Six people, eight `knows` edges, all inside node group zero.
fn small_graph(store: &mut SqliteStore) {
    store
        .create_node_table("person", &[("name", ColumnType::Text)])
        .unwrap();
    store
        .create_rel_table("knows", &[("since", ColumnType::Integer)])
        .unwrap();
    for i in 1..=6i64 {
        store
            .insert_node("person", &[Value::Text(format!("p{i}"))])
            .unwrap();
    }
    for (src, dst) in [
        (1, 2),
        (1, 3),
        (2, 3),
        (2, 5),
        (3, 4),
        (4, 5),
        (5, 6),
        (6, 1),
    ] {
        store
            .insert_rel("knows", src, dst, &[Value::Int(2000)])
            .unwrap();
    }
}

fn pragma_i64(store_path: &PathBuf, pragma: &str) -> i64 {
    let conn = rusqlite::Connection::open(store_path).unwrap();
    conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))
        .unwrap()
}

#[test]
fn open_applies_the_full_pragma_profile() {
    let (_dir, path) = temp_db();
    let check = |store: &SqliteStore| {
        for (pragma, want) in [
            ("page_size", 8192),
            ("cache_size", -16384),
            ("mmap_size", 0),
            ("foreign_keys", 0),
            ("busy_timeout", 5000),
            ("wal_autocheckpoint", 2000),
            ("synchronous", 1),
        ] {
            let got: i64 = store
                .raw()
                .query_row(&format!("PRAGMA {pragma}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(got, want, "pragma {pragma}");
        }
        let mode: String = store
            .raw()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    };
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    check(&store);
    drop(store);
    // Page size is baked into the fresh file, and the per-connection
    // settings come back on every reopen.
    assert_eq!(pragma_i64(&path, "page_size"), 8192);
    let store = SqliteStore::open(&path).unwrap();
    check(&store);
}

#[test]
fn csr_cache_matches_sql_and_serves_hits_by_pointer() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    for dir in [Direction::Fwd, Direction::Bwd] {
        let csr = store.csr("knows", 0, dir).unwrap();
        assert_eq!(csr.edge_count(), 8);
        for row in 0..=7i64 {
            assert_eq!(
                csr.neighbors(row),
                store.neighbors("knows", row, dir).unwrap(),
                "row {row} {dir:?}"
            );
        }
        assert!(csr.neighbors(-1).is_empty());
        assert!(csr.neighbors(i64::from(GROUP_ROWS)).is_empty());
        let again = store.csr("knows", 0, dir).unwrap();
        assert!(
            Arc::ptr_eq(&csr, &again),
            "a clean group serves the cached build"
        );
    }
}

#[test]
fn writes_invalidate_exactly_the_touched_groups() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    // An edge landing in group one: adjacency rows need no node rows,
    // integrity lives above the engine.
    let far = i64::from(GROUP_ROWS) + 17;
    store.insert_rel("knows", far, 3, &[Value::Null]).unwrap();
    let g0_fwd = store.csr("knows", 0, Direction::Fwd).unwrap();
    let g0_bwd = store.csr("knows", 0, Direction::Bwd).unwrap();
    let g1_fwd = store.csr("knows", 1, Direction::Fwd).unwrap();
    assert_eq!(g1_fwd.neighbors(far), &[3]);
    // A second far edge bumps group one forward and group zero
    // backward; group zero forward is untouched and keeps its build.
    store.insert_rel("knows", far, 5, &[Value::Null]).unwrap();
    assert!(Arc::ptr_eq(
        &g0_fwd,
        &store.csr("knows", 0, Direction::Fwd).unwrap()
    ));
    let g1_after = store.csr("knows", 1, Direction::Fwd).unwrap();
    assert!(!Arc::ptr_eq(&g1_fwd, &g1_after));
    assert_eq!(g1_after.neighbors(far), &[3, 5]);
    let g0_bwd_after = store.csr("knows", 0, Direction::Bwd).unwrap();
    assert!(!Arc::ptr_eq(&g0_bwd, &g0_bwd_after));
    assert_eq!(g0_bwd_after.neighbors(5), &[2, 4, far]);
}

#[test]
fn immediate_txns_commit_rollback_and_move_the_epoch() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    let before = store.epoch().unwrap();
    store.begin().unwrap();
    store
        .insert_node("person", &[Value::Text("eve".into())])
        .unwrap();
    store.commit().unwrap();
    assert!(store.epoch().unwrap() > before, "a commit moves the epoch");
    assert_eq!(store.node_count("person").unwrap(), 7);
    store.begin().unwrap();
    store
        .insert_node("person", &[Value::Text("gone".into())])
        .unwrap();
    store.rollback().unwrap();
    assert_eq!(
        store.node_count("person").unwrap(),
        7,
        "a rollback leaves no row"
    );
}

#[test]
fn checkpoint_truncates_the_wal() {
    let (_dir, path) = temp_db();
    let mut store = SqliteStore::open(&path).unwrap();
    small_graph(&mut store);
    let wal = path.with_extension("db-wal");
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "writes land in the WAL first"
    );
    store.checkpoint().unwrap();
    assert_eq!(
        std::fs::metadata(&wal).unwrap().len(),
        0,
        "wal_checkpoint(TRUNCATE) empties the log"
    );
    assert_eq!(store.rel_count("knows").unwrap(), 8);
}
