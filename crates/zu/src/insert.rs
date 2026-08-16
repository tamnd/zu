//! Running an `INSERT`: the values the statement wrote, the rows they
//! become, and the elements the clauses after it read.
//!
//! The write happens before the plan runs, which is the seam
//! [`zu_query::plan::LogicalPlan::Insert`] describes from the plan's
//! side. The reason is
//! that the executor reads through [`zu_query::exec::Graph`] and every
//! method on that trait reads, while appending a row needs the file, so
//! for now the session does the writing and the plan reads what came
//! of it. Everything in here is what has to happen in between: work out
//! what each column of each new row holds, work out which rows the
//! edges between them run between, stage all of it in one transaction,
//! and hand the created elements to the plan as arguments past the last
//! declared parameter.
//!
//! What a column takes is decided here rather than by the binder,
//! because a node table's property columns are not in the schema the
//! binder is given: the schema carries tables, labels and statistics,
//! and the columns live in the file. So this is where a value meets its
//! column, and where a value the column cannot hold is refused.

use std::collections::BTreeMap;

use zu_common::{FloatBits, LogicalType, Result, Temporal, ZuError};
use zu_query::binder::{BoundExpr, BoundInsertNode};

use crate::query::Value;
use crate::split::Write;
use crate::zu1::file::Zu1File;
use crate::zu1::props::{PropColumn, load_props};
use crate::zu1::txn::{Cell, WriteTxn};

/// One node about to be created: which table, what every column of it
/// holds, and where it will land.
pub(crate) struct NewNode {
    pub(crate) table: u32,
    /// Column id and its cell, one entry per column of the table, in
    /// column order.
    pub(crate) cols: Vec<(u32, Cell)>,
    /// The offset the row takes, which is the table's row count plus
    /// however many rows this statement is adding to it ahead of this
    /// one.
    pub(crate) offset: u64,
}

impl NewNode {
    /// The value the plan binds for this element once it exists.
    pub(crate) fn value(&self) -> Value {
        Value::Node {
            table: self.table,
            offset: self.offset,
        }
    }
}

/// One edge about to be created: which table, and the two rows it runs
/// between.
pub(crate) struct NewRel {
    pub(crate) table: u32,
    pub(crate) src: u64,
    pub(crate) dst: u64,
}

impl NewRel {
    /// The value the plan binds for this edge once it exists.
    pub(crate) fn value(&self) -> Value {
        Value::Rel {
            table: self.table,
            src: self.src,
            dst: self.dst,
        }
    }
}

/// The rows one write is making, filled a row at a time.
///
/// A write runs once for every row the clauses before it answered, so
/// this is opened once per statement and asked for every one of them.
/// It holds what is the same across the rows, which is the columns of
/// each table and where the next row of it lands, and it holds what
/// has been worked out so far, which is what gets staged at the end.
///
/// Nothing is written here: a statement that cannot be written has to
/// raise before the transaction opens, so that the failing case costs
/// no log write and no fold.
pub(crate) struct Batch<'a> {
    write: &'a Write,
    /// The property columns of each table being written to, read once.
    columns: BTreeMap<u32, Vec<PropColumn>>,
    /// Where the next row of each table lands, which is the count it
    /// holds plus the rows this statement has added to it so far.
    next: BTreeMap<u32, u64>,
    nodes: Vec<NewNode>,
    edges: Vec<NewRel>,
}

impl<'a> Batch<'a> {
    pub(crate) fn open(db: &mut Zu1File, write: &'a Write) -> Result<Self> {
        let mut columns = BTreeMap::new();
        let mut next = BTreeMap::new();
        for node in &write.nodes {
            if columns.contains_key(&node.table) {
                continue;
            }
            columns.insert(node.table, columns_of(db, node.table)?);
            next.insert(node.table, rows_in(db, node.table)?);
        }
        Ok(Self {
            write,
            columns,
            next,
            nodes: Vec::new(),
            edges: Vec::new(),
        })
    }

