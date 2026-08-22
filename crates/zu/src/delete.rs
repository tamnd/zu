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
//! edge in the file names its endpoints by row offset, so a plain
//! `DELETE` of an element that still has edges is refused with G1001,
//! and `DETACH DELETE` is the form that takes the edges away first. An
//! edge the statement named itself is deleted either way: it has no
//! edges on it, and the rows it ran between stay where they were.
//!
//! A delete does not compact either: the row keeps its offset and the
//! fold writes that offset into the table's tombstone chain, which is
//! what [`crate::deleted`] reads back and every scan filters by.

use std::collections::BTreeSet;

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};

use crate::insert::describe;
use crate::query::{Value, Zu1Graph};
use crate::split::Delete;
use crate::zu1::graph::Direction;
use crate::zu1::txn::WriteTxn;

/// One element a delete takes away: the table it is in and its offset.
type Row = (u32, u64);

/// One edge a detach takes away: the rel table it is in and the two
/// rows it runs between, which is the only name an edge has.
type Edge = (u32, u64, u64);

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
    rows: BTreeSet<Row>,
    /// The edges a `DETACH` is taking away, as the rel table and the two
    /// rows each one runs between. A set for the same reason the rows
    /// are: both ends of an edge can be deleted by the same statement,
    /// and an edge on a loop is read once forward and once backward.
    edges: BTreeSet<Edge>,
}

impl<'a> Removals<'a> {
    pub(crate) fn open(write: &'a Delete) -> Self {
        Self {
            write,
            rows: BTreeSet::new(),
            edges: BTreeSet::new(),
        }
    }

