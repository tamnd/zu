//! Several sessions on one file in one process, which is what a
//! connection pool over an embedded store is, and what a benchmark
//! harness running a write query at concurrency drives it as.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use zu::dataset::{NodeFile, RelFile, load_dataset};
use zu::session::Session;

/// Accounts 10, 11 and 12, keyed, with two transfers between them.
fn fixture(dir: &Path) -> std::path::PathBuf {
    let at = |name: &str, body: &str| {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    };
    let account = at("Account.csv", "id:ID,name:STRING\n10,a\n11,b\n12,c\n");
    let transfer = at(
        "transfer.csv",
        ":START_ID,:END_ID,:TYPE,ts:INT64\n10,11,transfer,1\n11,12,transfer,2\n",
    );
    let path = dir.join("vis.zu1");
    load_dataset(
        &[NodeFile {
            table: "Account".into(),
            path: account,
        }],
        &[RelFile {
            table: "transfer".into(),
            from: "Account".into(),
            to: "Account".into(),
            path: transfer,
            undirected: false,
        }],
        &path,
    )
    .unwrap();
    path
}

/// A row one session committed is a row every other session of the same
/// file reads, and the key it took is taken as far as they are
/// concerned too.
#[test]
fn a_committed_row_is_visible_on_another_session_of_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let mut a = Session::open(&path).unwrap();
    let mut b = Session::open(&path).unwrap();
    a.run("INSERT (:Account {id: 20, name: 'new'})", &[])
        .unwrap();
    let rows = b
        .run("MATCH (x:Account {id: 20}) RETURN x.name", &[])
        .unwrap()
        .rows
        .into_vec();
    assert_eq!(
        rows.len(),
        1,
        "session b cannot see what session a committed"
    );
    b.run("INSERT (:Account {id: 20, name: 'again'})", &[])
        .expect_err("20 is the id of a row a already wrote");
}

/// A key the delete before it freed is a key the next row takes, and
/// the row that took it is the row a lookup by that key finds.
///
/// The index is the file's and a commit that did not fold rewrote
/// nothing, so between the two there is a moment where the index still
/// names the row that went and the row that holds the key is only in
/// the patch. Reading the index alone answers that the key names a row
/// that is gone, which reads as a free key, and every one of these
/// statements is asked about a row it would then not find.
#[test]
fn a_key_a_delete_freed_is_found_on_the_row_that_took_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let mut s = Session::open(&path).unwrap();
    // Through a fold, so that 900 is a key the index on disk holds.
    s.run("INSERT (:Account {id: 900, name: 'first'})", &[])
        .unwrap();
    s.run(
        "MATCH (a:Account {id: 10}), (b:Account {id: 900}) INSERT (a)-[:transfer {ts: 5}]->(b)",
        &[],
    )
    .unwrap();
    s.run("MATCH (b:Account {id: 900}) DETACH DELETE b", &[])
        .unwrap();

    s.run("INSERT (:Account {id: 900, name: 'second'})", &[])
        .unwrap();
    let by_key = s
        .run("MATCH (b:Account {id: 900}) RETURN b.name AS name", &[])
        .unwrap()
        .rows
        .into_vec();
    assert_eq!(
        by_key.len(),
        1,
        "the key names the row the delete freed it for"
    );
    assert_eq!(by_key[0][0], zu_query::exec::Value::Str("second".into()));
    let scanned = s
        .run(
            "MATCH (b:Account) WHERE b.name = 'second' RETURN count(b) AS n",
            &[],
        )
        .unwrap()
        .rows
        .into_vec();
    assert_eq!(scanned[0][0], zu_query::exec::Value::Int(1));
    // And the key is taken again, which is the other half of the same
    // answer: a lookup that finds nothing is a lookup that lets a
    // second row under one key through.
    s.run("INSERT (:Account {id: 900, name: 'third'})", &[])
        .expect_err("900 is the id of the row just written");
}

/// The shape a benchmark harness runs a bracketed write in: create the
/// scratch row, do the write over it, delete it again, and repeat, with
/// other writers on other sessions going the whole time.
///
/// Every one of those three statements may land on a different session,
/// because a pool hands out whichever connection is free. So the create
/// has to see the delete before it, on whatever session ran it, or the
/// table ends up with the same key twice. Twice is not a row that reads
/// wrong, it is a table that cannot be folded at all: the index over it
/// maps one key to one row and is rebuilt from the rows, so the rebuild
/// raises and every later read of the table raises with it.
#[test]
fn a_bracket_cycled_across_a_pool_never_leaves_a_key_twice() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let pool: Vec<_> = (0..8).map(|_| Session::open(&path).unwrap()).collect();
    let pool: Vec<_> = pool.into_iter().map(std::sync::Mutex::new).collect();
    let pool = Arc::new(pool);
    let turn = Arc::new(AtomicUsize::new(0));
    // One statement, on whichever session is next in the rotation.
    let one = |pool: &Arc<Vec<std::sync::Mutex<Session>>>, turn: &Arc<AtomicUsize>, text: &str| {
        let at = turn.fetch_add(1, Ordering::Relaxed) % pool.len();
        let mut sess = pool[at].lock().unwrap();
        sess.run(text, &[]).map(|_| ())
    };
    let noise = {
        let (pool, turn) = (Arc::clone(&pool), Arc::clone(&turn));
        std::thread::spawn(move || {
            for i in 0..200 {
                let _ = one(
                    &pool,
                    &turn,
                    &format!("MATCH (a:Account {{id: 10}}) SET a.name = 'n{i}'"),
                );
            }
        })
    };
    for i in 0..200 {
        one(&pool, &turn, "INSERT (:Account {id: 900, name: 'scratch'})")
            .unwrap_or_else(|e| panic!("rep {i} could not create the scratch row: {e}"));
        one(
            &pool,
            &turn,
            "MATCH (a:Account {id: 10}), (b:Account {id: 900}) \
             INSERT (a)-[:transfer {ts: 5}]->(b)",
        )
        .unwrap_or_else(|e| panic!("rep {i} could not write over it: {e}"));
        one(&pool, &turn, "MATCH (b:Account {id: 900}) DETACH DELETE b")
            .unwrap_or_else(|e| panic!("rep {i} could not take it away: {e}"));
        {
            let at = turn.fetch_add(1, Ordering::Relaxed) % pool.len();
            let mut sess = pool[at].lock().unwrap();
            let left = sess
                .run("MATCH (b:Account {id: 900}) RETURN count(b) AS n", &[])
                .unwrap_or_else(|e| panic!("rep {i} on session {at}: {e}"))
                .rows
                .into_vec();
            assert_eq!(
                left[0][0],
                zu_query::exec::Value::Int(0),
                "rep {i}: the scratch row is still there on session {at} after its delete"
            );
        }
    }
    noise.join().unwrap();
    // The table still folds and still reads, which is what a second row
    // under one key takes away.
    drop(pool);
    let mut after = Session::open(&path).unwrap();
    let rows = after
        .run("MATCH (a:Account) RETURN a.id AS id ORDER BY id", &[])
        .expect("the table still reads")
        .rows
        .into_vec();
    assert_eq!(rows.len(), 3, "the scratch row outlived its bracket");
}