    /// Works out what one row of the run writes: `carried` is the row
    /// the clauses before the write answered, holding the slots in
    /// [`Write::carry`] in that order, and `props` is the property
    /// values behind them, one per property in written order.
    ///
    /// The created elements come back in the order
    /// [`Write::created`] names them, which is the order they are
    /// appended to the row the clauses after the write read.
    pub(crate) fn row(&mut self, carried: &[Value], props: &[Value]) -> Result<Vec<Value>> {
        let mut made: Vec<Value> = Vec::with_capacity(self.write.created.len());
        let mut at = 0;
        for node in &self.write.nodes {
            let values = &props[at..at + node.props.len()];
            at += node.props.len();
            let cols = row(node, &self.columns[&node.table], values)?;
            let offset = self.next.get_mut(&node.table).expect("read in open");
            let new = NewNode {
                table: node.table,
                cols,
                offset: *offset,
            };
            *offset += 1;
            made.push(new.value());
            self.nodes.push(new);
        }
        for rel in &self.write.rels {
            let ends = [(rel.src, "leaves"), (rel.dst, "arrives at")];
            let [src, dst] = ends.map(|(slot, side)| self.end(slot, side, carried, &made));
            let new = NewRel {
                table: rel.table,
                src: src?,
                dst: dst?,
            };
            made.push(new.value());
            self.edges.push(new);
        }
        Ok(made)
    }

    /// The row one end of an edge runs to: an element this write made,
    /// or one the clauses before it found.
    ///
    /// A found one is checked here rather than by the binder, because
    /// what the binder knows about it is which tables the match left
    /// open and what decides is the row.
    fn end(&self, slot: usize, side: &str, carried: &[Value], made: &[Value]) -> Result<u64> {
        let value = self
            .write
            .created
            .iter()
            .position(|s| *s == slot)
            .map(|at| &made[at])
            .or_else(|| {
                self.write
                    .carry
                    .iter()
                    .position(|s| *s == slot)
                    .map(|at| &carried[at])
            })
            .ok_or_else(|| {
                ZuError::InvalidArgument(
                    "an edge is being created between elements no clause of the statement binds"
                        .into(),
                )
            })?;
        match value {
            Value::Node { offset, .. } => Ok(*offset),
            other => Err(ZuError::gql(
                zu_common::gqlstatus::codes::C22G03,
                format!(
                    "an edge {side} an element, and this one {}",
                    match other {
                        Value::Null => "found nothing".to_string(),
                        other => format!("is {}", describe(other)),
                    }
                ),
            )),
        }
    }

    /// What the whole run writes, in the order it has to be staged in.
    pub(crate) fn staged(self) -> (Vec<NewNode>, Vec<NewRel>) {
        (self.nodes, self.edges)
    }
}

/// Stages every row of one statement in the open transaction.
///
/// The nodes go first. An edge names two rows by their offsets and the
/// rows are only there once the appends are staged, so staging them the
/// other way round would name rows that a replay of the log has not
/// made yet.
pub(crate) fn stage(txn: &mut WriteTxn<'_>, new: &[NewNode], edges: &[NewRel]) -> Result<u64> {
    let mut rows = 0;
    for node in new {
        let cols = node
            .cols
            .iter()
            .map(|(col, cell)| (*col, vec![cell.clone()]))
            .collect();
        rows += txn.insert_nodes(node.table, cols)?;
    }
    for edge in edges {
        txn.insert_rel(edge.table, edge.src, edge.dst);
        rows += 1;
    }
    Ok(rows)
}

/// The property columns of a node table, empty for a table that stores
/// none.
fn columns_of(db: &mut Zu1File, table: u32) -> Result<Vec<PropColumn>> {
    Ok(load_props(db, table)?.map_or_else(Vec::new, |dir| dir.columns))
}

/// How many rows the table holds now, which is where the next one goes.
///
/// The props directory is the count, because that is what the fold
/// checks its own arithmetic against, and a table without one is a
/// table [`row`] has already refused.
fn rows_in(db: &mut Zu1File, table: u32) -> Result<u64> {
    Ok(load_props(db, table)?.map_or(0, |dir| dir.node_count))
}

