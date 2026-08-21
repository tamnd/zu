//! Differential parity for the write statements, against sqlite.
//!
//! `tests/differential.rs` drives the storage layer: it calls
//! `insert_nodes`, `update`, `delete` and `insert_rel` on a
//! transaction and compares what the overlays, folds and CSR rebuilds
//! hold against plain B-tree rows. That covers everything under the
//! write path and nothing of the write path itself, so the sinks a
//! statement compiles to, the batch a statement commits, and the
//! visibility rules a clause behind a write reads through are all
//! outside it.
//!
//! This file is the same idea one level up. A seeded workload of
//! `INSERT`, `SET`, `REMOVE`, `DETACH DELETE`, edge insert and `MERGE`
//! runs as statements on a session, the same mutations run as SQL on a
//! sqlite database next to it, and what a reader can see of the two is
//! compared: every person by key with their age and name, and every
//! edge as the pair of keys it joins. sqlite is the oracle for the
//! same reason it is one downstairs, that it has no overlay to be
//! wrong about.
//!
//! Elements are lined up by a `key` property rather than by row
//! offset. A zu node id is the offset the store handed out and a
//! sqlite rowid is whatever sqlite felt like, so comparing them would
//! be comparing two allocators; a key a statement wrote is the thing
//! both stores actually agree about, and it is also the level a
//! statement speaks at.
//!
//! Three things interrupt the run and all three are where a write path
//! goes wrong. A comparison mid-stream reads what the overlay is
//! carrying before anything has sealed it. A close and reopen folds
//! the patches into the file and reads them back out of it. And a
//! forget, which is this process dying with the fold still owed, reads
//! them back out of the log instead: every statement here is its own
//! transaction and returned only after its commit was durable, so a
//! crash may lose nothing at all.

use crate::session::Session;
use zu_query::exec::Value;

use crate::zu1::file::Zu1File;
use crate::zu1::graph::bulk_load_as;
use crate::zu1::props::{PropValues, store_props};

/// Rows the fixture starts with, and therefore the keys 0 to 11. Small
/// because the workload is what makes the store interesting, not the
/// load.
const BASE_ROWS: u64 = 12;
const BASE_EDGES: [(u32, u32); 8] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 4),
    (4, 5),
    (5, 0),
    (6, 7),
    (7, 6),
];
/// Statements per seed. Long enough that deletes, merges and edges
/// pile up on the same rows, short enough that three seeds of it stay
/// inside a couple of seconds.
const STEPS: u32 = 240;

/// splitmix64, the same one `tests/differential.rs` uses: deterministic,
/// seedable, and no dependency to pull in for it.
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

/// The seed graph: twelve people whose key is their row, with an age
/// and a name on each, and a knows table over the pairs above.
fn seed(path: &std::path::Path) {
    let mut db = Zu1File::create(path).expect("create");
    bulk_load_as(&mut db, "person", "knows", BASE_ROWS, &BASE_EDGES).expect("load");
    let keys: Vec<u64> = (0..BASE_ROWS).collect();
    let ages: Vec<u64> = (0..BASE_ROWS).map(|i| 20 + i * 3).collect();
    let names: Vec<Vec<u8>> = (0..BASE_ROWS)
        .map(|i| format!("p{i}").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    store_props(
        &mut db,
        "person",
        &[
            ("key", PropValues::Int(&keys)),
            ("age", PropValues::Int(&ages)),
            ("name", PropValues::Str(&refs)),
        ],
    )
    .expect("props");
}

/// The oracle, with the same three columns and the same edge pairs.
/// A plain table with a primary key on it, so a merge is an upsert and
/// a delete of a person is a delete of a person.
fn oracle() -> rusqlite::Connection {
    let sq = rusqlite::Connection::open_in_memory().expect("sqlite");
    sq.execute_batch(
        "CREATE TABLE person (key INTEGER PRIMARY KEY, age INTEGER, name TEXT);
         CREATE TABLE knows (a INTEGER NOT NULL, b INTEGER NOT NULL);",
    )
    .expect("schema");
    for i in 0..BASE_ROWS {
        sq.execute(
            "INSERT INTO person (key, age, name) VALUES (?, ?, ?)",
            rusqlite::params![i as i64, 20 + (i * 3) as i64, format!("p{i}")],
        )
        .expect("seed person");
    }
    for (a, b) in BASE_EDGES {
        sq.execute(
            "INSERT INTO knows (a, b) VALUES (?, ?)",
            rusqlite::params![i64::from(a), i64::from(b)],
        )
        .expect("seed edge");
    }
    sq
}

/// One person, as both stores describe them. The name is optional
/// because a `REMOVE` leaves the column holding nothing, and a
/// property nobody wrote and a property somebody cleared read the same
/// way on either side.
type Person = (i64, i64, Option<String>);

fn zu_people(session: &mut Session) -> Vec<Person> {
    let out = session
        .run(
            "MATCH (p:person) RETURN p.key AS k, p.age AS a, p.name AS n ORDER BY k",
            &[],
        )
        .expect("read people");
    out.rows
        .iter()
        .map(|row| {
            let key = match &row[0] {
                Value::Int(k) => *k,
                other => panic!("a key is an integer, got {other:?}"),
            };
            let age = match &row[1] {
                Value::Int(a) => *a,
                other => panic!("an age is an integer, got {other:?}"),
            };
            let name = match &row[2] {
                Value::Str(s) => Some(s.clone()),
                Value::Null => None,
                other => panic!("a name is a string or nothing, got {other:?}"),
            };
            (key, age, name)
        })
        .collect()
}

fn sq_people(sq: &rusqlite::Connection) -> Vec<Person> {
    let mut stmt = sq
        .prepare("SELECT key, age, name FROM person ORDER BY key")
        .expect("prepare people");
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query people");
    rows.map(|r| r.expect("row")).collect()
}

fn zu_edges(session: &mut Session) -> Vec<(i64, i64)> {
    let out = session
        .run(
            "MATCH (a:person)-[:knows]->(b:person) \
             RETURN a.key AS x, b.key AS y ORDER BY x, y",
            &[],
        )
        .expect("read edges");
    out.rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Int(x), Value::Int(y)) => (*x, *y),
            other => panic!("an edge joins two keys, got {other:?}"),
        })
        .collect()
}

