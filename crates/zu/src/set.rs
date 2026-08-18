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

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};
use zu_query::binder::{BoundSetInto, BoundSetItem};

use crate::insert::{cell, describe};
use crate::query::Value;
use crate::split::Set;
use crate::zu1::catalog::{Catalog, MAX_LABELS};
use crate::zu1::file::Zu1File;
use crate::zu1::props::{PropColumn, load_props, load_rel_props};
use crate::zu1::txn::{Cell, WriteTxn};

/// One change about to be staged.
///
/// A property and a label are both what an element carries, and both are
/// what a `SET` writes, but they are not written the same way: a property
/// is a cell of a column, and a label is a bit of the row's label word.
/// So the two are one enum rather than one struct with a spare field.
pub(crate) enum Update {
    /// One cell: which table, which element of it, which column, and
    /// what the column takes.
    Cell {
        table: u32,
        at: At,
        /// The column's position in the table's props directory, which
        /// is what the fold reads a staged cell back by.
        col: u32,
        cell: Cell,
    },
    /// The labels one row takes on and stops carrying, as bits of the
    /// graph's label dictionary. Only a row has labels: an edge carries
    /// the one name its rel table is.
    Labels {
        table: u32,
        row: u64,
        add: u64,
        remove: u64,
    },
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
/// The property columns of the tables writes have touched, held by the
/// session rather than by the statement.
///
/// Which tables those are is not known until the rows arrive, because a
/// slot the write names is bound by a match and a match can leave
/// several tables open, so this fills in as the rows come. Reading a
/// directory back is a block chain walk and a copy of every block in
/// it, and a point write that did that per statement spent a quarter of
/// its processor time on blocks it had copied before.
///
/// What a directory says changes only when the layout of the file does,
/// and that moves the header epoch, so the session drops this along
/// with the plans and the readers an epoch move invalidates.
#[derive(Default)]
pub(crate) struct Dirs(BTreeMap<u32, Vec<PropColumn>>);

impl Dirs {
    /// The columns of `table`, read from the file the first time and
    /// held after that.
    ///
    /// A rel table keeps its columns inside its group directory rather
    /// than in the table index, because they are in edge order and the
    /// directory is what holds that order, so which of the two is read
    /// comes from what the element is.
    fn columns(&mut self, db: &mut Zu1File, table: u32, at: &At) -> Result<&[PropColumn]> {
        if let Entry::Vacant(empty) = self.0.entry(table) {
            let dir = match at {
                At::Row(_) => load_props(db, table)?,
                At::Edge(..) => load_rel_props(db, table)?,
            };
            empty.insert(dir.map_or_else(Vec::new, |dir| dir.columns));
        }
        let columns = &self.0[&table];
        if columns.is_empty() {
            return Err(ZuError::Unsupported {
                what: "setting a property on an element of a table that stores none",
                id: table,
            });
        }
        Ok(columns)
    }
}

pub(crate) struct Changes<'a> {
    write: &'a Set,
    /// The catalog as the statement started, which is what says which
    /// bit a label is and which labels a table has declared. A `SET` of
    /// a label a table has not declared declares it here, so this is
    /// also what the statement publishes when it is done.
    catalog: Catalog,
    /// Whether any of that happened, which is what says the catalog
    /// above has to be stored before the changes are staged.
    widened: bool,
    updates: Vec<Update>,
}