/// One row: every column of the table, in column order, filled from the
/// properties the pattern wrote.
fn row(
    node: &BoundInsertNode,
    columns: &[PropColumn],
    values: &[Value],
) -> Result<Vec<(u32, Cell)>> {
    // A row is grown by appending a cell to every column, so a table
    // with no columns has nowhere to grow: staging one would write
    // nothing and hand back an element at an offset no row occupies.
    if columns.is_empty() {
        return Err(ZuError::Unsupported {
            what: "creating an element in a table that stores no properties, which has no column for the row to grow",
            id: node.table,
        });
    }
    let mut cells = Vec::with_capacity(columns.len());
    for (ci, col) in columns.iter().enumerate() {
        // A fold rewrites a column out of its old values and the cells
        // the overlay holds, and an overlay cell is a value and never
        // an absence, so a column that may hold a null is one no
        // statement can append to yet. Saying so here names the column;
        // reaching the fold would name the table.
        if col.validity.is_some() {
            return Err(ZuError::Unsupported {
                what: "creating an element in a table whose columns may hold a null",
                id: node.table,
            });
        }
        let written = node
            .props
            .iter()
            .position(|(key, _)| *key == col.name)
            .ok_or_else(|| {
                ZuError::InvalidArgument(format!(
                    "the element carries no value for column '{}', and every column of a new row has to hold one",
                    col.name
                ))
            })?;
        cells.push((ci as u32, cell(&col.ty, &values[written], &col.name)?));
    }
    for (key, _) in &node.props {
        if !columns.iter().any(|col| col.name == *key) {
            return Err(ZuError::InvalidArgument(format!(
                "the element carries '{key}', which is not a column of the table it is created in"
            )));
        }
    }
    Ok(cells)
}

/// One value on its way into one column.
///
/// This is the inverse of the decoder in [`crate::query`], and the two
/// have to agree: the lane stores 64 bit words, so what a word out of a
/// boolean column means and what a boolean has to be written as are the
/// same fact read in two directions.
fn cell(ty: &LogicalType, value: &Value, key: &str) -> Result<Cell> {
    let wrong = || {
        ZuError::gql(
            zu_common::gqlstatus::codes::C22G03,
            format!(
                "property '{key}' holds {ty}, and {} is not one",
                describe(value)
            ),
        )
    };
    Ok(match (ty, value) {
        (LogicalType::Bool, Value::Bool(b)) => Cell::Int(u64::from(*b)),
        (LogicalType::Int { .. }, Value::Int(n)) => Cell::Int(*n as u64),
        // A whole number written into a float column is the one
        // widening allowed, the same one the appender allows, because a
        // caller who wrote 1 into a float column meant 1.0.
        (LogicalType::Float { bits, .. }, Value::Float(_) | Value::Int(_)) => {
            let f = match value {
                Value::Float(f) => *f,
                Value::Int(n) => *n as f64,
                _ => unreachable!("matched just above"),
            };
            match bits {
                FloatBits::B32 => Cell::Int(u64::from((f as f32).to_bits())),
                _ => Cell::Int(f.to_bits()),
            }
        }
        (LogicalType::Str { .. }, Value::Str(s)) => Cell::Str(s.as_bytes().to_vec()),
        (LogicalType::Date, Value::Temporal(Temporal::Date(d))) => Cell::Int(*d as i64 as u64),
        (LogicalType::LocalTime, Value::Temporal(Temporal::LocalTime(t))) => Cell::Int(*t as u64),
        (LogicalType::LocalDatetime, Value::Temporal(Temporal::LocalDatetime(t))) => {
            Cell::Int(*t as u64)
        }
        (LogicalType::Duration(want), Value::Temporal(Temporal::Duration(kind, n)))
            if want == kind =>
        {
            Cell::Int(*n as u64)
        }
        (_, Value::Null) => {
            return Err(ZuError::Unsupported {
                what: "creating an element with a property written as null",
                id: 0,
            });
        }
        _ => return Err(wrong()),
    })
}

/// What a value is, for the message when its column cannot hold it.
fn describe(value: &Value) -> &'static str {
    match value {
        Value::Null => "a null",
        Value::Bool(_) => "a boolean",
        Value::Int(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Str(_) => "a string",
        Value::Node { .. } => "a node",
        Value::Rel { .. } => "an edge",
        Value::List(_) => "a list",
        Value::Record(_) => "a record",
        Value::Temporal(_) => "a temporal value",
        Value::Path(_) => "a path",
        Value::Chain(_) => "a path",
    }
}