    /// Works out what one row of the run takes away: `carried` is the
    /// row the clauses before the write answered, holding the slots in
    /// [`Delete::carry`] in that order. A delete computes nothing, so
    /// there is nothing behind the row.
    pub(crate) fn row(&mut self, graph: &mut Zu1Graph<'_>, carried: &[Value]) -> Result<()> {
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
                })?
                .clone();
            self.element(graph, &value)?;
        }
        Ok(())
    }

    /// Takes one element away, whichever kind of delete item named it.
    /// A variable reads it out of the row and a `VALUE { ... }` gets it
    /// from the query it ran, and from here on the two are the same
    /// thing.
    pub(crate) fn element(&mut self, graph: &mut Zu1Graph<'_>, value: &Value) -> Result<()> {
        let (table, offset) = match value {
            Value::Node { table, offset } => (*table, *offset),
            // An edge is named by the rows it runs between, and
            // that is the whole of it: nothing has to be checked,
            // because an edge has no edges on it, and nothing else
            // is staged, because both ends stay.
            Value::Rel {
                table, src, dst, ..
            } => {
                self.edges.insert((*table, *src, *dst));
                return Ok(());
            }
            // An OPTIONAL MATCH that found nothing binds null, and
            // taking nothing away is what the statement asked for.
            Value::Null => return Ok(()),
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
        self.detach(graph, table, offset)?;
        self.rows.insert((table, offset));
        Ok(())
    }

    /// Takes the edges on the element away, or raises if the statement
    /// did not say to and there are any.
    ///
    /// Every rel table that starts or ends in the element's table is a
    /// place an edge on it can be, and an undirected table is both, so
    /// each one is asked in the direction it could hold one. A `DETACH`
    /// stages what it finds; a plain `DELETE` is defined to refuse an
    /// element that still has an edge (G1001), and its message names a
    /// table the edges are in so the reader knows what to detach.
    fn detach(&mut self, graph: &mut Zu1Graph<'_>, table: u32, offset: u64) -> Result<()> {
        // Worked out before anything is read, because reading takes the
        // graph and the catalog is the graph's.
        let rels: Vec<(u32, bool, bool)> = graph
            .catalog()
            .rel_tables()
            .iter()
            .filter(|rel| rel.from == table || rel.to == table)
            .map(|rel| (rel.id, rel.from == table, rel.to == table))
            .collect();
        let mut ends = Vec::new();
        for (rel, out, back) in rels {
            if self.write.detach {
                // Read out rather than counted, because what the edges
                // are is what has to be staged, and the count of them
                // is only interesting to the statement that refuses.
                if out {
                    graph.ends_of(rel, offset, Direction::Fwd, &mut ends)?;
                    self.edges
                        .extend(ends.drain(..).map(|dst| (rel, offset, dst)));
                }
                if back {
                    graph.ends_of(rel, offset, Direction::Bwd, &mut ends)?;
                    self.edges
                        .extend(ends.drain(..).map(|src| (rel, src, offset)));
                }
                continue;
            }
            let mut edges = 0;
            if out {
                edges += graph.edges_on(rel, offset, Direction::Fwd)?;
            }
            if back {
                edges += graph.edges_on(rel, offset, Direction::Bwd)?;
            }
            if edges > 0 {
                let name = &graph
                    .catalog()
                    .rel_by_id(rel)
                    .expect("named the table")
                    .name;
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

    /// What the whole run takes away: the rows, by table and then by
    /// row, and the edges a `DETACH` is taking with them.
    pub(crate) fn staged(self) -> (Vec<Row>, Vec<Edge>) {
        (
            self.rows.into_iter().collect(),
            self.edges.into_iter().collect(),
        )
    }
}

/// Stages every removal of one statement in the open transaction.
///
/// The rows arrive deduplicated, so the count is the number of elements
/// the statement took away and not the number of times it named them.
/// The edges go first, which is the order the log wants them in as
/// well: a row that is gone must never be the endpoint of an edge that
/// is still there, on either side of a crash.
pub(crate) fn stage(txn: &mut WriteTxn<'_>, rows: &[Row], edges: &[Edge]) -> Result<u64> {
    for &(rel, src, dst) in edges {
        txn.delete_rel(rel, src, dst);
    }
    for &(table, offset) in rows {
        txn.delete(table, offset);
    }
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::session::Session;
    use crate::zu1::file::Zu1File;
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

    /// The strings of the first column of one answer.
    fn names_of(out: &crate::query::QueryResult) -> Vec<String> {
        out.rows
            .iter()
            .map(|row| match &row[0] {
                Value::Str(s) => s.clone(),
                other => panic!("expected a string, got {other:?}"),
            })
            .collect()
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

    /// An edge a commit added and no fold has written into the CSR is
    /// an edge all the same, so the element it is on cannot go without
    /// a DETACH and a DETACH takes it with the element.
    ///
    /// This is what says the delete reads through the overlay. It reads
    /// the session's adjacency readers rather than loading its own, and
    /// those readers only see an unfolded edge because the writer hands
    /// them the patch, so a delete that read past the patch would let
    /// this statement through and leave the edge pointing at a row that
    /// is gone.
    #[test]
    fn an_edge_no_fold_has_landed_still_holds_the_element() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-unfolded.zu1");

        session
            .run(
                "MATCH (a:person {name: 'kay'}), (b:person {name: 'zoe'}) INSERT (a)-[:knows]->(b)",
                &[],
            )
            .expect("insert an edge");

        let err = session
            .run("MATCH (p:person {name: 'zoe'}) DELETE p", &[])
            .expect_err("kay knows zoe now");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("G1001"),
            "got: {err}"
        );

        session
            .run("MATCH (p:person {name: 'zoe'}) DETACH DELETE p", &[])
            .expect("detach delete");
        assert_eq!(names(&mut session), ["ada", "kay"]);
        let left = session
            .run(
                "MATCH (:person)-[:knows]->(:person) RETURN count(*) AS n",
                &[],
            )
            .expect("count");
        assert_eq!(left.rows[0][0], Value::Int(1), "ada still knows kay");
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

    /// An edge the statement named itself goes, and the two elements it
    /// ran between stay. Nothing about that needs DETACH: what DETACH
    /// says is that the edges on an element go too, and an edge has
    /// none on it.
    #[test]
    fn an_edge_the_statement_named_is_taken_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-edge.zu1");

        session
            .run(
                "MATCH (a:person {name: 'ada'})-[k:knows]->(b:person) DELETE k",
                &[],
            )
            .expect("delete the edge");

        assert_eq!(names(&mut session), ["ada", "kay", "zoe"]);
        let edges = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) RETURN count(*) AS n",
                &[],
            )
            .expect("read");
        assert_eq!(edges.rows[0][0], Value::Int(0));
        // Both ends are edge free now, so a plain DELETE takes either.
        session
            .run("MATCH (p:person {name: 'ada'}) DELETE p", &[])
            .expect("ada has nothing on her now");
        assert_eq!(names(&mut session), ["kay", "zoe"]);
    }

    /// The edges on an element go with it when the statement says
    /// DETACH, and the element goes with them.
    #[test]
    fn a_detach_delete_takes_the_edges_with_the_element() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "detach.zu1");

        session
            .run("MATCH (p:person {name: 'ada'}) DETACH DELETE p", &[])
            .expect("detach delete");

        assert_eq!(names(&mut session), ["kay", "zoe"]);
        // The edge ada knew kay by is gone too, so the end it left
        // behind has nothing on it.
        let edges = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) RETURN count(*) AS n",
                &[],
            )
            .expect("read");
        assert_eq!(edges.rows[0][0], Value::Int(0));
    }

    /// The end an edge arrived at detaches the same way the end it left
    /// does, which is what reading the backward direction is for.
    #[test]
    fn detaching_the_far_end_of_an_edge_works_the_same_way() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "detach-back.zu1");

        session
            .run("MATCH (p:person {name: 'kay'}) DETACH DELETE p", &[])
            .expect("detach delete");

        assert_eq!(names(&mut session), ["ada", "zoe"]);
        // ada is now edge free, so a plain DELETE takes her, which is
        // the check that the edge went rather than being hidden.
        session
            .run("MATCH (p:person {name: 'ada'}) DELETE p", &[])
            .expect("ada has nothing on her now");
        assert_eq!(names(&mut session), ["zoe"]);
    }

    /// The fold rebuilds the CSR without the edge, so what a detach took
    /// away is gone from the file and not only from a reader.
    #[test]
    fn a_detached_edge_stays_away_after_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("detach-reopen.zu1");
        seeded(&path);

        let mut session = Session::open(&path).expect("open");
        session
            .run("MATCH (p:person {name: 'ada'}) DETACH DELETE p", &[])
            .expect("detach delete");
        drop(session);

        let mut session = Session::open(&path).expect("reopen");
        assert_eq!(names(&mut session), ["kay", "zoe"]);
        let edges = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) RETURN count(*) AS n",
                &[],
            )
            .expect("read");
        assert_eq!(edges.rows[0][0], Value::Int(0));
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
    /// GQL leaves a deleted element bound, so a clause after the delete
    /// can name it, and what it holds is not there any more. Reading a
    /// property off one is 22G11 rather than the value the row used to
    /// hold, which is what the reader would otherwise answer with,
    /// because the row keeps its place until a vacuum moves it.
    #[test]
    fn reading_a_property_off_a_deleted_element_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "read-deleted.zu1");

        let err = session
            .run(
                "MATCH (p:person {name: 'ada'}) DETACH DELETE p RETURN p.name AS name",
                &[],
            )
            .expect_err("the element is gone");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22G11"));
        assert!(err.to_string().contains("took away"), "got: {err}");
    }

    /// The element the statement did not delete reads as it always did,
    /// so the refusal is about the reference and not about the clause
    /// order.
    #[test]
    fn reading_a_property_off_the_element_beside_it_still_answers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "read-beside.zu1");

        let out = session
            .run(
                "MATCH (p:person {name: 'ada'}), (q:person {name: 'kay'}) DETACH DELETE p RETURN q.name AS name",
                &[],
            )
            .expect("the other element is still there");
        assert_eq!(names_of(&out), ["kay"]);
    }

    /// GD03: the delete item is a query rather than a variable, and the
    /// element it answers is the one that goes.
    #[test]
    fn a_delete_item_can_be_a_query_that_names_the_element() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-value.zu1");

        session
            .run(
                "DELETE VALUE { MATCH (p:person {name: 'zoe'}) RETURN p }",
                &[],
            )
            .expect("delete");

        assert_eq!(names(&mut session), ["ada", "kay"]);
    }

    /// The subquery is a statement of its own, so it sees the store and
    /// not the variables around it. Here it names one element while the
    /// clauses around it name another, and both go.
    #[test]
    fn a_query_item_and_a_variable_item_go_together() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-value-and-var.zu1");

        session
            .run(
                "MATCH (p:person {name: 'ada'}) DETACH DELETE p, VALUE { MATCH (q:person {name: 'zoe'}) RETURN q }",
                &[],
            )
            .expect("delete");

        assert_eq!(names(&mut session), ["kay"]);
    }

    /// A value query expression is a value, so a query answering two
    /// rows has not said which element the item is about. Nothing goes.
    #[test]
    fn a_query_item_that_answers_more_than_one_element_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-value-two.zu1");

        let err = session
            .run("DELETE VALUE { MATCH (p:person) RETURN p }", &[])
            .expect_err("two elements is not one element");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22G03"));

        assert_eq!(names(&mut session), ["ada", "kay", "zoe"]);
    }

    /// The item deletes an element, so a query answering something that
    /// is not one is refused the same way `DELETE p.name` would be.
    #[test]
    fn a_query_item_that_answers_a_property_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-value-prop.zu1");

        let err = session
            .run(
                "DELETE VALUE { MATCH (p:person {name: 'zoe'}) RETURN p.name AS name }",
                &[],
            )
            .expect_err("a name is not an element");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22G03"));

        assert_eq!(names(&mut session), ["ada", "kay", "zoe"]);
    }

    /// The nested query answers the element, so it reads. One that
    /// writes is refused when the statement is compiled, before
    /// anything of either has run.
    #[test]
    fn a_query_item_that_writes_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-value-writes.zu1");

        let err = session
            .run(
                "DELETE VALUE { INSERT (p:person {name: 'new'}) RETURN p }",
                &[],
            )
            .expect_err("a delete item does not write");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("42001"));

        assert_eq!(names(&mut session), ["ada", "kay", "zoe"]);
    }

    /// A delete runs once for every row the clauses before it
    /// answered, so clauses that answered nothing delete nothing, and
    /// the query inside the item does not run either.
    #[test]
    fn a_query_item_under_a_match_that_found_nothing_deletes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-value-no-rows.zu1");

        session
            .run(
                "MATCH (p:person {name: 'nobody'}) DELETE VALUE { MATCH (q:person {name: 'zoe'}) RETURN q }",
                &[],
            )
            .expect("nothing to do is not a failure");

        assert_eq!(names(&mut session), ["ada", "kay", "zoe"]);
    }

    /// `value` is not a reserved word: the brace after it is what tells
    /// a value query expression from a variable somebody named that.
    #[test]
    fn a_variable_called_value_is_still_a_variable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "delete-value-name.zu1");

        session
            .run("MATCH (holder:person {name: 'zoe'}) DELETE holder", &[])
            .expect("delete");

        assert_eq!(names(&mut session), ["ada", "kay"]);
    }
}
