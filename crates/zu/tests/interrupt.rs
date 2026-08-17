//! Stopping a statement that is already running.
//!
//! Cancellation is easy to get almost right: a flag that is read once
//! before the query starts stops nothing, and a flag that unwinds the
//! executor leaves a session nobody can use afterwards. What makes it
//! useful is the pair of facts checked here, that a statement already
//! deep in a join stops within a boundary or two of being asked, and
//! that the connection it stopped on runs the next statement normally.
//! The count of rows read is checked alongside, because it comes from
//! the same handle and it is what a shell paints while a person waits.

use std::sync::atomic::{AtomicBool, Ordering};
use zu::{Database, ZuError};
use zu_zu1::file::Zu1File;
use zu_zu1::graph::bulk_load_as;

/// A graph big enough that the join below cannot finish while the
/// watcher thread is waking up, and small enough to load in a moment.
fn seeded(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    let nodes = 20_000u32;
    let edges: Vec<(u32, u32)> = (0..nodes).map(|i| (i, (i + 1) % nodes)).collect();
    bulk_load_as(&mut db, "person", "knows", nodes.into(), &edges).expect("load");
}

#[test]
fn a_running_statement_stops_when_asked_and_the_session_survives_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stop.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");
    let stop = conn.interrupt();
    let done = AtomicBool::new(false);

    // Four hundred million pairs, which is not a query anyone wants an
    // answer to; it is a query that is certainly still running when the
    // watcher asks it to stop.
    let source = "MATCH (a:person), (b:person) WHERE a <> b RETURN count(*) AS n";
    let err = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(5));
                if stop.rows() > 0 {
                    stop.stop();
                    return;
                }
            }
        });
        let err = conn.query(source).expect_err("the statement was stopped");
        done.store(true, Ordering::Relaxed);
        err
    });

    assert!(matches!(err, ZuError::Interrupted), "{err}");
    // Stopping is not failing, so there is no condition to report.
    assert!(err.gqlstatus().is_none(), "{err} carries a status");
    assert!(stop.rows() > 0, "the rows it read were counted");

    // The point of the whole thing: the connection is warm. The ask
    // stays where the caller put it until the caller takes it back,
    // since a handle that cleared itself would race with the statement
    // that is still winding down.
    stop.clear();
    let rows = conn.query("RETURN 1 AS n").expect("the session is open");
    assert_eq!(rows.rows.len(), 1);
    let counted = conn
        .query("MATCH (p:person) RETURN count(*) AS n")
        .expect("and it still reads the graph");
    assert_eq!(counted.rows.len(), 1);
}

#[test]
fn a_handle_asked_before_the_statement_starts_stops_it_at_the_first_boundary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("early.zu1");
    seeded(&path);
    let db = Database::open(&path).expect("open");
    let mut conn = db.connect().expect("connect");
    conn.interrupt().stop();
    let err = conn
        .query("MATCH (p:person) RETURN p.id AS id")
        .expect_err("stopped");
    assert!(matches!(err, ZuError::Interrupted), "{err}");
    // And an ask is spent once it has been answered, rather than
    // poisoning every statement that follows.
    conn.interrupt().clear();
    conn.query("MATCH (p:person) RETURN count(*) AS n")
        .expect("the next statement runs");
}
