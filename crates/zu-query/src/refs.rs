//! Reference values, GV60 and GV61.
//!
//! A graph and a binding table are values in GQL, not statements and
//! not data. `USE` names one, a parameter carries one, a procedure
//! binds one, and a session assigns one, and all four want the same
//! thing: something small that says which graph or which table, that
//! can be copied around and compared, and that does not drag the
//! contents with it. That is a handle, and this module is the two of
//! them.
//!
//! The part worth stating is what a handle is *for*. A graph reference
//! is a catalog id and the epoch it was taken at, which is enough to
//! find the graph again and enough to notice that the graph is gone. A
//! binding table reference is the rows, held behind an `Arc` so that
//! passing one costs a pointer, plus an identity so that two of them
//! can be told apart without walking the rows. Neither is a value the
//! language can build out of literals: the engine hands them out and
//! the query passes them along.
//!
//! Lifetime is the part that is easy to get wrong. Both handles record
//! the epoch they were taken at, because both can outlive it: a graph
//! can be dropped and a snapshot can move under the element references
//! a table holds. The check that reads the epoch belongs to whoever
//! knows what the current one is, which is the session, so what lives
//! here is the epoch and the question, and `zu`'s session asks it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::exec::Value;

/// GV60. A graph reference: which graph, and when it was named.
///
/// The id is the handle and the two names are for diagnostics, which
/// is the right way round. Resolving by name on every use would let
/// the graph a session is working in change under it when someone
/// drops a graph and makes another with the same name; resolving once
/// and carrying the id means the reference either still names that
/// graph or names one that is gone, and both of those are answers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphHandle {
    /// The catalog id of the graph, which is what a statement runs
    /// against.
    pub id: u32,
    /// The schema the graph is in, `/` for the root one.
    pub schema: String,
    /// The graph's name, as the catalog spells it.
    pub name: String,
    /// The epoch the reference was taken at. A later statement can
    /// still use the handle; the epoch is what makes "this graph has
    /// been dropped since" a thing the engine can say.
    pub epoch: u64,
}

impl GraphHandle {
    /// A handle on the graph `id` in `schema`, taken at `epoch`.
    pub fn new(id: u32, schema: impl Into<String>, name: impl Into<String>, epoch: u64) -> Self {
        GraphHandle {
            id,
            schema: schema.into(),
            name: name.into(),
            epoch,
        }
    }

    /// How the handle reads in a result and in a diagnostic:
    /// `GRAPH /social`, the schema and the name, which is what a user
    /// wrote to get it.
    pub fn label(&self) -> String {
        if self.schema == "/" {
            format!("GRAPH /{}", self.name)
        } else {
            format!("GRAPH {}/{}", self.schema, self.name)
        }
    }
}

/// The next handle number. Handles are numbered rather than compared
/// by contents, because a binding table reference is a reference: two
/// of them are the same when they name the same table, and two tables
/// that happen to hold the same rows are still two tables. Numbering
/// them also keeps DISTINCT and ORDER BY over references deterministic
/// within a process, which comparing addresses would not.
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// GV61. A binding table reference: the rows of a result, held once
/// and passed by handle.
///
/// The rows are materialised. ISO allows a pipelined table too, and
/// that one has to keep a snapshot alive while something reads it;
/// this one has already read it, so what it owes instead is the epoch
/// it read at, because the element references in its rows name rows of
/// that snapshot and of no other.
#[derive(Debug)]
pub struct BindingTable {
    /// The handle number, which is the table's identity.
    id: u64,
    /// Column names, in the order the statement returned them.
    columns: Vec<String>,
    /// One entry per row, each as long as `columns`.
    rows: Vec<Vec<Value>>,
    /// The epoch the rows were read at.
    epoch: u64,
}

impl BindingTable {
    /// A table over the columns and rows a statement produced, taken
    /// at `epoch`.
    ///
    /// It comes back inside an `Arc` because there is no use for one
    /// outside a handle: the whole point of the type is that copying
    /// the reference does not copy the rows.
    pub fn new(columns: Vec<String>, rows: Vec<Vec<Value>>, epoch: u64) -> Arc<BindingTable> {
        Arc::new(BindingTable {
            id: NEXT_HANDLE.fetch_add(1, Ordering::Relaxed),
            columns,
            rows,
            epoch,
        })
    }

