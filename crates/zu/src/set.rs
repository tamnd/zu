//! Running a `SET`: the elements the statement named, the columns its
//! keys land in, and the cells that go there.
//!
//! This is the other half of what [`crate::split`] does to a statement
//! that writes, and it sits in the same seam [`crate::insert`] does:
//! the executor reads through a graph and a graph reads, so the session
//! runs the write between two plans. What is different is what a write
//! carries across itself. An `INSERT` hands the clauses after it
//! elements that were not there before; a `SET` hands them the row it
//! was given, because it changed what a name already stood for rather
//! than binding a new one.
//!
//! Which column a key names is settled here rather than by the binder,
//! for the reason the inserted columns are: a table's property columns
//! live in the file and the schema the binder is given carries tables,
//! labels and statistics.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use zu_common::{Result, ZuError};
use zu_query::binder::BoundSetItem;

use crate::insert::{cell, describe};
use crate::query::Value;
use crate::split::Set;
use crate::zu1::file::Zu1File;
use crate::zu1::props::{PropColumn, load_props};
use crate::zu1::txn::{Cell, WriteTxn};

/// One cell about to change: which table, which row of it, which
/// column, and what the column takes.
pub(crate) struct Update {
    pub(crate) table: u32,
    pub(crate) offset: u64,
    /// The column's position in the table's props directory, which is
    /// what the fold reads a staged cell back by.
    pub(crate) col: u32,
    pub(crate) cell: Cell,
}

/// The changes one write is making, filled a row at a time.
///
/// A write runs once for every row the clauses before it answered, so
/// this is opened once per statement and asked for every one of them.
/// It holds the columns of each table it has had to read, and it holds
/// what has been worked out so far, which is what gets staged at the
/// end.
///
/// Nothing is written here: a statement that cannot be written has to
/// raise before the transaction opens, so that the failing case costs
/// no log write and no fold.
pub(crate) struct Changes<'a> {
    write: &'a Set,
    /// The property columns of each table an element has turned up in,
    /// read once per table. Which tables those are is not known until
    /// the rows arrive, because a slot the write names is bound by a
    /// match and a match can leave several tables open.
    columns: BTreeMap<u32, Vec<PropColumn>>,
    updates: Vec<Update>,
}

impl<'a> Changes<'a> {
    pub(crate) fn open(write: &'a Set) -> Self {
        Self {
            write,
            columns: BTreeMap::new(),
            updates: Vec::new(),
        }
    }

    /// Works out what one row of the run changes: `carried` is the row
    /// the clauses before the write answered, holding the slots in
    /// [`Set::carry`] in that order, and `values` is what the
    /// assignments take, one per item in written order.
    pub(crate) fn row(
        &mut self,
        db: &mut Zu1File,
        carried: &[Value],
        values: &[Value],
    ) -> Result<()> {
        for (at, item) in self.write.items.iter().enumerate() {
            let (table, offset) = self.element(item, carried)?;
            let columns = match self.columns.entry(table) {
                Entry::Occupied(held) => held.into_mut(),
                Entry::Vacant(empty) => empty.insert(columns_of(db, table)?),
            };
            let col = columns
                .iter()
                .position(|col| col.name == item.key)
                .ok_or_else(|| {
                    ZuError::InvalidArgument(format!(
                        "'{}' is not a column of the table the element it is being set on is in",
                        item.key
                    ))
                })?;
            self.updates.push(Update {
                table,
                offset,
                col: col as u32,
                cell: cell(&columns[col].ty, &values[at], &item.key)?,
            });
        }
        Ok(())
    }

    /// The row one assignment changes, out of the row the clauses
    /// before the write answered.
    ///
    /// A `SET` changes what it found, so the element is always one the
    /// row carries, and a row that found nothing there has nothing to
    /// change.
    fn element(&self, item: &BoundSetItem, carried: &[Value]) -> Result<(u32, u64)> {
        let value = self
            .write
            .carry
            .iter()
            .position(|slot| *slot == item.target)
            .map(|at| &carried[at])
            .ok_or_else(|| {
                ZuError::InvalidArgument(
                    "a property is being set on an element no clause of the statement binds".into(),
                )
            })?;
        match value {
            Value::Node { table, offset } => Ok((*table, *offset)),
            other => Err(ZuError::gql(
                zu_common::gqlstatus::codes::C22G03,
                format!(
                    "SET changes an element, and this one {}",
                    match other {
                        Value::Null => "found nothing".to_string(),
                        other => format!("is {}", describe(other)),
                    }
                ),
            )),
        }
    }

    /// What the whole run changes, in written order.
    pub(crate) fn staged(self) -> Vec<Update> {
        self.updates
    }
}

/// Stages every change of one statement in the open transaction.
///
/// Two assignments to one cell are staged in the order they were
/// written, which is the order the fold reads them back in, so the last
/// one written is the one the column ends up holding.
pub(crate) fn stage(txn: &mut WriteTxn<'_>, updates: &[Update]) -> Result<u64> {
    for update in updates {
        txn.update(update.table, update.offset, update.col, update.cell.clone());
    }
    Ok(updates.len() as u64)
}

