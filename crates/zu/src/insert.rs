//! Running an `INSERT`: the values the statement wrote, the rows they
//! become, and the elements the clauses after it read.
//!
//! The write sits between two plans rather than inside one, which is
//! what [`crate::split`] does to a statement that writes. The reason is
//! that the executor reads through [`zu_query::exec::Graph`] and every
//! method on that trait reads, while appending a row needs the file, so
//! the session does the writing and the plan after it reads what came
//! of it. Everything in here is what has to happen in between: work out
//! what each column of each new row holds, work out which rows the
//! edges between them run between and what those carry, stage all of it
//! in one transaction, and hand the created elements to the next plan
//! as arguments past the last declared parameter.
//!
//! What a column takes is decided here rather than by the binder,
//! because a node table's property columns are not in the schema the
//! binder is given: the schema carries tables, labels and statistics,
//! and the columns live in the file. So this is where a value meets its
//! column, and where a value the column cannot hold is refused.

use std::collections::{BTreeMap, BTreeSet};

use zu_common::gqlstatus::codes;
use zu_common::{FloatBits, GqlStatus, LogicalType, Result, Temporal, ZuError};
use zu_query::binder::{BoundExpr, BoundInsertNode, BoundInsertRel};

use crate::deleted::Deleted;
use crate::query::Value;
use crate::split::Insert;
use crate::zu1::catalog::{Catalog, MAX_PROPERTIES};
use crate::zu1::file::Zu1File;
use crate::zu1::props::{PropColumn, load_props, load_rel_props};
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

/// One edge about to be created: which table, the two rows it runs
/// between, and what it carries.
pub(crate) struct NewRel {
    pub(crate) table: u32,
    pub(crate) src: u64,
    pub(crate) dst: u64,
    /// Column position in the table's props directory and its cell, one
    /// entry per column of the table, in column order. Empty for a
    /// table that stores nothing on its edges.
    pub(crate) cols: Vec<(u32, Cell)>,
}

