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
use crate::zu1::props::{PropColumn, load_props, load_rel_props};
use crate::zu1::txn::{Cell, WriteTxn};

/// One cell about to change: which table, which element of it, which
/// column, and what the column takes.
pub(crate) struct Update {
    pub(crate) table: u32,
    pub(crate) at: At,
    /// The column's position in the table's props directory, which is
    /// what the fold reads a staged cell back by.
    pub(crate) col: u32,
    pub(crate) cell: Cell,
}

/// Which element of a table a change lands on.
///
/// A row is named by its offset and an edge by the rows it runs between,
/// which is the only name an edge has: its values sit in the order its
/// table holds its edges, and a fold rebuilds that order.
#[derive(Clone, Copy)]
pub(crate) enum At {
    Row(u64),
    Edge(u64, u64),
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
            let (table, element) = self.element(item, carried)?;
            let columns = match self.columns.entry(table) {
                Entry::Occupied(held) => held.into_mut(),
                Entry::Vacant(empty) => empty.insert(columns_of(db, table, &element)?),
            };
            match &item.key {
                Some(key) => {
                    let col = column_of(columns, key)?;
                    self.updates.push(Update {
                        table,
                        at: element,
                        col: col as u32,
                        cell: written(&columns[col], &values[at], key)?,
                    });
                }
                None => {
                    // Every column of the table is written, whether the
                    // record named it or not, because what this form says
                    // is what the element holds afterwards and not what
                    // to change about it. A column the record leaves out
                    // therefore takes a null, which is the same value
                    // REMOVE writes.
                    let given = named(&values[at], columns)?;
                    for (ci, col) in columns.iter().enumerate() {
                        let cell = match given.get(&ci) {
                            Some(value) => written(col, value, &col.name)?,
                            None => Cell::Null,
                        };
                        self.updates.push(Update {
                            table,
                            at: element,
                            col: ci as u32,
                            cell,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// The row one assignment changes, out of the row the clauses
    /// before the write answered.
    ///
    /// A `SET` changes what it found, so the element is always one the
    /// row carries, and a row that found nothing there has nothing to
    /// change.
    fn element(&self, item: &BoundSetItem, carried: &[Value]) -> Result<(u32, At)> {
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
            Value::Node { table, offset } => Ok((*table, At::Row(*offset))),
            Value::Rel { table, src, dst } => Ok((*table, At::Edge(*src, *dst))),
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
        match update.at {
            At::Row(offset) => {
                txn.update(update.table, offset, update.col, update.cell.clone());
            }
            At::Edge(src, dst) => {
                txn.update_rel(update.table, src, dst, update.col, update.cell.clone());
            }
        }
    }
    Ok(updates.len() as u64)
}

/// Which column of a table a key names.
fn column_of(columns: &[PropColumn], key: &str) -> Result<usize> {
    columns
        .iter()
        .position(|col| col.name == key)
        .ok_or_else(|| {
            ZuError::InvalidArgument(format!(
                "'{key}' is not a column of the table the element it is being set on is in"
            ))
        })
}

/// The columns a whole record assignment gave a value for, by column
/// position.
///
/// A field naming something the table does not keep is refused here
/// rather than passed over, for the reason `SET p.nope = 1` is: a table
/// holds the columns it holds, and a caller who wrote a key it does not
/// have has either mistyped it or is thinking of another table.
fn named<'a>(value: &'a Value, columns: &[PropColumn]) -> Result<BTreeMap<usize, &'a Value>> {
    let Value::Record(fields) = value else {
        // Unreachable through the parser, whose grammar for this form is
        // a record written out, so there is nothing else it can be.
        return Err(ZuError::InvalidArgument(format!(
            "SET of a whole record takes a record, and this one is {}",
            describe(value)
        )));
    };
    let mut given = BTreeMap::new();
    for (name, field) in fields {
        given.insert(column_of(columns, name)?, field);
    }
    Ok(given)
}

/// The cell one assignment puts in one column.
///
/// A null is the one value that does not have to fit the column, since
/// what it says is that the row holds nothing rather than that it holds
/// something of another type. That is what `REMOVE` writes, and it is
/// what `SET p.age = null` writes too, because GQL defines the first as
/// the second.
fn written(col: &PropColumn, value: &Value, key: &str) -> Result<Cell> {
    match value {
        Value::Null => Ok(Cell::Null),
        other => cell(&col.ty, other, key),
    }
}

/// The property columns of a table an element being changed sits in.
///
/// A rel table keeps its columns inside its group directory rather than
/// in the table index, because they are in edge order and the directory
/// is what holds that order, so which of the two is read comes from what
/// the element is.
fn columns_of(db: &mut Zu1File, table: u32, at: &At) -> Result<Vec<PropColumn>> {
    let dir = match at {
        At::Row(_) => load_props(db, table)?,
        At::Edge(..) => load_rel_props(db, table)?,
    };
    let columns = dir.map_or_else(Vec::new, |dir| dir.columns);
    if columns.is_empty() {
        return Err(ZuError::Unsupported {
            what: "setting a property on an element of a table that stores none",
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
    use crate::zu1::props::{PropValues, store_props, store_rel_props};

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

    /// The same two people with a year stored on the edge between them.
    /// A rel table keeps its columns over the edges in load order, so a
    /// change to one has to be put back into that order rather than at
    /// an offset of its own.
    fn seeded_with_edge_props(path: &Path) {
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
        store_rel_props(&mut db, "knows", &[("since", PropValues::Int(&[1990]))])
            .expect("edge props");
    }

    fn open_with_edge_props(dir: &tempfile::TempDir, name: &str) -> Session {
        let path = dir.path().join(name);
        seeded_with_edge_props(&path);
        Session::open(&path).expect("open")
    }

    /// Three people in a chain and one more edge back to the start,
    /// each edge carrying a year and a note. Two columns of two kinds
    /// over three edges is what says a change landed on the edge it
    /// named: the two beside it are there to still hold what they held.
    fn seeded_with_three_edges(path: &Path) {
        let mut db = Zu1File::create(path).expect("create");
        bulk_load_as(&mut db, "person", "knows", 3, &[(0, 1), (1, 2), (2, 0)]).expect("load");
        let names: Vec<&[u8]> = vec![b"ada", b"kay", b"joe"];
        store_props(
            &mut db,
            "person",
            &[
                ("age", PropValues::Int(&[10, 20, 30])),
                ("name", PropValues::Str(&names)),
            ],
        )
        .expect("props");
        let notes: Vec<&[u8]> = vec![b"one", b"two", b"three"];
        store_rel_props(
            &mut db,
            "knows",
            &[
                ("since", PropValues::Int(&[1990, 1991, 1992])),
                ("note", PropValues::Str(&notes)),
            ],
        )
        .expect("edge props");
    }

    fn open_with_three_edges(dir: &tempfile::TempDir, name: &str) -> Session {
        let path = dir.path().join(name);
        seeded_with_three_edges(&path);
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

    /// The other element a property can be set on. An edge keeps its
    /// values in the order its table holds its edges, so the change is
    /// named by the rows the edge runs between and the fold puts it
    /// back into that order.
    #[test]
    fn a_property_on_an_edge_is_set_and_then_read_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_edge_props(&dir, "set-edge.zu1");

        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) SET k.since = 1991",
                &[],
            )
            .expect("set on an edge");
        assert!(out.rows.is_empty(), "a write with nothing after it");

        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN k.since AS since",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Int(1991));
    }

    /// One edge of a table changes and the others keep what they held,
    /// which is what says the change landed on a pair rather than on a
    /// place in the column.
    #[test]
    fn setting_one_edge_leaves_the_other_edges_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_three_edges(&dir, "set-edge-one.zu1");

        session
            .run(
                "MATCH (a:person {name: 'kay'})-[k:knows]->(b:person) SET k.since = 2000",
                &[],
            )
            .expect("set the middle edge");

        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN a.name AS name, k.since AS since ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "joe", "kay"]);
        assert_eq!(after.rows[0][1], Value::Int(1990), "ada is where she was");
        assert_eq!(after.rows[1][1], Value::Int(1992), "joe is where he was");
        assert_eq!(after.rows[2][1], Value::Int(2000));
    }

    /// A string column on an edge is the blob side of the store, which
    /// a change rewrites rather than overwrites in place, and the
    /// rewrite is over the whole table's edges.
    #[test]
    fn setting_a_string_property_on_an_edge_rewrites_the_blob_side() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_three_edges(&dir, "set-edge-string.zu1");

        session
            .run(
                "MATCH (a:person {name: 'ada'})-[k:knows]->(b:person) SET k.note = $to",
                &[("to", Value::Str("met at work".into()))],
            )
            .expect("set a note");

        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN a.name AS name, k.note AS note ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(strings(&after, 1), ["met at work", "three", "two"]);
    }