    /// The table's identity, which is what two references to it share.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// The epoch the rows were read at.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The same table, recorded as read at `epoch`.
    ///
    /// A table built inside a statement is made at epoch nought,
    /// because a table that does not outlive its statement has no
    /// later to be stale in and nothing ever asks. A session parameter
    /// is the case where it does outlive it, and this is where it
    /// picks up the epoch it was read at. The handle number is kept,
    /// because it is the same table: nothing about it has changed
    /// except that somebody now knows when it was read. The rows are
    /// moved rather than copied whenever the handle is the only one,
    /// which it is on the path this exists for.
    pub fn held(table: Arc<BindingTable>, epoch: u64) -> Arc<BindingTable> {
        if table.epoch == epoch {
            return table;
        }
        let id = table.id;
        let (columns, rows) = match Arc::try_unwrap(table) {
            Ok(owned) => (owned.columns, owned.rows),
            Err(shared) => (shared.columns.clone(), shared.rows.clone()),
        };
        Arc::new(BindingTable {
            id,
            columns,
            rows,
            epoch,
        })
    }

    /// One row as a record, which is the value form of a row: field
    /// names from the columns, in name order like every other record.
    pub fn record(&self, row: usize) -> Option<Value> {
        let row = self.rows.get(row)?;
        let fields = self
            .columns
            .iter()
            .cloned()
            .zip(row.iter().cloned())
            .collect();
        Some(Value::record(fields))
    }

    /// Whether any cell holds an element reference, directly or inside
    /// a list, a record or a path.
    ///
    /// This is the question the epoch is for. A table of numbers and
    /// strings means the same thing at every epoch, so carrying one
    /// forward is harmless; a table holding a node means a row of one
    /// snapshot, and the row a later snapshot has at that offset may
    /// belong to something else. The walk is over the rows because
    /// nothing else knows: a column has no declared type here yet.
    pub fn holds_elements(&self) -> bool {
        self.rows.iter().flatten().any(holds_element)
    }

    /// How the handle reads in a result: the shape rather than the
    /// contents, because printing the rows of a table that was passed
    /// by reference would defeat passing it by reference.
    pub fn label(&self) -> String {
        let cols = self.columns.len();
        let rows = self.rows.len();
        let plural = |n: usize, word: &str| {
            if n == 1 {
                format!("{n} {word}")
            } else {
                format!("{n} {word}s")
            }
        };
        format!(
            "BINDING TABLE #{} ({}, {})",
            self.id,
            plural(cols, "column"),
            plural(rows, "row")
        )
    }
}

/// Two references are the same reference when they name the same
/// table. Cloning the `Arc` keeps the number, so a handle passed to
/// three statements is one table in all three.
impl PartialEq for BindingTable {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for BindingTable {}

fn holds_element(v: &Value) -> bool {
    match v {
        Value::Node { .. } | Value::Rel { .. } | Value::Path(_) | Value::Chain(_) => true,
        Value::List(items) => items.iter().any(holds_element),
        Value::Record(fields) => fields.iter().any(|(_, v)| holds_element(v)),
        // A graph reference is a catalog id and survives a snapshot
        // moving; a nested binding table answers for itself.
        Value::BindingTable(t) => t.holds_elements(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_tables_over_the_same_rows_are_two_references() {
        let rows = vec![vec![Value::Int(1)]];
        let a = BindingTable::new(vec!["n".into()], rows.clone(), 7);
        let b = BindingTable::new(vec!["n".into()], rows, 7);
        assert_ne!(a, b);
        assert_eq!(a, Arc::clone(&a));
    }

    #[test]
    fn a_row_reads_as_a_record() {
        let t = BindingTable::new(
            vec!["b".into(), "a".into()],
            vec![vec![Value::Int(1), Value::Int(2)]],
            0,
        );
        assert_eq!(
            t.record(0),
            Some(Value::record(vec![
                ("b".into(), Value::Int(1)),
                ("a".into(), Value::Int(2)),
            ]))
        );
        assert_eq!(t.record(1), None);
    }

    #[test]
    fn only_a_table_holding_elements_is_tied_to_its_epoch() {
        let plain = BindingTable::new(vec!["n".into()], vec![vec![Value::Int(1)]], 3);
        assert!(!plain.holds_elements());
        let nested = BindingTable::new(
            vec!["n".into()],
            vec![vec![Value::List(vec![Value::Node {
                table: 1,
                offset: 4,
            }])]],
            3,
        );
        assert!(nested.holds_elements());
    }

    #[test]
    fn a_graph_handle_reads_as_its_schema_and_name() {
        assert_eq!(
            GraphHandle::new(1, "/", "social", 9).label(),
            "GRAPH /social"
        );
        assert_eq!(
            GraphHandle::new(2, "app", "social", 9).label(),
            "GRAPH app/social"
        );
    }
}