impl NewRel {
    /// The value the plan binds for this edge once it exists.
    pub(crate) fn value(&self) -> Value {
        Value::Rel {
            table: self.table,
            src: self.src,
            dst: self.dst,
            // A staged edge has no row in the stored columns until the
            // fold lands, so a property read off it answers null, which
            // is what it answered before an edge carried a row at all.
            ord: Value::NO_REL_ROW,
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
    write: &'a Insert,
    /// The catalog as the statement started, which is what says which
    /// graph a table is in and which two node tables a rel table runs
    /// between. Owned for the reason [`crate::delete::Removals`] owns
    /// one: the file it came out of is handed in a row at a time.
    catalog: Catalog,
    /// The rows the file has lost, which is what says an endpoint an
    /// earlier clause found is one a `DELETE` has since taken away. A
    /// statement's own delete is in here because a write folds as it is
    /// staged, so the part after it reads a file the tombstone is in.
    gone: Deleted,
    /// The property columns of each node table being written to, read
    /// once.
    columns: BTreeMap<u32, Vec<PropColumn>>,
    /// The property columns of each rel table being written to, read
    /// once. A rel table stores nothing on its edges until something is
    /// stored there, and then this is empty for it.
    rel_columns: BTreeMap<u32, Vec<PropColumn>>,
    /// Where the next row of each table lands, which is the count it
    /// holds plus the rows this statement has added to it so far.
    next: BTreeMap<u32, u64>,
    nodes: Vec<NewNode>,
    edges: Vec<NewRel>,
}

impl<'a> Batch<'a> {
    pub(crate) fn open(db: &mut Zu1File, write: &'a Insert, catalog: Catalog) -> Result<Self> {
        let mut columns = BTreeMap::new();
        let mut next = BTreeMap::new();
        for node in &write.nodes {
            room_for(&node.props, "element", codes::C22G0S)?;
            if columns.contains_key(&node.table) {
                continue;
            }
            columns.insert(node.table, columns_of(db, node.table)?);
            next.insert(node.table, rows_in(db, node.table)?);
        }
        let mut rel_columns = BTreeMap::new();
        for rel in &write.rels {
            room_for(&rel.props, "edge", codes::C22G0T)?;
            if rel_columns.contains_key(&rel.table) {
                continue;
            }
            rel_columns.insert(rel.table, rel_columns_of(db, rel.table)?);
        }
        // Read once per statement rather than per row, and not at all
        // by a statement that writes no edges, which is where the
        // endpoint questions are asked.
        let gone = match write.rels.is_empty() {
            true => Deleted::default(),
            false => Deleted::load(db)?,
        };
        Ok(Self {
            write,
            catalog,
            gone,
            columns,
            rel_columns,
            next,
            nodes: Vec::new(),
            edges: Vec::new(),
        })
    }

    /// Works out what one row of the run writes: `carried` is the row
    /// the clauses before the write answered, holding the slots in
    /// [`Insert::carry`] in that order, and `props` is the property
    /// values behind them, one per property in written order.
    ///
    /// The created elements come back in the order
    /// [`Insert::created`] names them, which is the order they are
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
            let values = &props[at..at + rel.props.len()];
            at += rel.props.len();
            let cols = edge_row(rel, &self.rel_columns[&rel.table], values)?;
            let table = self.catalog.rel_by_id(rel.table).ok_or_else(|| {
                ZuError::InvalidArgument(format!(
                    "the edge is going in unknown table {}",
                    rel.table
                ))
            })?;
            let ends = [
                (rel.src, "leaves", table.from),
                (rel.dst, "arrives at", table.to),
            ];
            let [src, dst] =
                ends.map(|(slot, side, want)| self.end(slot, side, want, carried, &made));
            let new = NewRel {
                table: rel.table,
                src: src?,
                dst: dst?,
                cols,
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
    /// open and what decides is the row. `want` is the node table the
    /// rel table declares at this end, and there are three ways a row
    /// can fail to be one of its rows: it is in a table of another
    /// graph, it is in the wrong table of this one, or it is in the
    /// right table and a `DELETE` has taken it away.
    fn end(
        &self,
        slot: usize,
        side: &str,
        want: u32,
        carried: &[Value],
        made: &[Value],
    ) -> Result<u64> {
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
            Value::Node { table, offset } if *table == want => {
                if self.gone.holds(*table, *offset) {
                    return Err(ZuError::gql(
                        codes::CG1002,
                        format!(
                            "an edge {side} row {offset} of '{}', which a DELETE has taken away",
                            name_of(&self.catalog, *table)
                        ),
                    ));
                }
                Ok(*offset)
            }
            Value::Node { table, offset } => {
                Err(wrong_end(&self.catalog, *table, *offset, want, side))
            }
            other => Err(ZuError::gql(
                codes::C22G03,
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

    /// The rel tables this run writes into that store something on
    /// their edges, which are the ones a second edge over a pair
    /// cannot go into.
    pub(crate) fn propful(&self) -> BTreeSet<u32> {
        self.rel_columns
            .iter()
            .filter(|(_, cols)| !cols.is_empty())
            .map(|(&table, _)| table)
            .collect()
    }

    /// The rows this run is creating, as table and offset. They are not
    /// in the store yet, so nothing may be asked of the store about
    /// them.
    pub(crate) fn created_rows(&self) -> BTreeSet<(u32, u64)> {
        self.nodes.iter().map(|n| (n.table, n.offset)).collect()
    }

    /// What the whole run writes, in the order it has to be staged in.
    pub(crate) fn staged(self) -> (Vec<NewNode>, Vec<NewRel>) {
        (self.nodes, self.edges)
    }
}

/// Refuses a second edge over a pair that a rel table storing edge
/// properties already holds, before the write reaches the log.
///
/// The store holds a pair that runs twice perfectly well: a bulk load
/// takes every copy the file gave it, the reader answers a pair with
/// the whole run, and the fold keeps the copies in the order they were
/// written. What a statement cannot do yet is say afterwards which copy
/// it means. A DELETE names an edge by the rows it runs between and so
/// does a property write, so both would reach every copy of the pair
/// rather than the one the statement matched, and an edge property
/// nobody can then delete or change on its own is worse than one that
/// was never written. So the second edge is refused here, where the
/// statement can be told why, and the refusal goes when those two learn
/// to carry the ordinal the match already knows.
///
/// `propful` names the tables this applies to. A rel table that stores
/// nothing on its edges has nothing to address by the pair, so a pair
/// may run through it as many times as the statements say.
///
/// `created` names the rows the same statement is making. They have no
/// edges by definition, and asking the store about a row it does not
/// hold yet is an error rather than an answer, so an edge onto one is
/// checked against this statement's own edges and nothing else.
pub(crate) fn refuse_duplicate_pairs(
    graph: &mut impl zu_query::exec::Graph,
    catalog: &Catalog,
    edges: &[NewRel],
    propful: &BTreeSet<u32>,
    created: &BTreeSet<(u32, u64)>,
) -> Result<()> {
    let mut written: BTreeSet<(u32, u64, u64)> = BTreeSet::new();
    for edge in edges {
        if !propful.contains(&edge.table) {
            continue;
        }
        // Twice in one statement is as much a duplicate as once now
        // and once before, and the store cannot be asked about the
        // first of the two because it is not in it yet.
        let fresh = written.insert((edge.table, edge.src, edge.dst));
        let ends = catalog.rel_by_id(edge.table);
        let brand_new = ends.is_some_and(|rel| {
            created.contains(&(rel.from, edge.src)) || created.contains(&(rel.to, edge.dst))
        });
        if fresh
            && (brand_new
                || graph
                    .edge_ordinal(edge.table, edge.src, edge.dst)?
                    .is_none())
        {
            continue;
        }
        return Err(ZuError::InvalidArgument(format!(
            "'{}' stores properties on its edges, and rows {} and {} already run through it, \
             so a second edge between them has nowhere to put its values",
            catalog
                .rel_by_id(edge.table)
                .map_or("?", |rel| rel.name.as_str()),
            edge.src,
            edge.dst
        )));
    }
    Ok(())
}

/// Fills in the stored row of every edge the statement has just
/// written, now that staging has folded them into their tables.
///
/// An edge is handed to the clauses after the write before it is
/// staged, because what settles the rows it runs between is the write
/// itself, so at that moment it has no row of its own and a property
/// read off it would answer null. By the time this runs the fold has
/// landed, so the row is there to be found and `RETURN k.since` reads
/// back what the same statement wrote on `k`.
///
/// A pair of rows that runs more than once names as many edges as it
/// has copies, and the one this statement wrote is the last of them:
/// the fold keeps the copies in the order they were written and puts
/// the ones it is adding behind the ones already there.
pub(crate) fn settle(graph: &mut impl zu_query::exec::Graph, rows: &mut [Value]) -> Result<()> {
    for row in rows {
        let Value::List(values) = row else {
            continue;
        };
        for value in values {
            if let Value::Rel {
                table,
                src,
                dst,
                ord,
            } = value
                && *ord == Value::NO_REL_ROW
                && let Some((base, count)) = graph.edge_run(*table, *src, *dst)?
            {
                *ord = base + count - 1;
            }
        }
    }
    Ok(())
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
        txn.insert_rel_carrying(edge.table, edge.src, edge.dst, edge.cols.clone());
        rows += 1;
    }
    Ok(rows)
}

/// The condition an endpoint in the wrong table is.
///
/// Which one it is turns on the graph. A row of another graph is not an
/// element of the graph being written at all, and the standard has a
/// code saying exactly that. A row of this graph in a table the rel
/// table does not run between is an element of the right graph and the
/// wrong type, which is the other code. The binder turns away every
/// case it can see, but a slot a match left open over several tables is
/// one only the row settles.
fn wrong_end(catalog: &Catalog, table: u32, offset: u64, want: u32, side: &str) -> ZuError {
    let graph_of = |id: u32| catalog.node_by_id(id).map(|t| t.graph);
    if graph_of(table) != graph_of(want) {
        return ZuError::gql(
            codes::CG1003,
            format!(
                "an edge {side} row {offset} of '{}', which is not in the graph the edge is being written in",
                name_of(catalog, table)
            ),
        );
    }
    ZuError::gql(
        codes::C22G0W,
        format!(
            "an edge {side} an element of '{}', and row {offset} is in '{}'",
            name_of(catalog, want),
            name_of(catalog, table)
        ),
    )
}

/// A node table's name, for a message that has to say which table a row
/// is in.
fn name_of(catalog: &Catalog, table: u32) -> &str {
    catalog.node_by_id(table).map_or("?", |t| t.name.as_str())
}

/// Refuses a pattern writing more properties than an element holds.
///
/// zu declares a finite limit rather than calling itself unlimited,
/// because a declared limit is a limit an encoding can be built on, and
/// this is the one the plan writes down (plan/07 §2.4). The count is
/// the pattern's own, so it is settled once per statement and before
/// any column is read: a pattern this wide is refused whatever the
/// table holds.
fn room_for(props: &[(String, BoundExpr)], noun: &str, status: GqlStatus) -> Result<()> {
    if props.len() <= MAX_PROPERTIES {
        return Ok(());
    }
    Err(ZuError::gql(
        status,
        format!(
            "the {noun} carries {} properties, and zu holds {MAX_PROPERTIES} on one",
            props.len()
        ),
    ))
}

/// The property columns of a node table, empty for a table that stores
/// none.
fn columns_of(db: &mut Zu1File, table: u32) -> Result<Vec<PropColumn>> {
    Ok(load_props(db, table)?.map_or_else(Vec::new, |dir| dir.columns))
}

/// The property columns of a rel table, empty for a table that stores
/// nothing on its edges, which is most of them.
fn rel_columns_of(db: &mut Zu1File, rel: u32) -> Result<Vec<PropColumn>> {
    Ok(load_rel_props(db, rel)?.map_or_else(Vec::new, |dir| dir.columns))
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
    fill(node.table, "element", &node.props, columns, values)
}

/// One edge: every column its table stores, in column order, filled
/// from the properties the pattern wrote.
///
/// A rel table that stores nothing on its edges is the ordinary case
/// rather than a refusal, because an edge is a pair of rows in the
/// neighbour lists and needs no column to exist. Writing a value onto
/// one of those edges is what there is nowhere to put, and the fold
/// says the same thing about a log record that reaches it.
fn edge_row(
    rel: &BoundInsertRel,
    columns: &[PropColumn],
    values: &[Value],
) -> Result<Vec<(u32, Cell)>> {
    if columns.is_empty() {
        if rel.props.is_empty() {
            return Ok(Vec::new());
        }
        return Err(ZuError::Unsupported {
            what: "writing a property onto an edge of a table that stores none on its edges",
            id: rel.table,
        });
    }
    fill(rel.table, "edge", &rel.props, columns, values)
}

/// The cells of one new row of one table: every column of it, in column
/// order, filled from the properties the pattern wrote.
///
/// `noun` is what is being written, which is all that separates the
/// message an element gets from the one an edge gets.
fn fill(
    table: u32,
    noun: &str,
    props: &[(String, BoundExpr)],
    columns: &[PropColumn],
    values: &[Value],
) -> Result<Vec<(u32, Cell)>> {
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
                id: table,
            });
        }
        let written = props
            .iter()
            .position(|(key, _)| *key == col.name)
            .ok_or_else(|| {
                ZuError::InvalidArgument(format!(
                    "the {noun} carries no value for column '{}', and every column of a new row has to hold one",
                    col.name
                ))
            })?;
        cells.push((ci as u32, cell(&col.ty, &values[written], &col.name)?));
    }
    for (key, _) in props {
        if !columns.iter().any(|col| col.name == *key) {
            return Err(ZuError::InvalidArgument(format!(
                "the {noun} carries '{key}', which is not a column of the table it is created in"
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
pub(crate) fn cell(ty: &LogicalType, value: &Value, key: &str) -> Result<Cell> {
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
pub(crate) fn describe(value: &Value) -> &'static str {
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
/// order [`Batch::row`] reads them back, which is every node's in
/// written order and then every edge's.
pub(crate) fn value_exprs(nodes: &[BoundInsertNode], rels: &[BoundInsertRel]) -> Vec<BoundExpr> {
    let node_values = nodes.iter().flat_map(|node| node.props.iter());
    let rel_values = rels.iter().flat_map(|rel| rel.props.iter());
    node_values
        .chain(rel_values)
        .map(|(_, expr)| expr.clone())
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

    /// An edge type that names no rel table is a table to be made, the
    /// way a node label that names none is (`crate::declare`), as long
    /// as the pattern says which two node tables the edge runs between.
    /// When it does not, there is nothing to make and the statement is
    /// a reference error rather than a parse error: it is well formed
    /// and mentions something that is not there.
    #[test]
    fn an_edge_type_that_names_no_table_and_no_ends_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "label.zu1");

        let err = session
            .run(
                "MATCH (a), (b:person {name: 'kay'}) INSERT (a)-[e:employs]->(b)",
                &[],
            )
            .expect_err("nothing says which table the edge leaves");
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
                // The write has folded by the time the row is handed
                // on, so the edge has a row of its own: the second of
                // the table, behind the one the fixture loaded.
                ord: 1,
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

    /// Two people, one edge, and a year stored on that edge. A rel
    /// table stores its columns over the edges in load order, so this
    /// is the fixture for a write that has to slot a value into that
    /// order rather than append to the end of it.
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
        crate::zu1::props::store_rel_props(
            &mut db,
            "knows",
            &[("since", PropValues::Int(&[1990]))],
        )
        .expect("edge props");
    }

    fn open_with_edge_props(dir: &tempfile::TempDir, name: &str) -> Session {
        let path = dir.path().join(name);
        seeded_with_edge_props(&path);
        Session::open(&path).expect("open")
    }

    /// A second edge over a pair a table storing edge properties
    /// already holds is refused where the statement runs, not where the
    /// fold would have found it, so the log never takes a record the
    /// fold cannot fold.
    #[test]
    fn a_second_edge_over_one_pair_is_refused_before_it_is_logged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_edge_props(&dir, "second-edge.zu1");

        let err = session
            .run(
                "MATCH (a:person), (b:person) WHERE a.name = 'ada' AND b.name = 'kay' \
                 INSERT (a)-[:knows {since: 2020}]->(b)",
                &[],
            )
            .expect_err("rows 0 and 1 already run through knows");
        let said = err.to_string();
        assert!(said.contains("rows 0 and 1"), "{said}");
        assert!(said.contains("knows"), "{said}");

        // Refused where it was written, so nothing is in the file and
        // the next statement reads the store it always read.
        let out = session
            .run("MATCH ()-[k:knows]->() RETURN k.since", &[])
            .expect("the store is intact");
        assert_eq!(out.rows.len(), 1);

        // Twice in one statement is the same refusal, and neither of
        // the two is in the store to be found by the other.
        let err = session
            .run(
                "INSERT (x:person {age: 1, name: 'a'})-[:knows {since: 1}]->(y:person {age: 2, name: 'b'}), \
                 (x)-[:knows {since: 2}]->(y)",
                &[],
            )
            .expect_err("one statement, one pair, two edges");
        assert!(err.to_string().contains("rows 2 and 3"), "{err}");
    }

    /// The case the milestone line is about: an edge is written
    /// carrying a value, and the value is there to read afterwards.
    #[test]
    fn an_edge_is_written_carrying_a_property() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_edge_props(&dir, "edge-props.zu1");

        session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe'})-[:knows {since: 2020}]->(y:person {age: 40, name: 'raj'})",
                &[],
            )
            .expect("insert an edge carrying a year");

        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) RETURN a.name AS from, k.since AS since ORDER BY from",
                &[],
            )
            .expect("read the years back");
        assert_eq!(strings(&out, 0), ["ada", "zoe"]);
        assert_eq!(
            out.rows[0][1],
            Value::Int(1990),
            "the seeded edge is where it was"
        );
        assert_eq!(
            out.rows[1][1],
            Value::Int(2020),
            "and the written one holds what it was written with"
        );
    }

    /// The value is an expression evaluated per row, so a write after a
    /// match writes a different year on every edge it makes.
    #[test]
    fn an_edge_property_is_an_expression_per_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_edge_props(&dir, "edge-props-rows.zu1");

        session
            .run(
                "MATCH (a:person) INSERT (a)-[:knows {since: a.age + 2000}]->(b:person {age: 1, name: 'new'})",
                &[],
            )
            .expect("one edge per person found");

        let out = session
            .run(
                "MATCH (a:person)-[k:knows]->(b:person) WHERE b.name = 'new' RETURN a.name AS from, k.since AS since ORDER BY from",
                &[],
            )
            .expect("read them back");
        assert_eq!(strings(&out, 0), ["ada", "kay"]);
        assert_eq!(out.rows[0][1], Value::Int(2010));
        assert_eq!(out.rows[1][1], Value::Int(2020));
    }

