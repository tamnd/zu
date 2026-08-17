//! Splitting a statement at the clause that writes.
//!
//! A statement that writes is two statements with a write between
//! them, and it has to be run that way. The clauses before the write
//! say which rows it runs for, the write runs once for each of them,
//! and the clauses after it read what came of it. Running the whole
//! plan against the store afterwards would answer a different
//! question: the operators under the write would find the rows the
//! write just made and the statement would meet its own output.
//!
//! So the run is a chain of parts. Each part but the last ends in a
//! projection of the row the write carries across it, the write's
//! property values behind it, and the created elements are appended to
//! that row before the next part reads it back through
//! [`zu_query::plan::LogicalPlan::Rows`].

use zu_common::Result;
use zu_query::binder::{
    BoundClause, BoundExpr, BoundInsertNode, BoundInsertRel, BoundItem, BoundQuery, BoundSetItem,
    Schema, Type,
};
use zu_query::plan::LogicalPlan;
use zu_query::{optimizer, plan};

/// One part of a split statement: the clauses that run together, and
/// the write they end in, if they end in one.
pub(crate) struct Part {
    pub(crate) query: BoundQuery,
    pub(crate) plan: LogicalPlan,
    pub(crate) write: Option<Write>,
}

/// The write one part ends in.
///
/// The three of them are the three things a write does to a row: make
/// elements the clauses after it read, change what elements already
/// there hold, or take elements away. What they share is the row on
/// either side, which is why the split is written once over all three.
pub(crate) enum Write {
    Insert(Insert),
    Set(Set),
    Delete(Delete),
}

impl Write {
    /// The slots the part before the write projects, in the order its
    /// rows hold them. The write's own values follow them in the same
    /// row.
    pub(crate) fn carry(&self) -> &[usize] {
        match self {
            Write::Insert(insert) => &insert.carry,
            Write::Set(set) => &set.carry,
            Write::Delete(delete) => &delete.carry,
        }
    }

    /// The value expressions the projection at the end of the part
    /// carries behind the row, in the order the write reads them back.
    fn values(&self) -> Vec<BoundExpr> {
        match self {
            Write::Insert(insert) => crate::insert::value_exprs(&insert.nodes, &insert.rels),
            Write::Set(set) => set.items.iter().map(|item| item.value.clone()).collect(),
            // A delete names the elements it takes away and computes
            // nothing, so there is nothing behind the row.
            Write::Delete(_) => Vec::new(),
        }
    }

    /// The slots the write binds, which the next part reads on the end
    /// of the row. A `SET` binds nothing: it changes what a name
    /// already stands for.
    fn created(&self) -> &[usize] {
        match self {
            Write::Insert(insert) => &insert.created,
            Write::Set(_) | Write::Delete(_) => &[],
        }
    }
}

/// An `INSERT`: what it writes, and what the row on either side of it
/// holds.
pub(crate) struct Insert {
    pub(crate) nodes: Vec<BoundInsertNode>,
    pub(crate) rels: Vec<BoundInsertRel>,
    /// The slots the part before the write projects, in the order its
    /// rows hold them. The property values follow them in the same
    /// row, one per property in written order.
    pub(crate) carry: Vec<usize>,
    /// The slots the created elements bind, nodes first, which is the
    /// order the write hands them back in and the order they are
    /// appended to the row.
    pub(crate) created: Vec<usize>,
}

/// A `SET`: the assignments it makes, and the row it carries across
/// itself unchanged.
pub(crate) struct Set {
    pub(crate) items: Vec<BoundSetItem>,
    /// The slots the part before the write projects. The values the
    /// assignments take follow them in the same row, one per item in
    /// written order.
    pub(crate) carry: Vec<usize>,
}

/// A `DELETE`: the slots it takes away, and the row it carries across
/// itself unchanged. The slots stay named on the other side, because
/// GQL leaves a deleted element bound and reading one is 22G11.
pub(crate) struct Delete {
    pub(crate) slots: Vec<usize>,
    /// The slots the part before the write projects. A delete carries
    /// nothing behind them.
    pub(crate) carry: Vec<usize>,
}