impl<'a> Changes<'a> {
    pub(crate) fn open(write: &'a Set, catalog: Catalog) -> Self {
        Self {
            write,
            catalog,
            widened: false,
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
        dirs: &mut Dirs,
        carried: &[Value],
        values: &[Value],
    ) -> Result<()> {
        for (at, item) in self.write.items.iter().enumerate() {
            let (table, element) = self.element(item, carried)?;
            // A label is not a column, so an item that writes one needs
            // nothing read out of the table's props and is settled
            // before they are.
            if let BoundSetInto::Labels { labels, on } = &item.into {
                let mask = self.mask(table, &element, labels, *on)?;
                let (add, remove) = match on {
                    true => (mask, 0),
                    false => (0, mask),
                };
                let At::Row(row) = element else {
                    unreachable!("an edge was turned away where the mask was worked out");
                };
                self.updates.push(Update::Labels {
                    table,
                    row,
                    add,
                    remove,
                });
                continue;
            }
            let columns = dirs.columns(db, table, &element)?;
            match &item.into {
                BoundSetInto::Labels { .. } => unreachable!("settled above"),
                BoundSetInto::Property(key) => {
                    let col = column_of(columns, key)?;
                    self.updates.push(Update::Cell {
                        table,
                        at: element,
                        col: col as u32,
                        cell: written(&columns[col], &values[at], key)?,
                    });
                }
                BoundSetInto::Record => {
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
                        self.updates.push(Update::Cell {
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
            Value::Rel {
                table, src, dst, ..
            } => Ok((*table, At::Edge(*src, *dst))),
            other => Err(ZuError::gql(
                codes::C22G03,
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

    /// The bits a set of written labels is, against the table the
    /// element sits in.
    ///
    /// A label the table has not declared is a catalog change the
    /// statement makes: `SET p:Manager` says a row of the table carries
    /// it, and a catalog that did not say the table may hold it would
    /// then disagree with the file. So the declaration goes onto this
    /// statement's own copy of the catalog, which is published before
    /// the changes are staged and undone with them if the statement
    /// raises. `REMOVE` declares nothing, because a label the table has
    /// never declared is one no row of it carries and there is nothing
    /// to take off; widening the catalog to record an absence would say
    /// the table may hold a label because somebody said it does not.
    ///
    /// An edge is turned away, because an edge carries the one name its
    /// rel table is and there is no word beside it to hold another. zu
    /// therefore declares one as both the most labels an edge may carry
    /// and the fewest, and the two codes for that are the two things a
    /// statement can ask of an edge's labels: putting one on exceeds
    /// the maximum and taking one off falls below the minimum.
    fn mask(&mut self, table: u32, at: &At, labels: &[String], on: bool) -> Result<u64> {
        if matches!(at, At::Edge(..)) {
            let (status, why) = match on {
                true => (
                    codes::C22G0R,
                    "carries one label, the name of its rel table, which is the most zu holds on an edge",
                ),
                false => (
                    codes::C22G0Q,
                    "carries one label, the name of its rel table, which is the fewest zu holds on an edge",
                ),
            };
            return Err(ZuError::gql(
                status,
                format!("the labels of an edge are not a statement's to change: an edge {why}"),
            ));
        }
        let node = self.catalog.node_by_id(table).ok_or_else(|| {
            ZuError::InvalidArgument(format!(
                "the element being changed is in unknown table {table}"
            ))
        })?;
        let primary = node.primary_label();
        let mut declared = node.label_mask();
        let mut mask = 0u64;
        for label in labels {
            let known = self
                .catalog
                .label_id(label)
                .filter(|id| declared & 1 << id != 0);
            let id = match (known, on) {
                (Some(id), _) => id,
                (None, false) => continue,
                (None, true) => {
                    self.room_for(label)?;
                    let id = self.catalog.declare_label(table, label)?;
                    declared |= 1 << id;
                    self.widened = true;
                    id
                }
            };
            if id == primary {
                // Taking the table's own name off a row would leave the
                // row with no label at all, and a node carries at least
                // the one, so that is the minimum said out loud rather
                // than a type error. Putting it on is a different thing
                // to be told: the row has it already and always did.
                return Err(match on {
                    true => ZuError::gql(
                        codes::C22G03,
                        format!(
                            "'{label}' is the name of the table the element is in, so every row of it carries that label"
                        ),
                    ),
                    false => ZuError::gql(
                        codes::C22G0N,
                        format!(
                            "'{label}' is the name of the table the element is in, and a node carries at least the one label, so there is no taking it off"
                        ),
                    ),
                });
            }
            mask |= 1 << id;
        }
        Ok(mask)
    }

    /// Whether the graph's label dictionary has room for a name it has
    /// not seen.
    ///
    /// A node's labels are the bits of one word, so the dictionary is
    /// 64 names wide and a node carries at most 64 labels: the two
    /// limits are the same limit, and a statement that needs a
    /// sixty-fifth name is a statement asking a node to carry more
    /// labels than zu holds. The catalog would refuse this itself, but
    /// it refuses it as a storage limit, and what a reader is owed here
    /// is the standard's code for the limit they hit.
    fn room_for(&self, label: &str) -> Result<()> {
        if self.catalog.labels().len() < MAX_LABELS {
            return Ok(());
        }
        Err(ZuError::gql(
            codes::C22G0P,
            format!(
                "'{label}' would be label {} of this graph, and a node's labels are the bits of one word, so zu holds {MAX_LABELS}",
                MAX_LABELS + 1
            ),
        ))
    }

    /// What the whole run changes, in written order, and the catalog it
    /// widened along the way, which is `None` when it widened none.
    ///
    /// The catalog is published before the changes are staged, because
    /// the fold turns a label change away when the table it lands on has
    /// not declared the bit, and the fold reads the catalog out of the
    /// file rather than out of the statement.
    pub(crate) fn staged(self) -> (Vec<Update>, Option<Catalog>) {
        let widened = self.widened.then_some(self.catalog);
        (self.updates, widened)
    }
}

/// Stages every change of one statement in the open transaction.
///
/// Two assignments to one cell are staged in the order they were
/// written, which is the order the fold reads them back in, so the last
/// one written is the one the column ends up holding.
pub(crate) fn stage(txn: &mut WriteTxn<'_>, updates: &[Update]) -> Result<u64> {
    for update in updates {
        match update {
            Update::Cell {
                table,
                at: At::Row(offset),
                col,
                cell,
            } => txn.update(*table, *offset, *col, cell.clone()),
            Update::Cell {
                table,
                at: At::Edge(src, dst),
                col,
                cell,
            } => txn.update_rel(*table, *src, *dst, *col, cell.clone()),
            Update::Labels {
                table,
                row,
                add,
                remove,
            } => txn.update_labels(*table, *row, *add, *remove)?,
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

    /// The same two people with one of them a Bot, so the graph's
    /// label dictionary holds names beyond the table's own and the
    /// table has declared them. A declared label is what a change can
    /// put on a row, and Admin is declared without being on anyone,
    /// which is the state a statement that puts it on someone starts
    /// from.
    fn seeded_with_labels(path: &Path) {
        seeded(path);
        let mut db = Zu1File::open(path).expect("open");
        crate::zu1::props::store_labels(&mut db, "person", &[vec!["Bot"], vec!["Admin"]])
            .expect("labels");
        // Admin goes on and comes straight back off, so the table has
        // declared it and no row carries it.
        crate::zu1::props::store_labels(&mut db, "person", &[vec!["Bot"], vec![]]).expect("labels");
    }

    fn open_with_labels(dir: &tempfile::TempDir, name: &str) -> Session {
        let path = dir.path().join(name);
        seeded_with_labels(&path);
        Session::open(&path).expect("open")
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

    /// A label the table declares goes onto the row the statement
    /// found, and a pattern naming that label then finds it. The rows
    /// the statement did not name keep the labels they had.
    #[test]
    fn a_label_is_put_on_a_row_and_a_pattern_then_finds_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "set-label.zu1");

        session
            .run("MATCH (p:person) WHERE p.name = 'kay' SET p:Bot", &[])
            .expect("set");
        let after = session
            .run("MATCH (p:Bot) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(after.rows.len(), 2, "ada was one already");
        assert_eq!(after.rows[0][0], Value::Str("ada".into()));
        assert_eq!(after.rows[1][0], Value::Str("kay".into()));
        // The properties are what they were: a label is a bit beside
        // the row rather than anything in a column.
        let ages = session
            .run("MATCH (p:person) RETURN p.age AS age ORDER BY age", &[])
            .expect("read");
        assert_eq!(ages.rows[0][0], Value::Int(10));
        assert_eq!(ages.rows[1][0], Value::Int(20));
    }

    /// A label comes off the row REMOVE named, and the row stays where
    /// it was: what changed is one bit of one word.
    #[test]
    fn a_label_taken_off_a_row_stops_a_pattern_finding_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "remove-label.zu1");

        session
            .run("MATCH (p:Bot) REMOVE p:Bot", &[])
            .expect("remove");
        let after = session
            .run("MATCH (p:Bot) RETURN p.name", &[])
            .expect("read");
        assert!(after.rows.is_empty());
        let all = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(all.rows.len(), 2, "both rows are still there");
    }

    /// Two changes to one row in one statement both land, because the
    /// two masks compose rather than the second replacing the first.
    #[test]
    fn a_label_going_on_and_another_coming_off_both_land() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "both-labels.zu1");

        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Admin", &[])
            .expect("set");
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' REMOVE p:Bot", &[])
            .expect("remove");
        let admins = session
            .run("MATCH (p:Admin) RETURN p.name AS name", &[])
            .expect("read");
        assert_eq!(admins.rows[0][0], Value::Str("ada".into()));
        let bots = session
            .run("MATCH (p:Bot) RETURN p.name", &[])
            .expect("read");
        assert!(
            bots.rows.is_empty(),
            "the one that was a Bot is not one now"
        );
    }

    /// A label change is in the file rather than in the session, so a
    /// reopen reads it back.
    #[test]
    fn a_label_change_stays_after_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reopen-label.zu1");
        seeded_with_labels(&path);
        {
            let mut session = Session::open(&path).expect("open");
            session
                .run("MATCH (p:person) WHERE p.name = 'kay' SET p:Admin", &[])
                .expect("set");
        }
        let mut session = Session::open(&path).expect("reopen");
        let after = session
            .run("MATCH (p:Admin) RETURN p.name AS name", &[])
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Str("kay".into()));
    }

    /// A label the table has not declared is one the statement declares:
    /// the catalog says the table may hold it, the row's word carries
    /// the bit, and a pattern naming it finds the row afterwards. This
    /// is the catalog change a data statement stages.
    #[test]
    fn a_label_the_table_has_not_declared_is_declared_by_the_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "undeclared.zu1");

        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Manager", &[])
            .expect("set a label nobody declared");

        let after = session
            .run("MATCH (p:Manager) RETURN p.name AS name", &[])
            .expect("read");
        assert_eq!(after.rows.len(), 1);
        assert_eq!(after.rows[0][0], Value::Str("ada".into()));
        // The other row of the table is where it was: the table now
        // declares the label, which is not the same as carrying it.
        let all = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&all, 0), ["ada", "kay"]);
    }

    /// The declaration is in the catalog rather than in the session, so
    /// a reopen reads the label back, both as what the table may hold
    /// and as what the row carries.
    #[test]
    fn a_declared_label_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("declare-reopen.zu1");
        seeded_with_labels(&path);
        {
            let mut session = Session::open(&path).expect("open");
            session
                .run("MATCH (p:person) WHERE p.name = 'kay' SET p:Manager", &[])
                .expect("set");
        }

        let mut session = Session::open(&path).expect("reopen");
        assert!(
            session.catalog().label_id("Manager").is_some(),
            "the dictionary kept the name"
        );
        let after = session
            .run("MATCH (p:Manager) RETURN p.name AS name", &[])
            .expect("read");
        assert_eq!(after.rows[0][0], Value::Str("kay".into()));
    }