    /// A column of a rel table holds one value per edge, so an edge
    /// written without one has nothing to put there, and the message
    /// names the column.
    #[test]
    fn an_edge_leaving_a_column_out_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open_with_edge_props(&dir, "edge-missing.zu1");

        let err = session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe'})-[:knows]->(y:person {age: 40, name: 'raj'})",
                &[],
            )
            .expect_err("the year was left out");
        assert!(err.to_string().contains("'since'"), "got: {err}");

        let after = session.run("MATCH (p:person) RETURN count(*) AS n", &[]);
        assert_eq!(
            after.expect("read").rows[0][0],
            Value::Int(2),
            "nothing was written"
        );
    }

    /// A table that stores nothing on its edges has nowhere to put a
    /// value, and says so rather than dropping it.
    #[test]
    fn a_property_on_an_edge_of_a_bare_table_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "edge-nowhere.zu1");

        let err = session
            .run(
                "INSERT (x:person {age: 30, name: 'zoe'})-[:knows {since: 2020}]->(y:person {age: 40, name: 'raj'})",
                &[],
            )
            .expect_err("the table stores nothing on its edges");
        assert!(
            err.to_string().contains("stores none on its edges"),
            "got: {err}"
        );
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
    /// An element carries at most what zu declares it carries, and the
    /// limit is declared rather than absent so that a statement asking
    /// for more is told the standard's answer for hitting one. The
    /// count is the pattern's own, so this is settled before a column
    /// is read and the table it names never matters.
    #[test]
    fn an_element_carrying_more_properties_than_zu_holds_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "wide-element.zu1");

        let props: Vec<String> = (0..=MAX_PROPERTIES).map(|n| format!("p{n}: {n}")).collect();
        let err = session
            .run(&format!("INSERT (x:person {{{}}})", props.join(", ")), &[])
            .expect_err("wider than an element goes");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22G0S"));
        assert!(err.to_string().contains("4096"), "got: {err}");
    }

    /// The same limit on an edge, which the standard gives its own code
    /// because an engine can hold different numbers on the two.
    #[test]
    fn an_edge_carrying_more_properties_than_zu_holds_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "wide-edge.zu1");

        let props: Vec<String> = (0..=MAX_PROPERTIES).map(|n| format!("p{n}: {n}")).collect();
        let err = session
            .run(
                &format!(
                    "MATCH (a:person {{name: 'ada'}}), (b:person {{name: 'kay'}}) INSERT (a)-[e:knows {{{}}}]->(b)",
                    props.join(", ")
                ),
                &[],
            )
            .expect_err("wider than an edge goes");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("22G0T"));
        assert!(err.to_string().contains("4096"), "got: {err}");
    }

    /// A delete leaves the element bound, so the same statement can go
    /// on to write an edge onto it. The row is not there any more, and
    /// an edge to a row that is not there is an edge no reader can
    /// resolve, so it is refused by the code the standard has for
    /// exactly that rather than written into the neighbour lists.
    #[test]
    fn an_edge_onto_an_element_a_delete_took_away_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "deleted-end.zu1");

        let err = session
            .run(
                "MATCH (a:person {name: 'ada'}), (b:person {name: 'kay'}) DETACH DELETE b INSERT (a)-[e:knows]->(b)",
                &[],
            )
            .expect_err("the far end is gone");
        assert_eq!(err.gqlstatus().map(|s| s.code()), Some("G1002"));
        assert!(err.to_string().contains("taken away"), "got: {err}");
    }

    /// A rel table runs between two node tables and a statement can
    /// name an element in neither of them. Where the binder can see
    /// which table an endpoint is bound over it settles it there, and
    /// this is that case.
    #[test]
    fn an_edge_onto_an_element_of_another_table_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "wrong-table.zu1");

        session
            .run("INSERT (c:city {name: 'oslo'})", &[])
            .expect("a second table");
        let err = session
            .run(
                "MATCH (a:person {name: 'ada'}), (c:city) INSERT (a)-[e:knows]->(c)",
                &[],
            )
            .expect_err("the far end is the wrong table");
        assert_eq!(
            err.gqlstatus().map(|s| s.code()),
            Some("42002"),
            "got: {err}"
        );
    }

    /// The write asks the same question again, because a slot the
    /// binder left open over several tables is only one table by the
    /// time a row of it is in hand. Which of the two answers comes
    /// back turns on whether the element is in the graph being written
    /// in at all: another table of the same graph is a type question,
    /// and another graph is a reference to something the statement is
    /// not working on.
    #[test]
    fn the_write_names_the_table_and_the_graph_an_endpoint_missed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = open(&dir, "ends.zu1");

        session
            .run("INSERT (c:city {name: 'oslo'})", &[])
            .expect("a second table of the same graph");
        session
            .run("CREATE GRAPH other AS COPY OF home", &[])
            .expect("a second graph");
        let catalog = session.catalog().clone();

        let person = catalog.node_by_name("person").expect("person").id;
        let city = catalog.node_by_name("city").expect("city").id;
        let elsewhere = catalog
            .node_tables()
            .iter()
            .find(|t| t.name == "person" && t.id != person)
            .expect("the copy")
            .id;

        let same = wrong_end(&catalog, city, 0, person, "arrives at");
        assert_eq!(
            same.gqlstatus().map(|s| s.code()),
            Some("22G0W"),
            "got: {same}"
        );
        assert!(same.to_string().contains("city"), "got: {same}");

        let across = wrong_end(&catalog, elsewhere, 0, person, "leaves");
        assert_eq!(
            across.gqlstatus().map(|s| s.code()),
            Some("G1003"),
            "got: {across}"
        );
        assert!(
            across.to_string().contains("not in the graph"),
            "got: {across}"
        );
    }
}
