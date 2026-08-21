//! Reading a result in batches instead of all at once, the way `dx/04`
//! §4 writes it.
//!
//! What is checked here is that the streamed answer is the buffered
//! answer: the same rows in the same order, with SKIP and LIMIT spent
//! across batches rather than inside each one, and the same columns
//! reported afterwards. The rest is the two things streaming exists
//! for, that a caller can stop early and that the statements which
//! cannot stream still arrive through the same loop.

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_as;
use zu::{Database, Flow};

mod pin;

const NODES: u32 = 500;

/// Rows enough for a scan to advance in several steps rather than one,
/// which is what a test about stopping partway needs: a run that hands
/// its whole table over in a single step has no partway to stop at.
const MANY: u32 = 8 * 1024;

fn seeded(path: &std::path::Path, nodes: u32) {
    let mut db = Zu1File::create(path).expect("create");
    let mut edges: Vec<(u32, u32)> = (0..nodes)
        .flat_map(|i| [(i, (i + 1) % nodes), (i, (i + 7) % nodes)])
        .collect();
    edges.sort_unstable();
    bulk_load_as(&mut db, "person", "knows", nodes.into(), &edges).expect("load");
}

fn opened(name: &str) -> (tempfile::TempDir, Database) {
    opened_with(name, NODES)
}

fn opened_with(name: &str, nodes: u32) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    seeded(&path, nodes);
    let db = Database::open(&path).expect("open");
    (dir, db)
}

/// Every row a statement returns, read the ordinary way, so a streamed
/// run has something to be equal to.
fn buffered(conn: &mut zu::Connection, source: &str) -> Vec<i64> {
    conn.query(source)
        .expect("query")
        .iter()
        .map(|row| row.get_at::<i64>(0).expect("an integer column"))
        .collect()
}

#[test]
fn a_streamed_statement_gives_the_rows_the_buffered_one_gives_in_batches() {
    let _reading = pin::reading();
    let (_dir, db) = opened("stream.zu1");
    let mut conn = db.connect().expect("connect");

    let source = "MATCH (p:person) RETURN p.id AS id";
    let want = buffered(&mut conn, source);
    assert_eq!(want.len(), NODES as usize);

    let mut got = Vec::new();
    let mut sizes = Vec::new();
    let out = conn
        .query_stream_batched(source, &[], 64, |batch| {
            sizes.push(batch.len());
            for row in batch.iter() {
                got.push(row.get_at::<i64>(0).expect("an integer column"));
            }
            Ok(Flow::More)
        })
        .expect("stream");

    assert_eq!(got, want);
    assert_eq!(out.rows, NODES as u64);
    assert_eq!(out.columns, vec!["id".to_string()]);
    assert!(out.streamed, "a plain scan streams");
    assert!(!out.stopped);
    // Rows arrived in pieces rather than one piece, and no piece was
    // larger than the size that was asked for, which is what bounds
    // what the caller is holding.
    assert!(sizes.len() > 1, "{sizes:?}");
    assert!(sizes.iter().all(|&n| n > 0 && n <= 64), "{sizes:?}");
    assert_eq!(sizes.iter().sum::<usize>(), want.len());
}

#[test]
fn a_sink_that_stops_stops_the_scan_under_it() {
    let _reading = pin::reading();
    let (_dir, db) = opened("stop.zu1");
    let mut conn = db.connect().expect("connect");

    let mut batches = 0;
    let out = conn
        .query_stream_batched("MATCH (p:person) RETURN p.id AS id", &[], 32, |batch| {
            batches += 1;
            assert_eq!(batch.len(), 32);
            Ok(Flow::Stop)
        })
        .expect("stream");

    assert_eq!(batches, 1);
    assert_eq!(out.rows, 32);
    assert!(out.stopped);
    assert!(out.streamed);

    // The connection is not left holding anything by the stop: the next
    // statement on it reads the whole table.
    let all = buffered(&mut conn, "MATCH (p:person) RETURN p.id AS id");
    assert_eq!(all.len(), NODES as usize);
}