    /// The declaration a statement makes inside a transaction goes back
    /// with the transaction, and it goes back after a crash the same
    /// way it goes back after a `ROLLBACK`: the widened catalog is
    /// published by the same roots the savepoint keeps, so putting the
    /// roots back takes the label out of the dictionary along with the
    /// bit off the row.
    #[test]
    fn a_label_declared_inside_a_transaction_the_process_died_inside_is_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("declare-crash.zu1");
        seeded_with_labels(&path);
        let mut session = Session::open(&path).expect("open");

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Manager", &[])
            .expect("declare and set");
        session
            .run("MATCH (p:person) WHERE p.name = 'kay' SET p.age = 99", &[])
            .expect("a second statement over the first");
        // The process stops here, with the transaction open and both
        // statements published.
        std::mem::forget(session);

        let mut reopened = Session::open(&path).expect("reopen");
        assert!(
            reopened.catalog().label_id("Manager").is_none(),
            "the declaration went back with the transaction"
        );
        let ages = reopened
            .run("MATCH (p:person) RETURN p.age AS age ORDER BY age", &[])
            .expect("read");
        assert_eq!(ages.rows.len(), 2);
        assert_ne!(ages.rows[1][0], Value::Int(99), "and so did the write");
    }

    /// A table that had no labels beyond its own name stores no bitset
    /// at all, so the first label put on one of its rows is both a
    /// catalog change and the segment coming into being.
    #[test]
    fn the_first_label_on_a_table_that_had_none_lands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "declare-first.zu1");

        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Admin", &[])
            .expect("set the first label");

        let after = session
            .run("MATCH (p:Admin) RETURN p.name AS name", &[])
            .expect("read");
        assert_eq!(after.rows.len(), 1);
        assert_eq!(after.rows[0][0], Value::Str("ada".into()));
    }

    /// A statement that declares a label and then raises leaves neither
    /// behind, because the declaration is published under the same
    /// savepoint the changes are.
    #[test]
    fn a_declaration_is_undone_with_the_statement_that_made_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "declare-undone.zu1");

        let err = session
            .run(
                "MATCH (p:person) WHERE p.name = 'ada' SET p:Manager WITH p RETURN p.age / 0 AS bad",
                &[],
            )
            .expect_err("the clause after the write raises");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("22012"),
            "the write ran and the clause after it raised, got: {err}"
        );

        assert!(
            session.catalog().label_id("Manager").is_none(),
            "the dictionary is where it was"
        );
        let after = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "kay"]);
    }

    /// The declaration a statement makes is published where the rows it
    /// changed are, so a transaction rolled back takes both: the label
    /// is gone from the dictionary and no row carries the bit.
    #[test]
    fn a_rollback_takes_the_declaration_with_the_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "declare-rollback.zu1");

        session.run("START TRANSACTION", &[]).expect("start");
        session
            .run("MATCH (p:person) WHERE p.name = 'ada' SET p:Manager", &[])
            .expect("set");
        let inside = session
            .run("MATCH (p:Manager) RETURN p.name AS name", &[])
            .expect("read inside the transaction");
        assert_eq!(inside.rows.len(), 1, "the statement before this one ran");
        session.run("ROLLBACK", &[]).expect("rollback");

        assert!(
            session.catalog().label_id("Manager").is_none(),
            "the dictionary is where it was"
        );
        let after = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "kay"]);
    }

    /// A label nobody declared is a label no row carries, so taking it
    /// off one is a statement that changes nothing rather than one that
    /// widens the catalog to record an absence.
    #[test]
    fn removing_a_label_nobody_declared_declares_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "remove-undeclared.zu1");

        session
            .run("MATCH (p:person) REMOVE p:Manager", &[])
            .expect("nothing to take off");

        assert!(
            session.catalog().label_id("Manager").is_none(),
            "the dictionary is where it was"
        );
    }

    /// The name of the table is the label every row of it carries, so
    /// it is not a label a statement puts on a row or takes off one.
    #[test]
    fn the_name_of_the_table_is_not_a_label_to_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "primary-label.zu1");

        // The two directions are two different things to be told. The
        // row has the label already, which is a type error about what
        // was written; taking it off would leave the row with no label
        // at all, which is the minimum a node carries.
        for (source, code) in [
            ("MATCH (p:person) SET p:person", "22G03"),
            ("MATCH (p:person) REMOVE p:person", "22G0N"),
        ] {
            let err = session.run(source, &[]).expect_err("the table's own name");
            assert!(
                err.to_string().contains("name of the table"),
                "{source}: {err}"
            );
            assert_eq!(err.gqlstatus().map(|s| s.code()), Some(code), "{source}");
        }
    }

    /// A node's labels are the bits of one word, so the graph holds 64
    /// names and a statement asking for a sixty-fifth is asking a node
    /// to carry more labels than there are bits. The limit is declared
    /// rather than unlimited on purpose, so this is the code the
    /// standard has for hitting a declared one.
    #[test]
    fn a_label_past_the_width_of_the_word_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "label-width.zu1");

        // The fixture arrives with a few names in the dictionary
        // already, table names among them, so the run below asks for
        // new ones until it is told there is no room rather than
        // counting on how many were there to start with.
        let mut refused = None;
        for n in 0..MAX_LABELS {
            match session.run(&format!("MATCH (p:person) SET p:L{n}"), &[]) {
                Ok(_) => continue,
                Err(err) => {
                    refused = Some(err);
                    break;
                }
            }
        }
        let err = refused.expect("the dictionary fills before the run is out of names");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22G0P"));
        assert!(err.to_string().contains("one word"), "got: {err}");
    }

    /// An edge carries the one name its rel table is, so one is both
    /// the most labels zu holds on an edge and the fewest, and the two
    /// ways of asking get the two codes for those two limits.
    #[test]
    fn the_labels_of_an_edge_are_a_limit_in_both_directions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "edge-label-limits.zu1");

        for (source, code) in [
            ("MATCH (p:person)-[k:knows]->(q:person) SET k:Bot", "22G0R"),
            (
                "MATCH (p:person)-[k:knows]->(q:person) REMOVE k:knows",
                "22G0Q",
            ),
        ] {
            let err = session.run(source, &[]).expect_err("an edge has one label");
            assert_eq!(err.gqlstatus().map(|s| s.code()), Some(code), "{source}");
        }
    }

    /// A graph with a closed type over the same two people, with two
    /// node types: one for a person and one for a person who is also a
    /// Manager. Two types keyed differently over one table is what a
    /// label change moves an element between.
    fn open_typed(dir: &tempfile::TempDir, name: &str) -> Session {
        let path = dir.path().join(name);
        // The fixture with no labels on it, because the graph type
        // below describes what a row of the copy may carry and a row
        // that arrives carrying something else is a graph that was
        // already outside its type before the statement under test.
        seeded(&path);
        let mut session = Session::open(&path).expect("open");
        for source in [
            "CREATE PROPERTY GRAPH TYPE t {
               (:person {name :: STRING, age :: INT}),
               (:person&Manager {name :: STRING, age :: INT})
             }",
            "CREATE GRAPH g TYPED t AS COPY OF home",
        ] {
            session.run(source, &[]).expect("the graph and its type");
        }
        session
    }

    /// The box this pair is about: a label change moves an element from
    /// the element type it belonged to into another one, and a closed
    /// graph type is what says which moves there are. This one lands,
    /// because the type describes a person who is a Manager.
    #[test]
    fn a_label_change_moves_an_element_between_element_types() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_typed(&dir, "moved.zu1");

        session
            .run("USE g MATCH (p:person {name: 'ada'}) SET p:Manager", &[])
            .expect("the type describes a person who is a Manager");

        let after = session
            .run(
                "USE g MATCH (p:Manager) RETURN p.name AS name ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada"]);
    }

    /// The same change with a label the type says nothing about takes
    /// the element out of every element type the graph has, which is
    /// the promise a closed type makes, so it is refused with G2000 and
    /// the message names the element and what it would have carried.
    #[test]
    fn a_label_change_that_belongs_to_no_element_type_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_typed(&dir, "unheld.zu1");

        let err = session
            .run("USE g MATCH (p:person {name: 'ada'}) SET p:Owner", &[])
            .expect_err("no element type describes a person who is an Owner");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("G2000"));
        let text = err.to_string();
        assert!(text.contains("row 0 of 'person'"), "got: {text}");
        assert!(text.contains("person, Owner"), "got: {text}");

        // The refusal is the statement's, so nothing it did survives:
        // the declaration the SET would have made is gone with it.
        let after = session
            .run(
                "USE g MATCH (p:person) RETURN p.name AS name ORDER BY name",
                &[],
            )
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "kay"]);
    }

    /// The graph the same file's home is stays open, so the change the
    /// typed graph refuses is one the untyped graph beside it takes.
    #[test]
    fn a_graph_with_no_type_takes_the_change_the_typed_one_refuses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_typed(&dir, "beside.zu1");

        session
            .run("MATCH (p:person {name: 'ada'}) SET p:Owner", &[])
            .expect("the home graph promises nothing");
    }

    /// An edge carries the one name its rel table is, and there is no
    /// word beside it to hold another, so a label change on one is
    /// turned away by name.
    #[test]
    fn an_edge_has_no_labels_to_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_labels(&dir, "edge-label.zu1");

        let err = session
            .run("MATCH (p:person)-[k:knows]->(q:person) SET k:Bot", &[])
            .expect_err("an edge has no labels");
        assert!(err.to_string().contains("labels of an edge"), "got: {err}");
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
    /// An element holds one value per property, so a clause that
    /// assigns to the same one twice has not said which of the two it
    /// wants. Two separate `SET` clauses stay last wins, which is what
    /// running one clause after another means, and this is about the
    /// one clause.
    #[test]
    fn assigning_to_one_property_twice_in_one_clause_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "twice.zu1");

        let err = session
            .run("MATCH (p:person) SET p.age = 1, p.age = 2", &[])
            .expect_err("two values for one property");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("22G0M"),
            "got: {err}"
        );
        assert!(err.to_string().contains("'age'"), "got: {err}");
    }

    /// The whole record and one property of it are the same assignment
    /// twice for the same reason, whichever order they come in.
    #[test]
    fn assigning_a_record_and_a_property_of_it_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "record-and-property.zu1");

        let err = session
            .run("MATCH (p:person) SET p = {age: 1}, p.age = 2", &[])
            .expect_err("the record and a property of it");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("22G0M"),
            "got: {err}"
        );
    }

    /// Two clauses, not one, so the second wins and nothing is
    /// refused.
    #[test]
    fn two_clauses_assigning_to_one_property_stay_last_wins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "two-clauses.zu1");

        session
            .run(
                "MATCH (p:person {name: 'ada'}) SET p.age = 1 SET p.age = 2",
                &[],
            )
            .expect("two clauses in a row");
        let out = session
            .run("MATCH (p:person {name: 'ada'}) RETURN p.age AS age", &[])
            .expect("read it back");
        assert_eq!(out.rows[0][0], crate::query::Value::Int(2));
    }
}
