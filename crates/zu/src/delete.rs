//! Running a `DELETE`: the elements the statement named, and the check
//! that says whether they can go.
//!
//! This sits in the same seam [`crate::set`] and [`crate::insert`] do,
//! for the same reason: the executor reads through a graph and a graph
//! reads, so the session runs the write between two plans. A delete
//! carries the row it was given across itself, because it binds nothing
//! and it leaves the names it took away named.
//!
//! What a delete cannot do is leave an edge pointing at nothing. Every
//! edge in the file names its endpoints by row offset, so an element
//! that still has edges is refused with G1001 rather than deleted, and
//! `DETACH DELETE`, which is the way GQL says take the edges too, is a
//! piece of its own that waits on an edge being deletable at all.
//!
//! A delete does not compact either: the row keeps its offset and the
//! fold writes that offset into the table's tombstone chain, which is
//! what [`crate::deleted`] reads back and every scan filters by.

use std::collections::{BTreeSet, HashMap};

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};

use crate::insert::describe;
use crate::query::Value;
use crate::split::Delete;
use crate::zu1::catalog::Catalog;
use crate::zu1::file::Zu1File;
use crate::zu1::graph::{Direction, GraphReader};
use crate::zu1::txn::WriteTxn;

/// The elements one delete is taking away, filled a row at a time.
///
/// A write runs once for every row the clauses before it answered, so
/// this is opened once per statement and asked for every one of them.
/// The rows are a set, because a statement can name one element twice
/// and taking it away twice is taking it away once.
///
/// Nothing is written here: a statement that cannot be written has to
/// raise before the transaction opens, so an element that still has
/// edges costs no log write and no fold.
pub(crate) struct Removals<'a> {
    write: &'a Delete,
    /// The catalog as it stood when the write opened, which is what says
    /// where an element's edges could be. Owned because the file it was
    /// read out of is handed in a row at a time.
    catalog: Catalog,
    /// The rel tables whose adjacency has had to be read, loaded once
    /// each. Which ones those are depends on the tables the elements
    /// turn up in, and a slot the write names is bound by a match that
    /// can leave several tables open.
    readers: HashMap<u32, GraphReader>,
    rows: BTreeSet<(u32, u64)>,
}

impl<'a> Removals<'a> {
    pub(crate) fn open(write: &'a Delete, catalog: Catalog) -> Self {
        Self {
            write,
            catalog,
            readers: HashMap::new(),
            rows: BTreeSet::new(),
        }
    }

    /// Works out what one row of the run takes away: `carried` is the
    /// row the clauses before the write answered, holding the slots in
    /// [`Delete::carry`] in that order. A delete computes nothing, so
    /// there is nothing behind the row.
    pub(crate) fn row(&mut self, db: &mut Zu1File, carried: &[Value]) -> Result<()> {
        for &slot in &self.write.slots {
            let value = self
                .write
                .carry
                .iter()
                .position(|carried_slot| *carried_slot == slot)
                .map(|at| &carried[at])
                .ok_or_else(|| {
                    ZuError::InvalidArgument(
                        "an element is being deleted that no clause of the statement binds".into(),
                    )
                })?;
            let (table, offset) = match value {
                Value::Node { table, offset } => (*table, *offset),
                // An OPTIONAL MATCH that found nothing binds null, and
                // taking nothing away is what the statement asked for.
                Value::Null => continue,
                other => {
                    return Err(ZuError::gql(
                        codes::C22G03,
                        format!(
                            "DELETE takes away an element, and this one is {}",
                            describe(other)
                        ),
                    ));
                }
            };
            self.detached(db, table, offset)?;
            self.rows.insert((table, offset));
        }
        Ok(())
    }

    /// Raises unless the element has no edges left on it.
    ///
    /// Every rel table that starts or ends in the element's table is a
    /// place an edge on it can be, and an undirected table is both, so
    /// each one is asked for the degree of the row in the direction it
    /// could hold it. `DELETE` without `DETACH` is defined to refuse an
    /// element that still has one (G1001), and the message names a table
    /// the edges are in so the reader knows what to detach or delete.
    fn detached(&mut self, db: &mut Zu1File, table: u32, offset: u64) -> Result<()> {
        let rels: Vec<(u32, bool, bool)> = self
            .catalog
            .rel_tables()
            .iter()
            .filter(|rel| rel.from == table || rel.to == table)
            .map(|rel| (rel.id, rel.from == table, rel.to == table))
            .collect();
        for (rel, out, back) in rels {
            self.ensure_reader(db, rel)?;
            let reader = self.readers.get(&rel).expect("just loaded");
            let mut edges = 0;
            if out {
                edges += reader.degree_of(db, offset, Direction::Fwd)?;
            }
            if back {
                edges += reader.degree_of(db, offset, Direction::Bwd)?;
            }
            if edges > 0 {
                let name = &self.catalog.rel_by_id(rel).expect("named the table").name;
                return Err(ZuError::gql(
                    codes::CG1001,
                    format!(
                        "the element still has {edges} edges in '{name}', and DELETE without DETACH does not take an edge away"
                    ),
                ));
            }
        }
        Ok(())
    }