/// Splits a bound statement into the parts the session runs, or
/// answers `None` for a statement that writes nothing and runs as one
/// plan the way every read does.
pub(crate) fn split(query: &BoundQuery, schema: &Schema) -> Result<Option<Vec<Part>>> {
    if !query.clauses.iter().any(is_write) {
        return Ok(None);
    }
    let mut parts: Vec<Part> = Vec::new();
    let mut rest = query.clauses.as_slice();
    // The slots the next part reads back in, which is nothing for the
    // first one: it runs over the store the way a read does.
    let mut base: Option<Vec<usize>> = None;
    loop {
        let Some(at) = rest.iter().position(is_write) else {
            let part = compile(query, rest.to_vec(), query.columns.clone(), base, schema)?;
            parts.push(Part {
                query: part.0,
                plan: part.1,
                write: None,
            });
            return Ok(Some(parts));
        };
        let write = write_of(&rest[at]);
        let mut clauses = rest[..at].to_vec();
        let exprs = write
            .carry()
            .iter()
            .map(|slot| BoundExpr::Var(*slot))
            .chain(write.values());
        let items: Vec<BoundItem> = exprs.enumerate().map(|(i, expr)| item(expr, i)).collect();
        let columns = (0..items.len()).map(|i| format!("#{i}")).collect();
        clauses.push(BoundClause::Project {
            distinct: false,
            items,
            order_by: Vec::new(),
            skip: None,
            limit: None,
            filter: None,
        });
        let part = compile(query, clauses, columns, base, schema)?;
        base = Some(
            write
                .carry()
                .iter()
                .copied()
                .chain(write.created().iter().copied())
                .collect(),
        );
        parts.push(Part {
            query: part.0,
            plan: part.1,
            write: Some(write),
        });
        rest = &rest[at + 1..];
    }
}

/// Whether a clause is one the statement has to be split at.
fn is_write(clause: &BoundClause) -> bool {
    matches!(
        clause,
        BoundClause::Insert { .. } | BoundClause::Set { .. } | BoundClause::Delete { .. }
    )
}

/// The write a clause [`is_write`] answered for describes.
fn write_of(clause: &BoundClause) -> Write {
    match clause {
        BoundClause::Insert { nodes, rels, carry } => Write::Insert(Insert {
            nodes: nodes.clone(),
            rels: rels.clone(),
            carry: carry.clone(),
            created: nodes
                .iter()
                .map(|node| node.slot)
                .chain(rels.iter().map(|rel| rel.slot))
                .collect(),
        }),
        BoundClause::Set { items, carry } => Write::Set(Set {
            items: items.clone(),
            carry: carry.clone(),
        }),
        BoundClause::Delete { slots, carry } => Write::Delete(Delete {
            slots: slots.clone(),
            carry: carry.clone(),
        }),
        _ => unreachable!("the position that matched"),
    }
}

/// One item of the projection a part ends in. The name is positional
/// because nothing reads these by name: the next part reads the row by
/// position and the slots say what each column is.
fn item(expr: BoundExpr, i: usize) -> BoundItem {
    BoundItem {
        expr,
        ty: Type::Any,
        name: format!("#{i}"),
        slot: None,
        aggregate: false,
    }
}

/// Binds one part's clauses into a query of their own and plans it.
///
/// Everything but the clauses carries over from the statement: the
/// slots are the statement's slots, and a part reading a slot the
/// part before it projected reads the same variable under the same
/// name, which is what makes the rows line up.
fn compile(
    query: &BoundQuery,
    clauses: Vec<BoundClause>,
    columns: Vec<String>,
    base: Option<Vec<usize>>,
    schema: &Schema,
) -> Result<(BoundQuery, LogicalPlan)> {
    let query = BoundQuery {
        clauses,
        variables: query.variables.clone(),
        params: query.params.clone(),
        columns,
        path_shapes: query.path_shapes.clone(),
        labels: query.labels.clone(),
    };
    let leaf = match base {
        // The rows the part before this one carried arrive as one
        // argument past the last declared parameter, the same place
        // every part reads them from, because a part reads at most one
        // of them.
        Some(slots) => LogicalPlan::Rows {
            slots,
            arg: query.params.len(),
        },
        None => LogicalPlan::Empty,
    };
    let built = plan::build_over(&query, leaf)?;
    let plan = optimizer::optimize(built, &query, schema)?;
    Ok((query, plan))
}