/// The property columns of a table an element being changed sits in.
///
/// A table whose columns may hold a null is refused here rather than in
/// the fold, because a fold rewrites a column out of its old values and
/// the cells the overlay holds and an overlay cell is never an absence.
/// Saying so here costs no log write; reaching the fold would leave a
/// committed record nothing can apply.
fn columns_of(db: &mut Zu1File, table: u32) -> Result<Vec<PropColumn>> {
    let columns = load_props(db, table)?.map_or_else(Vec::new, |dir| dir.columns);
    if columns.is_empty() {
        return Err(ZuError::Unsupported {
            what: "setting a property on an element of a table that stores none",
            id: table,
        });
    }
    if columns.iter().any(|col| col.validity.is_some()) {
        return Err(ZuError::Unsupported {
            what: "setting a property in a table whose columns may hold a null",
            id: table,
        });
    }
    Ok(columns)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::session::Session;
    use crate::zu1::graph::bulk_load_as;
    use crate::zu1::props::{PropValues, store_props};

    use super::*;

    /// Two people with an age and a name, the same fixture the insert
    /// tests use, because what a write has to get right about a string
    /// column is the same on both sides.
    fn seeded(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        bulk_load_as(&mut db, "person", "knows", 2, &[(0, 1)]).expect("load");
        let names: Vec<&[u8]> = vec![b"ada", b"kay"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20])),
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

    fn strings(result: &crate::query::QueryResult, col: usize) -> Vec<String> {
        result
            .rows
            .iter()
            .map(|row| match &row[col] {
                Value::Str(s) => s.clone(),
                other => panic!("expected a string, got {other:?}"),
            })
            .collect()
    }

    /// The statement the milestone line is about: a match says which
    /// element changes, the change happens, and the next statement
    /// finds the new value there.
    #[test]
    fn a_property_is_set_and_then_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set.zu1");

        let out = session
            .run("MATCH (p:person {name: 'ada'}) SET p.age = 37", &[])
            .expect("set");
        assert!(out.rows.is_empty(), "a write with nothing after it");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Int(37));
        assert_eq!(after.rows[1][1], Value::Int(20), "kay is where she was");
    }

    /// The clause after the write reads the row the write ran for, and
    /// the element it names holds what was just put there.
    #[test]
    fn the_clause_after_a_set_reads_the_new_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-return.zu1");

        let out = session
            .run(
                "MATCH (p:person {name: 'ada'}) SET p.age = 37 RETURN p.name AS name, p.age AS age",
                &[],
            )
            .expect("set");
        assert_eq!(out.columns, ["name", "age"]);
        assert_eq!(strings(&out, 0), ["ada"]);
        assert_eq!(out.rows[0][1], Value::Int(37));
    }

    /// A write runs once for every row the clauses before it answered,
    /// and the value is an expression evaluated per row.
    #[test]
    fn a_set_runs_once_for_every_row_the_match_answered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-rows.zu1");

        session
            .run("MATCH (p:person) SET p.age = p.age + 1", &[])
            .expect("set");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Int(11));
        assert_eq!(after.rows[1][1], Value::Int(21));
    }

    /// A string column is the blob side of the store, which a change
    /// has to rewrite rather than overwrite in place.
    #[test]
    fn setting_a_string_rewrites_the_blob_side() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-string.zu1");

        session
            .run(
                "MATCH (p:person {name: 'ada'}) SET p.name = $to",
                &[("to", Value::Str("adelaide".into()))],
            )
            .expect("set");

        let after = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["adelaide", "kay"]);
    }

    /// One statement sets two properties, and both of them are there
    /// afterwards.
    #[test]
    fn two_assignments_in_one_statement_both_land() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-two.zu1");

        session
            .run(
                "MATCH (p:person {name: 'ada'}) SET p.age = 37, p.name = 'zoe'",
                &[],
            )
            .expect("set");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(strings(&after, 0), ["kay", "zoe"]);
        assert_eq!(after.rows[1][1], Value::Int(37));
    }

    /// A statement that inserts and then changes what it inserted runs
    /// as three parts, and the change lands on the row the insert made.
    #[test]
    fn a_set_after_an_insert_changes_what_the_insert_made() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "insert-then-set.zu1");

        session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe'}) SET x.age = 31",
                &[],
            )
            .expect("insert then set");

        let after = session
            .run("MATCH (p:person {name: 'zoe'}) RETURN p.age AS age", &[])
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Int(31));
    }

    /// A key naming no column of the table is a mistake worth naming,
    /// since the alternative is a value that goes nowhere and a
    /// statement that says it worked.
    #[test]
    fn setting_a_key_that_names_no_column_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-nocolumn.zu1");

        let err = session
            .run("MATCH (p:person) SET p.nickname = 'z'", &[])
            .expect_err("no such column");
        assert!(err.to_string().contains("'nickname'"), "got: {err}");

        let after = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "kay"], "nothing changed");
    }

    /// A value the column cannot hold raises the type condition, and
    /// the change does not happen.
    #[test]
    fn setting_a_value_of_the_wrong_type_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-type.zu1");

        let err = session
            .run("MATCH (p:person) SET p.age = 'thirty'", &[])
            .expect_err("a string into an integer column");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("22G03"),
            "got: {err}"
        );
    }

    /// A name no clause bound stands for nothing to change, and the
    /// binder says so rather than the run.
    #[test]
    fn setting_on_a_name_nothing_bound_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-unbound.zu1");

        let err = session
            .run("MATCH (p:person) SET q.age = 1", &[])
            .expect_err("q stands for nothing");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("42002"),
            "got: {err}"
        );
    }

    /// An edge stores its properties in the order its table holds its
    /// edges, and nothing writes into that order yet, so this is
    /// refused by name rather than by type.
    #[test]
    fn setting_a_property_on_an_edge_says_which_part_is_not_in_yet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-edge.zu1");

        let err = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) SET k.since = 1990",
                &[],
            )
            .expect_err("not in yet");
        assert!(err.to_string().contains("SET on an edge"), "got: {err}");
    }

    /// EXPLAIN prints the statement, not the parts it is run as, so the
    /// change is in the listing the way the match is.
    #[test]
    fn explaining_a_set_prints_the_assignment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-explain.zu1");

        let listing = session
            .explain("MATCH (p:person) SET p.age = 37")
            .expect("explain");
        assert!(listing.contains("Set p.age = 37"), "got: {listing}");
    }
}