fn sq_edges(sq: &rusqlite::Connection) -> Vec<(i64, i64)> {
    let mut stmt = sq
        .prepare(
            "SELECT k.a, k.b FROM knows k \
             JOIN person pa ON pa.key = k.a JOIN person pb ON pb.key = k.b \
             ORDER BY k.a, k.b",
        )
        .expect("prepare edges");
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query edges");
    rows.map(|r| r.expect("row")).collect()
}

/// Everything a reader can see, on both sides, at one moment.
fn compare(session: &mut Session, sq: &rusqlite::Connection, what: &str) {
    assert_eq!(zu_people(session), sq_people(sq), "{what}: the people");
    assert_eq!(zu_edges(session), sq_edges(sq), "{what}: the edges");
}

/// Which keys are still there, so the workload picks an element that
/// exists rather than writing three quarters of its statements against
/// nothing.
fn live_keys(sq: &rusqlite::Connection) -> Vec<i64> {
    let mut stmt = sq
        .prepare("SELECT key FROM person ORDER BY key")
        .expect("prepare keys");
    let rows = stmt.query_map([], |r| r.get(0)).expect("query keys");
    rows.map(|r| r.expect("row")).collect()
}

/// One seeded run: the same workload as statements on a session and as
/// SQL on the oracle, compared mid-stream, across a fold, and across a
/// crash.
fn run_seed(seed_value: u64) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("write-parity-{seed_value}.zu1"));
    seed(&path);
    let sq = oracle();
    let mut session = Session::open(&path).expect("open");
    let mut rng = Rng(seed_value);
    // Keys are handed out and never reused, so a key that was deleted
    // does not come back as somebody else and the two stores cannot
    // drift by agreeing on a number that means two things.
    let mut next_key = BASE_ROWS as i64;

    for step in 0..STEPS {
        let live = live_keys(&sq);
        if live.is_empty() {
            // The workload deleted the graph. Nothing below has an
            // element to run against, so put one back and carry on.
            let key = next_key;
            next_key += 1;
            session
                .run(
                    &format!("INSERT (p:person {{key: {key}, age: 30, name: 'refill'}})"),
                    &[],
                )
                .expect("refill");
            sq.execute(
                "INSERT INTO person (key, age, name) VALUES (?, 30, 'refill')",
                [key],
            )
            .expect("refill oracle");
            continue;
        }
        let pick = |rng: &mut Rng| live[rng.below(live.len() as u64) as usize];

        match rng.below(12) {
            // A row written from nothing.
            0 | 1 => {
                let key = next_key;
                next_key += 1;
                let age = rng.below(90) as i64;
                let name = format!("n{}", rng.below(10_000));
                session
                    .run(
                        &format!("INSERT (p:person {{key: {key}, age: {age}, name: '{name}'}})"),
                        &[],
                    )
                    .expect("insert");
                sq.execute(
                    "INSERT INTO person (key, age, name) VALUES (?, ?, ?)",
                    rusqlite::params![key, age, name],
                )
                .expect("insert oracle");
            }
            // A point write onto an integer column.
            2 | 3 => {
                let key = pick(&mut rng);
                let age = rng.below(90) as i64;
                session
                    .run(
                        &format!("MATCH (p:person) WHERE p.key = {key} SET p.age = {age}"),
                        &[],
                    )
                    .expect("set age");
                sq.execute(
                    "UPDATE person SET age = ? WHERE key = ?",
                    rusqlite::params![age, key],
                )
                .expect("set age oracle");
            }
            // And onto a string one, which lands in the blob rather
            // than the lane the word goes straight onto.
            4 => {
                let key = pick(&mut rng);
                let name = format!("u{}", rng.below(10_000));
                session
                    .run(
                        &format!("MATCH (p:person) WHERE p.key = {key} SET p.name = '{name}'"),
                        &[],
                    )
                    .expect("set name");
                sq.execute(
                    "UPDATE person SET name = ? WHERE key = ?",
                    rusqlite::params![name, key],
                )
                .expect("set name oracle");
            }
            // A property taken away, which is the assignment of a null
            // the standard says it is.
            5 => {
                let key = pick(&mut rng);
                session
                    .run(
                        &format!("MATCH (p:person) WHERE p.key = {key} REMOVE p.name"),
                        &[],
                    )
                    .expect("remove name");
                sq.execute("UPDATE person SET name = NULL WHERE key = ?", [key])
                    .expect("remove name oracle");
            }
            // An element and its edges, gone.
            6 => {
                let key = pick(&mut rng);
                session
                    .run(
                        &format!("MATCH (p:person) WHERE p.key = {key} DETACH DELETE p"),
                        &[],
                    )
                    .expect("detach delete");
                sq.execute("DELETE FROM knows WHERE a = ? OR b = ?", [key, key])
                    .expect("detach oracle");
                sq.execute("DELETE FROM person WHERE key = ?", [key])
                    .expect("delete oracle");
            }
            // An edge between two elements a match found, which may be
            // the same one twice and may be a pair that already has an
            // edge over it.
            7..=9 => {
                let a = pick(&mut rng);
                let b = pick(&mut rng);
                session
                    .run(
                        &format!(
                            "MATCH (x:person), (y:person) WHERE x.key = {a} AND y.key = {b} \
                             INSERT (x)-[:knows]->(y)"
                        ),
                        &[],
                    )
                    .expect("insert edge");
                sq.execute(
                    "INSERT INTO knows (a, b) VALUES (?, ?)",
                    rusqlite::params![a, b],
                )
                .expect("insert edge oracle");
            }
            // A merge, half of them onto a key that is there and half
            // onto one that is not, so both arms run and the mixed
            // statement is the one commit it should be.
            _ => {
                let key = if rng.below(2) == 0 {
                    pick(&mut rng)
                } else {
                    let key = next_key;
                    next_key += 1;
                    key
                };
                let age = rng.below(90) as i64;
                let name = format!("m{}", rng.below(10_000));
                session
                    .run(
                        &format!(
                            "MERGE (p:person {{key: {key}}}) \
                             ON CREATE SET p.age = {age}, p.name = '{name}' \
                             ON MATCH SET p.age = {age}"
                        ),
                        &[],
                    )
                    .expect("merge");
                sq.execute(
                    "INSERT INTO person (key, age, name) VALUES (?, ?, ?) \
                     ON CONFLICT (key) DO UPDATE SET age = excluded.age",
                    rusqlite::params![key, age, name],
                )
                .expect("merge oracle");
            }
        }

        if step % 17 == 16 {
            // Mid-stream: what the overlay is carrying, with nothing
            // having sealed it yet.
            compare(&mut session, &sq, &format!("seed {seed_value} step {step}"));
        }
        if step % 61 == 60 {
            // A close folds the patches onto the file, so the session
            // that comes back reads them out of the columns rather
            // than out of the patch.
            drop(session);
            session = Session::open(&path).expect("reopen");
            compare(
                &mut session,
                &sq,
                &format!("seed {seed_value} folded at {step}"),
            );
        }
    }

    compare(&mut session, &sq, &format!("seed {seed_value} final"));

    // The process dies here. Nothing folds and nothing publishes,
    // because the drop is what does both, so what comes back is what
    // the log holds; every statement above was its own transaction and
    // returned only once its commit was durable, so what the log holds
    // is all of it.
    std::mem::forget(session);
    crate::shared::forget(&path);
    let mut session = Session::open(&path).expect("reopen after a crash");
    compare(&mut session, &sq, &format!("seed {seed_value} recovered"));

    // And once more from the file the recovery wrote, which is the
    // read a process that starts tomorrow does.
    drop(session);
    let mut session = Session::open(&path).expect("reopen after recovery");
    compare(&mut session, &sq, &format!("seed {seed_value} reopened"));
}

/// The write statements against the oracle, over three seeds.
///
/// Three rather than one because the interesting states are the ones
/// where a delete, a merge and an edge insert land on the same row in
/// some order, and one seed only ever finds the orders it happens to
/// draw.
#[test]
fn seeded_write_statements_agree_with_the_oracle() {
    for seed_value in [11, 2027, 99_137] {
        run_seed(seed_value);
    }
}