#[test]
fn skip_and_limit_are_spent_across_batches_and_not_inside_each_one() {
    let _reading = pin::reading();
    let (_dir, db) = opened("window.zu1");
    let mut conn = db.connect().expect("connect");

    // The batch is smaller than the skip and smaller than the limit, so
    // a window applied per batch would give a different answer than the
    // buffered one and this would catch it.
    let source = "MATCH (p:person) RETURN p.id AS id SKIP 70 LIMIT 100";
    let want = buffered(&mut conn, source);
    assert_eq!(want.len(), 100);

    let mut got = Vec::new();
    let out = conn
        .query_stream_batched(source, &[], 16, |batch| {
            for row in batch.iter() {
                got.push(row.get_at::<i64>(0).expect("an integer column"));
            }
            Ok(Flow::More)
        })
        .expect("stream");

    assert_eq!(got, want);
    assert_eq!(out.rows, 100);
    assert!(!out.stopped, "the limit ran out, the caller did not stop");

    // A limit of zero returns nothing and calls the sink no times,
    // rather than calling it once with an empty batch.
    let mut called = 0;
    let out = conn
        .query_stream("MATCH (p:person) RETURN p.id AS id LIMIT 0", &[], |_| {
            called += 1;
            Ok(Flow::More)
        })
        .expect("stream");
    assert_eq!(called, 0);
    assert_eq!(out.rows, 0);
}

#[test]
fn a_statement_that_must_see_every_row_first_arrives_in_batches_anyway() {
    let _reading = pin::reading();
    let (_dir, db) = opened("ordered.zu1");
    let mut conn = db.connect().expect("connect");

    // ORDER BY does not know its first row until it has read its last,
    // so this one is buffered and cut into batches afterwards. The
    // caller's loop is the same and `streamed` is what says so.
    let source = "MATCH (p:person) RETURN p.id AS id ORDER BY id DESC";
    let want = buffered(&mut conn, source);

    let mut got = Vec::new();
    let mut sizes = Vec::new();
    let out = conn
        .query_stream_batched(source, &[], 128, |batch| {
            sizes.push(batch.len());
            for row in batch.iter() {
                got.push(row.get_at::<i64>(0).expect("an integer column"));
            }
            Ok(Flow::More)
        })
        .expect("stream");

    assert_eq!(got, want);
    assert_eq!(got[0], i64::from(NODES) - 1);
    assert_eq!(out.rows, NODES as u64);
    assert!(!out.streamed, "ORDER BY cannot stream");
    assert!(sizes.len() > 1, "{sizes:?}");

    // An aggregate is the same case for the same reason, and one group
    // is still one batch.
    let mut rows = 0;
    let out = conn
        .query_stream("MATCH (p:person) RETURN count(p) AS n", &[], |batch| {
            rows += batch.len();
            Ok(Flow::More)
        })
        .expect("stream");
    assert_eq!(rows, 1);
    assert!(!out.streamed);
}