    fn ensure_reader(&mut self, db: &mut Zu1File, rel: u32) -> Result<()> {
        if self.readers.contains_key(&rel) {
            return Ok(());
        }
        let name = self
            .catalog
            .rel_by_id(rel)
            .ok_or_else(|| ZuError::InvalidArgument(format!("unknown rel table {rel}")))?
            .name
            .clone();
        self.readers
            .insert(rel, GraphReader::load_table(db, &name)?);
        Ok(())
    }

    /// What the whole run takes away, by table and then by row.
    pub(crate) fn staged(self) -> Vec<(u32, u64)> {
        self.rows.into_iter().collect()
    }
}

/// Stages every removal of one statement in the open transaction.
///
/// The rows arrive deduplicated, so the count is the number of elements
/// the statement took away and not the number of times it named them.
pub(crate) fn stage(txn: &mut WriteTxn<'_>, rows: &[(u32, u64)]) -> Result<u64> {
    for &(table, offset) in rows {
        txn.delete(table, offset);
    }
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::session::Session;
    use crate::zu1::graph::bulk_load_as;
    use crate::zu1::props::{PropValues, store_props};

    use super::*;

    /// Three people and one edge, so the fixture holds both an element
    /// a delete can take and elements it has to refuse.
    fn seeded(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        bulk_load_as(&mut db, "person", "knows", 3, &[(0, 1)]).expect("load");
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"zoe"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20, 30])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .expect("props");
    }

    fn open(dir: &tempfile::TempDir, name: &str) -> Session {
        let path = dir.path().join(name);
        seeded(&path);
        Session::open(&path).expect("open")
    }

    fn names(session: &mut Session) -> Vec<String> {
        session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read")
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Str(s) => s.clone(),
                other => panic!("expected a string, got {other:?}"),
            })
            .collect()
    }

    /// The statement the milestone line is about: a match says which
    /// element goes, it goes, and nothing after that finds it.
    #[test]
    fn an_element_with_no_edges_is_taken_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete.zu1");

        let out = session
            .run("MATCH (p:person {name: 'zoe'}) DELETE p", &[])
            .expect("delete");
        assert!(out.rows.is_empty(), "a write with nothing after it");

        assert_eq!(names(&mut session), ["ada", "kay"]);
        let count = session
            .run("MATCH (p:person) RETURN count(*) AS n", &[])
            .expect("count");
        assert_eq!(count.rows[0][0], Value::Int(2));
    }

    /// The key still names the row it always named, because offsets do
    /// not move, so what says the row is gone is the tombstone and a
    /// lookup by key has to read it too.
    #[test]
    fn a_key_no_longer_finds_what_was_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-key.zu1");

        session
            .run("MATCH (p:person {name: 'zoe'}) DELETE p", &[])
            .expect("delete");

        let gone = session
            .run("MATCH (p:person {id: 2}) RETURN p.name AS name", &[])
            .expect("read");
        assert!(gone.rows.is_empty(), "the key names a row that is gone");
        let kept = session
            .run("MATCH (p:person {id: 1}) RETURN p.name AS name", &[])
            .expect("read");
        assert_eq!(kept.rows[0][0], Value::Str("kay".into()));
    }

    /// An element with edges on it cannot go, because every edge names
    /// its endpoints by offset and nothing would be left to name.
    #[test]
    fn an_element_that_still_has_edges_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-edges.zu1");

        let err = session
            .run("MATCH (p:person {name: 'ada'}) DELETE p", &[])
            .expect_err("ada still knows kay");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("G1001"),
            "got: {err}"
        );
        assert_eq!(names(&mut session), ["ada", "kay", "zoe"]);

        // The other end of the same edge is no more deletable than the
        // end it leaves, which is what reading the backward direction
        // is for.
        let err = session
            .run("MATCH (p:person {name: 'kay'}) DELETE p", &[])
            .expect_err("kay is known by ada");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("G1001"),
            "got: {err}"
        );
    }

    /// Naming an element twice takes it away once, and the count the
    /// statement ran on is the elements and not the namings.
    #[test]
    fn naming_the_same_element_twice_takes_it_away_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-twice.zu1");

        session
            .run("MATCH (p:person {name: 'zoe'}) DELETE p, p", &[])
            .expect("delete");
        assert_eq!(names(&mut session), ["ada", "kay"]);
    }

    /// The tombstone is folded into the file, so the row is still gone
    /// after the session that deleted it has closed.
    #[test]
    fn what_a_delete_took_away_stays_away_after_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("delete-reopen.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person {name: 'zoe'}) DELETE p", &[])
            .expect("delete");
        drop(session);

        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(names(&mut session), ["ada", "kay"]);
    }

    /// A name no clause bound stands for nothing to take away, and the
    /// binder says so rather than the run.
    #[test]
    fn deleting_a_name_nothing_bound_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-unbound.zu1");

        let err = session
            .run("MATCH (p:person) DELETE q", &[])
            .expect_err("q stands for nothing");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("42002"),
            "got: {err}"
        );
    }
}