/// The property expressions of one insert clause, flattened in the
/// order [`plan_rows`] reads them back.
pub(crate) fn value_exprs(nodes: &[BoundInsertNode]) -> Vec<BoundExpr> {
    nodes
        .iter()
        .flat_map(|node| node.props.iter().map(|(_, expr)| expr.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::session::Session;
    use crate::zu1::graph::bulk_load_as;
    use crate::zu1::props::{PropValues, store_props};

    use super::*;

    /// Two people with an age and a name, and one edge. The names are
    /// the observable: a string comes out of the blob side of the
    /// store, which is the side an insert has to grow rather than
    /// overwrite.
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

    /// The statement the milestone is about: an element is created,
    /// the clause after it reads what was created, and the next
    /// statement finds it there.
    #[test]
    fn a_created_element_is_returned_and_then_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "insert.zu1");

        let out = session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe'}) RETURN x.name AS name, x.age AS age",
                &[],
            )
            .expect("insert");
        assert_eq!(out.columns, ["name", "age"]);
        assert_eq!(out.rows.len(), 1, "one element written, one row back");
        assert_eq!(out.rows[0][0], Value::Str("zoe".into()));
        assert_eq!(out.rows[0][1], Value::Int(30));

        let after = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "kay", "zoe"]);
    }

    /// A property is an expression like any other, and it is the same
    /// evaluator, so arithmetic and parameters work here because they
    /// work everywhere.
    #[test]
    fn a_property_value_is_an_expression() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "expr.zu1");

        let out = session
            .run(
                "INSERT (x:person {age: 20 + 10, name: $who}) RETURN x.name AS name, x.age AS age",
                &[("who", Value::Str("zoe".into()))],
            )
            .expect("insert");
        assert_eq!(out.rows[0][0], Value::Str("zoe".into()));
        assert_eq!(out.rows[0][1], Value::Int(30));
    }

    /// Two elements in one statement are two rows, and the second one
    /// lands behind the first rather than on top of it.
    #[test]
    fn two_elements_in_one_statement_land_one_after_the_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "two.zu1");

        let out = session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe'}), (y:person {age: 40, name: 'raj'}) \
                 RETURN x.name AS first, y.name AS second",
                &[],
            )
            .expect("insert");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], Value::Str("zoe".into()));
        assert_eq!(out.rows[0][1], Value::Str("raj".into()));

        let after = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "kay", "raj", "zoe"]);
    }

    /// A write statement need not project anything, and one that does
    /// not answers no rows rather than one empty row.
    #[test]
    fn a_write_with_nothing_after_it_answers_no_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "bare.zu1");

        let out = session
            .run("INSERT (x:person {age: 30, name: 'zoe'})", &[])
            .expect("insert");
        assert!(out.columns.is_empty());
        assert!(out.rows.is_empty());

        let after = session
            .run("MATCH (p:person) RETURN p.name AS name ORDER BY name", &[])
            .expect("read");
        assert_eq!(strings(&after, 0), ["ada", "kay", "zoe"]);
    }

    /// Every column of a new row has to hold something, because the
    /// fold has nowhere to put an absence, and the message names the
    /// column rather than the table.
    #[test]
    fn an_element_leaving_a_column_out_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "missing.zu1");

        let err = session
            .run("INSERT (x:person {name: 'zoe'})", &[])
            .expect_err("a column was left out");
        assert!(err.to_string().contains("'age'"), "got: {err}");

        let after = session.run("MATCH (p:person) RETURN p.name AS n", &[]);
        assert_eq!(after.expect("read").rows.len(), 2, "nothing was written");
    }

    /// A property the table has no column for is a mistake worth
    /// naming, since the alternative is a value that goes nowhere and a
    /// statement that says it worked.
    #[test]
    fn an_element_carrying_something_that_is_not_a_column_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "extra.zu1");

        let err = session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe', nickname: 'z'})",
                &[],
            )
            .expect_err("a column that is not there");
        assert!(err.to_string().contains("'nickname'"), "got: {err}");
    }

    /// A value the column cannot hold raises the type condition, and
    /// the write does not happen.
    #[test]
    fn a_value_of_the_wrong_type_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "type.zu1");

        let err = session
            .run("INSERT (x:person {age: 'thirty', name: 'zoe'})", &[])
            .expect_err("a string into an integer column");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("22G03"),
            "got: {err}"
        );
    }

    /// A label that names no table is a reference error, not a parse
    /// error: the statement is well formed and mentions something that
    /// is not there.
    #[test]
    fn a_label_that_names_no_table_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "label.zu1");

        let err = session
            .run("INSERT (x:company {name: 'acme'})", &[])
            .expect_err("no such table");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("42002"),
            "got: {err}"
        );
    }

    /// A table that stores no properties has no column to append to, so
    /// there is no row to make. Answering that is better than staging
    /// nothing and handing back an element sitting at an offset the
    /// table does not reach.
    #[test]
    fn a_table_that_stores_no_properties_has_nowhere_to_put_a_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bare-table.zu1");
        let mut db = Zu1File::create(&path).expect("create");
        bulk_load_as(&mut db, "person", "knows", 2, &[(0, 1)]).expect("load");
        drop(db);

        let mut session = Session::open(&path).expect("open");
        let err = session
            .run("INSERT (x:person)", &[])
            .expect_err("no column to grow");
        assert!(
            err.to_string().contains("stores no properties"),
            "got: {err}"
        );
    }

    /// An element with no label has nowhere to go, and picking a table
    /// for it would be inventing one.
    #[test]
    fn an_element_with_no_label_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "nolabel.zu1");

        let err = session.run("INSERT (x)", &[]).expect_err("no label");
        assert!(err.to_string().contains("label"), "got: {err}");
    }

    /// An edge written between two new elements is an edge the next
    /// hop walks, which is the whole of what writing one is for.
    #[test]
    fn an_edge_between_two_new_elements_is_there_to_walk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "edge.zu1");

        let out = session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe'})-[k:knows]->(y:person {age: 40, name: 'raj'}) \
                 RETURN x.name AS from, y.name AS to",
                &[],
            )
            .expect("insert an edge");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0][0], Value::Str("zoe".into()));
        assert_eq!(out.rows[0][1], Value::Str("raj".into()));

        let after = session
            .run(
                "MATCH (p:person {name: 'zoe'})-[:knows]->(q) RETURN q.name AS name",
                &[],
            )
            .expect("walk it");
        assert_eq!(strings(&after, 0), ["raj"]);
        // The edge the fixture came with is still the edge it came
        // with, so the write added one rather than replacing the list.
        let all = session
            .run("MATCH (p:person)-[:knows]->(q) RETURN q.name AS name", &[])
            .expect("every edge");
        assert_eq!(all.rows.len(), 2);
    }

    /// The edge the statement wrote is a value the clauses after it
    /// read, the same way a created element is.
    #[test]
    fn the_edge_a_statement_wrote_is_a_value_it_can_return() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "edge-value.zu1");

        let out = session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe'})<-[k:knows]-(y:person {age: 40, name: 'raj'}) \
                 RETURN k AS edge",
                &[],
            )
            .expect("insert an edge");
        // The arrow points at zoe, so raj is the end it leaves, and
        // both of them landed behind the two the fixture seeded.
        assert_eq!(
            out.rows[0][0],
            Value::Rel {
                table: session
                    .catalog()
                    .rel_tables()
                    .iter()
                    .find(|t| t.name == "knows")
                    .expect("knows")
                    .id,
                src: 3,
                dst: 2,
            }
        );
    }

    /// A name already standing for an element is that element again,
    /// which is how one statement hangs two edges off one node.
    #[test]
    fn two_edges_leave_one_element_the_statement_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "two-edges.zu1");

        session
            .run(
                "INSERT (a:person {age: 30, name: 'zoe'})-[:knows]->(b:person {age: 40, name: 'raj'}), \
                 (a)-[:knows]->(c:person {age: 50, name: 'ivy'})",
                &[],
            )
            .expect("two edges off one element");

        let out = session
            .run(
                "MATCH (p:person {name: 'zoe'})-[:knows]->(q) RETURN q.name AS name ORDER BY name",
                &[],
            )
            .expect("walk them");
        assert_eq!(strings(&out, 0), ["ivy", "raj"]);
    }

    /// The statement this milestone line is about: a match says which
    /// rows the write runs for, the write runs once for each of them,
    /// and the clause after it reads what each one made.
    #[test]
    fn an_insert_after_a_match_runs_once_for_every_row_it_answered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "matched.zu1");

        let out = session
            .run(
                "MATCH (a:person) INSERT (a)-[:knows]->(b:person {age: 1, name: 'new'}) RETURN a.name AS was, b.name AS made ORDER BY was",
                &[],
            )
            .expect("insert");
        assert_eq!(out.columns, ["was", "made"]);
        assert_eq!(strings(&out, 0), ["ada", "kay"], "one row per person found");
        assert_eq!(strings(&out, 1), ["new", "new"]);

        // Two people were found and two were written, and each of the
        // two edges runs from the one the row was about.
        let after = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) WHERE b.name = 'new' RETURN a.name AS name ORDER BY name",
                &[],
            )
            .expect("walk");
        assert_eq!(strings(&after, 0), ["ada", "kay"]);
        let count = session
            .run("MATCH (p:person) RETURN count(*) AS n", &[])
            .expect("count");
        assert_eq!(count.rows[0][0], Value::Int(4));
    }

    /// A write runs for the rows the clauses before it answered and
    /// for no others, so the rows the write itself makes are not rows
    /// it then runs for.
    #[test]
    fn a_write_does_not_run_for_the_rows_it_is_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "halloween.zu1");

        session
            .run(
                "MATCH (a:person) INSERT (b:person {age: a.age, name: 'copy'})",
                &[],
            )
            .expect("insert");
        let out = session
            .run("MATCH (p:person) RETURN count(*) AS n", &[])
            .expect("count");
        assert_eq!(out.rows[0][0], Value::Int(4), "two found, two written");
    }

    /// A match that answers nothing is a write that runs no times,
    /// which writes nothing and answers no rows.
    #[test]
    fn a_match_that_answers_nothing_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "nobody.zu1");

        let out = session
            .run(
                "MATCH (a:person {name: 'nobody'}) INSERT (b:person {age: 1, name: 'x'}) RETURN b.name AS name",
                &[],
            )
            .expect("insert");
        assert!(out.rows.is_empty());
        let count = session
            .run("MATCH (p:person) RETURN count(*) AS n", &[])
            .expect("count");
        assert_eq!(count.rows[0][0], Value::Int(2));
    }

    /// An edge between two elements a match found runs between the two
    /// rows of that row of the match.
    #[test]
    fn an_edge_between_two_matched_elements_is_written_per_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "pair.zu1");

        session
            .run(
                "MATCH (a:person {name: 'kay'}), (b:person {name: 'ada'}) INSERT (a)-[:knows]->(b)",
                &[],
            )
            .expect("insert");
        // The fixture has ada knows kay, and now kay knows ada too.
        let out = session
            .run(
                "MATCH (a:person)-[:knows]->(b:person) RETURN a.name AS from, b.name AS to ORDER BY from",
                &[],
            )
            .expect("walk");
        assert_eq!(strings(&out, 0), ["ada", "kay"]);
        assert_eq!(strings(&out, 1), ["kay", "ada"]);
    }

    /// Two writes in one statement run in the order they are written,
    /// and the second one sees what the first one made.
    #[test]
    fn a_statement_writes_twice_and_the_second_reads_the_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "twice.zu1");

        let out = session
            .run(
                "INSERT (a:person {age: 1, name: 'one'}) INSERT (a)-[:knows]->(b:person {age: 2, name: 'two'}) RETURN a.name AS first, b.name AS second",
                &[],
            )
            .expect("insert");
        assert_eq!(strings(&out, 0), ["one"]);
        assert_eq!(strings(&out, 1), ["two"]);
        let after = session
            .run(
                "MATCH (a:person {name: 'one'})-[:knows]->(b:person) RETURN b.name AS name",
                &[],
            )
            .expect("walk");
        assert_eq!(strings(&after, 0), ["two"]);
    }

    /// EXPLAIN prints the statement, not the parts it is run as. The
    /// parts are how the write is carried out and the plan is what the
    /// reader asked about, so the listing still holds the write.
    #[test]
    fn explaining_a_write_prints_the_whole_statement() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "explain.zu1");

        let listing = session
            .explain("MATCH (a:person) INSERT (a)-[:knows]->(b:person {age: 1, name: 'new'})")
            .expect("explain");
        assert!(listing.contains("Insert"), "the write is in it: {listing}");
        assert!(listing.contains("Scan"), "so is the match: {listing}");
    }

    /// A profile is the counters of one run of one plan, and a
    /// statement that writes is run as several, so profiling one is
    /// refused by name rather than answered with the counters of
    /// whichever part happened to be last.
    #[test]
    fn profiling_a_write_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "profile.zu1");

        let err = session
            .profile("INSERT (x:person {age: 1, name: 'new'})", &[])
            .expect_err("a write has no one plan to profile");
        assert!(
            err.to_string()
                .contains("profiling a statement that writes"),
            "says what it will not do: {err}"
        );
    }
}