    /// An edge property is in the file after the write, not just in the
    /// overlay the statement left behind.
    #[test]
    fn an_edge_property_stays_set_after_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("set-edge-reopen.zu1");
        seeded_with_edge_props(&path);
        {
            let mut session = Session::open(&path).expect("open");
            session
                .run(
                    "MATCH (a:person)-[k:knows]->(b:person) SET k.since = 1991",
                    &[],
                )
                .expect("set");
        }

        let mut session = Session::open(&path).expect("reopen");
        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN k.since AS since",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Int(1991));
    }

    /// A table that stores nothing on its edges has no column for a
    /// value to land in, and there is no column to make: what columns a
    /// table has is a matter for the catalog and not for a statement
    /// that writes one value.
    #[test]
    fn setting_a_property_on_an_edge_of_a_bare_table_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-edge-bare.zu1");

        let err = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) SET k.since = 1990",
                &[],
            )
            .expect_err("the table stores nothing on its edges");
        assert!(err.to_string().contains("stores none"), "got: {err}");
    }

    /// The other half of the milestone line: a property comes off an
    /// element, and what is there afterwards is a null rather than an
    /// old value or an error.
    #[test]
    fn a_property_is_removed_and_then_reads_as_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "remove.zu1");

        let out = session
            .run("MATCH (p:person {name: 'ada'}) REMOVE p.age", &[])
            .expect("remove");
        assert!(out.rows.is_empty(), "a write with nothing after it");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Null, "ada holds no age");
        assert_eq!(after.rows[1][1], Value::Int(20), "kay is where she was");
    }

    /// A column the statement did not name keeps what it held, which is
    /// the difference between taking a property off an element and
    /// taking the element apart.
    #[test]
    fn removing_one_property_leaves_the_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "remove-one.zu1");

        session
            .run("MATCH (p:person {name: 'ada'}) REMOVE p.age", &[])
            .expect("remove");

        let after = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "kay"]);
    }

    /// GQL defines REMOVE as writing null, so the assignment spelling
    /// of it does the same thing. If these two ever disagree, one of
    /// them is wrong.
    #[test]
    fn setting_a_property_to_null_removes_it_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "remove-by-set.zu1");

        session
            .run("MATCH (p:person {name: 'ada'}) SET p.age = null", &[])
            .expect("set null");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Null);
    }

    /// The blob side stores an absence as no bytes at all, and the row
    /// still has to read back as a null rather than as an empty string.
    #[test]
    fn removing_a_string_property_reads_back_as_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "remove-string.zu1");

        session
            .run("MATCH (p:person {name: 'ada'}) REMOVE p.name", &[])
            .expect("remove");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.age AS age, p.name AS name ORDER BY age",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Null, "the name is gone");
        assert_eq!(after.rows[1][1], Value::Str("kay".into()));
    }

    /// A row that holds nothing can be written to again, which is what
    /// says the mask is state rather than a decision taken once.
    #[test]
    fn a_removed_property_can_be_set_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "remove-then-set.zu1");

        session
            .run("MATCH (p:person {name: 'ada'}) REMOVE p.age", &[])
            .expect("remove");
        session
            .run("MATCH (p:person {name: 'ada'}) SET p.age = 41", &[])
            .expect("set");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Int(41));
    }

    /// Every row of the column can hold nothing, which is the case a
    /// mask of one word has to get right at both ends.
    #[test]
    fn removing_a_property_from_every_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "remove-all.zu1");

        session
            .run("MATCH (p:person) REMOVE p.age", &[])
            .expect("remove");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Null);
        assert_eq!(after.rows[1][1], Value::Null);
    }

    /// A statement that removes one property and writes another runs as
    /// two writes over one row, and the log carries a run of each kind
    /// rather than one record that cannot say which it is.
    #[test]
    fn a_remove_and_a_set_in_one_statement_both_land() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "remove-and-set.zu1");

        session
            .run(
                "MATCH (p:person {name: 'ada'}) SET p.age = 44 REMOVE p.name",
                &[],
            )
            .expect("set then remove");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.age AS age, p.name AS name ORDER BY age",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[1][0], Value::Int(44));
        assert_eq!(after.rows[1][1], Value::Null);
    }

    /// The whole record form says what an element holds afterwards, so a
    /// property the record leaves out is gone rather than left alone.
    /// This is the conformance case for the form, spelling and all.
    #[test]
    fn a_whole_record_replaces_every_property_of_a_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-record.zu1");

        session
            .run(
                "MATCH (p:person {name: 'ada'}) SET p = {name: 'ada lovelace'}",
                &[],
            )
            .expect("set a record");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Str("ada lovelace".into()));
        assert_eq!(after.rows[0][1], Value::Null, "the age was not named");
        // The other person was not in the match, so nothing about them
        // moved.
        assert_eq!(after.rows[1][0], Value::Str("kay".into()));
        assert_eq!(after.rows[1][1], Value::Int(20));
    }

    /// A record that names every column writes every column, which is
    /// the case where the form empties nothing.
    #[test]
    fn a_whole_record_that_names_every_column_writes_them_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-record-full.zu1");

        session
            .run(
                "MATCH (p:person {name: 'ada'}) SET p = {age: 41, name: 'ada again'}",
                &[],
            )
            .expect("set a record");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY age",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[1][0], Value::Str("ada again".into()));
        assert_eq!(after.rows[1][1], Value::Int(41));
    }

    /// The empty record is the way to say that an element holds nothing
    /// at all, and it is the same write as removing every property.
    #[test]
    fn an_empty_record_empties_every_property() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-record-empty.zu1");

        session
            .run("MATCH (p:person {name: 'ada'}) SET p = {}", &[])
            .expect("set an empty record");

        let after = session
            .run("MATCH (p:person) RETURN p.name AS name, p.age AS age", &[])
            .expect("read");
        let emptied = after
            .rows
            .iter()
            .filter(|row| row[0] == Value::Null && row[1] == Value::Null)
            .count();
        assert_eq!(emptied, 1, "one person holds nothing and one still does");
    }

    /// A field of the record is an expression like any other, and it is
    /// worked out before the write, so it can read the element the record
    /// is about to replace.
    #[test]
    fn a_field_of_the_record_can_read_what_it_replaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-record-read.zu1");

        session
            .run(
                "MATCH (p:person {name: 'ada'}) SET p = {age: p.age + 1, name: p.name}",
                &[],
            )
            .expect("set a record");

        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY age",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Str("ada".into()));
        assert_eq!(after.rows[0][1], Value::Int(11));
    }

    /// A key the table does not keep is refused, the way it is when one
    /// assignment names it: what columns a table has is a matter for the
    /// catalog, and a caller who wrote one it has not has mistyped it.
    #[test]
    fn a_record_naming_a_key_the_table_does_not_have_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "set-record-bad-key.zu1");

        let err = session
            .run("MATCH (p:person) SET p = {nope: 1}", &[])
            .expect_err("no such column");
        assert!(err.to_string().contains("'nope'"), "got: {err}");
    }

    /// A record replaces an edge's properties the way it replaces a
    /// node's. Where the values go is different and settled elsewhere;
    /// what this says is that the form reaches both kinds of element.
    #[test]
    fn a_whole_record_replaces_every_property_of_an_edge() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_three_edges(&dir, "set-record-edge.zu1");

        session
            .run(
                "MATCH (a:person {name: 'ada'})-[k:knows]->(b:person) SET k = {since: 2000}",
                &[],
            )
            .expect("set a record on an edge");

        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN a.name AS who, k.since AS since, k.note AS note ORDER BY who",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Str("ada".into()));
        assert_eq!(after.rows[0][1], Value::Int(2000));
        assert_eq!(after.rows[0][2], Value::Null, "the note was not named");
        // The two edges the statement did not name kept both of theirs.
        assert_eq!(after.rows[1][0], Value::Str("joe".into()));
        assert_eq!(after.rows[1][1], Value::Int(1992));
        assert_eq!(after.rows[1][2], Value::Str("three".into()));
        assert_eq!(after.rows[2][1], Value::Int(1991));
        assert_eq!(after.rows[2][2], Value::Str("two".into()));
    }

    /// The replacement is in the file and not only in the overlay, so it
    /// is still there after a close and an open.
    #[test]
    fn a_replaced_record_stays_replaced_after_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("set-record-reopen.zu1");
        seeded(&path);
        {
            let mut session = Session::open(&path).expect("open");
            session
                .run(
                    "MATCH (p:person {name: 'ada'}) SET p = {name: 'ada lovelace'}",
                    &[],
                )
                .expect("set a record");
        }

        let mut session = Session::open(&path).expect("reopen");
        let after = session
            .run(
                "MATCH (p:person) RETURN p.name AS name, p.age AS age ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Str("ada lovelace".into()));
        assert_eq!(after.rows[0][1], Value::Null);
    }

    /// A label is the other thing GQL lets REMOVE take, and an element
    /// carries the labels of the table it is in, so it is turned away
    /// by name rather than by a parse error about a colon.
    #[test]
    fn removing_a_label_says_which_part_is_not_in_yet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "remove-label.zu1");

        let err = session
            .run("MATCH (p:person) REMOVE p:person", &[])
            .expect_err("not in yet");
        assert!(err.to_string().contains("REMOVE of a label"), "got: {err}");
    }

    /// A property comes off an edge the way it comes off a node, and
    /// what is there afterwards is a null. An edge column holds an
    /// absence the way a row column does, as a clear bit beside a
    /// placeholder nothing reads.
    #[test]
    fn a_property_is_removed_from_an_edge_and_then_reads_as_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_three_edges(&dir, "remove-edge.zu1");

        session
            .run(
                "MATCH (a:person {name: 'kay'})-[k:knows]->(b:person) REMOVE k.since",
                &[],
            )
            .expect("remove");

        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN a.name AS name, k.since AS since ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Int(1990), "ada is where she was");
        assert_eq!(after.rows[1][1], Value::Int(1992), "joe is where he was");
        assert_eq!(after.rows[2][1], Value::Null, "kay's year is gone");
    }

    /// A string property on an edge comes off too, and the blob side
    /// says the absence with no bytes rather than with an empty string.
    #[test]
    fn removing_a_string_property_from_an_edge_reads_back_as_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_three_edges(&dir, "remove-edge-string.zu1");

        session
            .run(
                "MATCH (a:person {name: 'ada'})-[k:knows]->(b:person) REMOVE k.note",
                &[],
            )
            .expect("remove");

        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN a.name AS name, k.note AS note ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Null);
        assert_eq!(after.rows[1][1], Value::Str("three".into()));
        assert_eq!(after.rows[2][1], Value::Str("two".into()));
    }

    /// An edge whose property was taken off can take one again, which
    /// is what says the mask is state and not a decision taken once.
    #[test]
    fn a_removed_edge_property_can_be_set_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_three_edges(&dir, "remove-edge-then-set.zu1");

        session
            .run(
                "MATCH (a:person {name: 'kay'})-[k:knows]->(b:person) REMOVE k.since",
                &[],
            )
            .expect("remove");
        session
            .run(
                "MATCH (a:person {name: 'kay'})-[k:knows]->(b:person) SET k.since = 2001",
                &[],
            )
            .expect("set");

        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN a.name AS name, k.since AS since ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[2][1], Value::Int(2001));
    }

    /// An edge that holds nothing in a column keeps holding nothing
    /// through a fold that has other work to do, which is the case a
    /// mask being rebuilt into a new edge order has to get right.
    #[test]
    fn an_edge_absence_survives_a_later_write_to_the_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_three_edges(&dir, "remove-edge-then-write.zu1");

        session
            .run(
                "MATCH (a:person {name: 'kay'})-[k:knows]->(b:person) REMOVE k.since",
                &[],
            )
            .expect("remove");
        session
            .run(
                "MATCH (a:person {name: 'ada'})-[k:knows]->(b:person) SET k.since = 1900",
                &[],
            )
            .expect("set another edge");

        let after = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN a.name AS name, k.since AS since ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(after.rows[0][1], Value::Int(1900));
        assert_eq!(after.rows[2][1], Value::Null, "kay's year is still gone");
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
