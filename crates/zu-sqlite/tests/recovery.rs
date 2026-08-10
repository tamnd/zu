//! The sqlite row of the docs/08 section 7 recovery matrix. The row
//! delegates crash recovery to SQLite's own WAL recovery, so the test
//! builds a real crash image, the database file plus a hot WAL that
//! was never checkpointed, and proves a fresh open replays the log:
//! every committed transaction is visible and nothing else is.

use zu_sqlite::{ColumnType, SqliteStore, Value};

#[test]
fn a_hot_wal_replays_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let live = dir.path().join("live.db");
    let crashed = dir.path().join("crashed.db");

    let mut store = SqliteStore::open(&live).unwrap();
    store
        .create_node_table("person", &[("name", ColumnType::Text)])
        .unwrap();
    store
        .create_rel_table("knows", "person", "person", &[])
        .unwrap();
    store.begin().unwrap();
    for i in 0..6i64 {
        store
            .insert_node_at("person", i, &[Value::Text(format!("p{i}"))])
            .unwrap();
    }
    store.insert_rel("knows", 0, 1, &[]).unwrap();
    store.insert_rel("knows", 1, 2, &[]).unwrap();
    store.commit().unwrap();

    // The commit landed in the WAL, not the main file; a crash here
    // loses nothing only if recovery replays the log.
    let wal = live.with_extension("db-wal");
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "the committed transaction must still sit in the WAL"
    );

    // An open transaction at crash time: its writes must vanish.
    store.begin().unwrap();
    store
        .insert_node_at("person", 6, &[Value::Text("gone".into())])
        .unwrap();

    // The crash image: file and hot WAL copied out from under the live
    // connection, which never closes cleanly and never checkpoints.
    std::fs::copy(&live, &crashed).unwrap();
    std::fs::copy(&wal, crashed.with_extension("db-wal")).unwrap();

    // Control: the same file without its WAL has none of the data,
    // so whatever the recovered store answers came from log replay.
    let bare = dir.path().join("bare.db");
    std::fs::copy(&live, &bare).unwrap();
    let control = SqliteStore::open(&bare).unwrap();
    assert!(
        control.node_count("person").is_err(),
        "without the WAL the person table does not exist yet"
    );

    let recovered = SqliteStore::open(&crashed).unwrap();
    assert_eq!(
        recovered.node_count("person").unwrap(),
        6,
        "committed rows survive, the open transaction leaves none"
    );
    assert_eq!(recovered.rel_count("knows").unwrap(), 2);
    assert_eq!(
        recovered.read_node_prop("person", 5, "name").unwrap(),
        Value::Text("p5".into())
    );
}