/// The two executors both stream, and a caller cannot be able to tell
/// which one ran its statement. The pipeline executor hands a morsel
/// over at a time and the old one hands a chunk over at a time, so the
/// batch boundaries are theirs to pick, but the rows and their order
/// are not.
#[test]
fn the_two_executors_stream_the_same_rows_in_the_same_order() {
    let (_dir, db) = opened("parity.zu1");
    let mut conn = db.connect().expect("connect");

    let statements = [
        "MATCH (p:person) RETURN p.id AS id",
        "MATCH (p:person) WHERE p.id % 7 = 0 RETURN p.id AS id",
        "MATCH (p:person) RETURN p.id AS id SKIP 33 LIMIT 111",
        "MATCH (p:person)-[:knows]->(f) RETURN f.id AS id LIMIT 250",
    ];
    let read = |conn: &mut zu::Connection, source: &str| {
        let mut got = Vec::new();
        let out = conn
            .query_stream_batched(source, &[], 48, |batch| {
                for row in batch.iter() {
                    got.push(row.get_at::<i64>(0).expect("an integer column"));
                }
                Ok(Flow::More)
            })
            .expect("stream");
        (got, out.rows, out.streamed)
    };

    for source in statements {
        let pipeline = {
            let _reading = pin::reading();
            read(&mut conn, source)
        };
        // Pinned rather than set, because the variable is the
        // process's and the tests beside this one are running
        // statements while it is held.
        let old = pin::pinned("ZU_EXEC2", "0", || read(&mut conn, source));
        assert_eq!(pipeline.0, old.0, "{source}");
        assert_eq!(pipeline.1, old.1, "{source}");
        assert!(pipeline.2 && old.2, "both streamed {source}");
        assert!(!pipeline.0.is_empty(), "{source} returned nothing");
    }
}

/// A stop the caller asked for is a failure, and a stop the sink asked
/// for is an answer. Both end the scan partway, and telling them apart
/// is the whole of what a caller can do about it: a truncated result
/// reported as a whole one is the one outcome nobody can defend
/// against, because there is nothing in it to notice.
///
/// Both executors, because a client is what raises this and a client
/// does not know which one took its statement.
#[test]
fn a_statement_interrupted_partway_through_a_stream_says_so() {
    let (_dir, db) = opened_with("interrupt.zu1", MANY);
    let mut conn = db.connect().expect("connect");
    let interrupt = conn.session_mut().interrupt();

    let stopping = |conn: &mut zu::Connection| {
        let mut rows = 0u64;
        let out =
            conn.query_stream_batched("MATCH (p:person) RETURN p.id AS id", &[], 16, |batch| {
                rows += batch.len() as u64;
                // From inside the sink, which is where a signal fires
                // in a client handing rows to a caller: the statement
                // is between batches and most of the table is left.
                interrupt.stop();
                Ok(Flow::More)
            });
        (rows, out)
    };

    for engine in ["1", "0"] {
        let (rows, out) = pin::pinned("ZU_EXEC2", engine, || stopping(&mut conn));
        let err = out.expect_err("the statement was interrupted");
        assert!(matches!(err, zu::ZuError::Interrupted), "{engine}: {err}");
        assert!(
            rows < u64::from(MANY),
            "{engine}: {rows} rows is the whole scan"
        );
        interrupt.clear();
    }

    // Exactly as it was: a statement that was stopped leaves the
    // session holding nothing, so the next one reads the whole table.
    let all = buffered(&mut conn, "MATCH (p:person) RETURN p.id AS id");
    assert_eq!(all.len(), MANY as usize);
}

#[test]
fn a_streamed_statement_takes_parameters_and_a_failing_sink_fails_the_call() {
    let _reading = pin::reading();
    let (_dir, db) = opened("params.zu1");
    let mut conn = db.connect().expect("connect");

    let mut got = Vec::new();
    let out = conn
        .query_stream(
            "MATCH (p:person) WHERE p.id > $floor RETURN p.id AS id",
            &[("floor", Value::Int(i64::from(NODES) - 4))],
            |batch| {
                for row in batch.iter() {
                    got.push(row.get_at::<i64>(0).expect("an integer column"));
                }
                Ok(Flow::More)
            },
        )
        .expect("stream");
    assert_eq!(got, vec![497, 498, 499]);
    assert_eq!(out.rows, 3);

    // The error a sink raises is the error the call returns, unchanged,
    // because a caller that could not write a batch somewhere wants its
    // own condition back rather than one the engine invented for it.
    let err = conn
        .query_stream("MATCH (p:person) RETURN p.id AS id", &[], |_| {
            Err(zu::ZuError::InvalidArgument(
                "the writer is full".to_string(),
            ))
        })
        .expect_err("the sink failed");
    assert!(err.to_string().contains("the writer is full"), "{err}");
}
