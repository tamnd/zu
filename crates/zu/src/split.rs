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
    BoundClause, BoundExpr, BoundInsertNode, BoundInsertRel, BoundItem, BoundQuery, Schema, Type,
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

/// The write one part ends in, and what the row on either side of it
/// holds.
pub(crate) struct Write {
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

/// Splits a bound statement into the parts the session runs, or
/// answers `None` for a statement that writes nothing and runs as one
/// plan the way every read does.
pub(crate) fn split(query: &BoundQuery, schema: &Schema) -> Result<Option<Vec<Part>>> {
    if !query
        .clauses
        .iter()
        .any(|c| matches!(c, BoundClause::Insert { .. }))
    {
        return Ok(None);
    }
    let mut parts: Vec<Part> = Vec::new();
    let mut rest = query.clauses.as_slice();
    // The slots the next part reads back in, which is nothing for the
    // first one: it runs over the store the way a read does.
    let mut base: Option<Vec<usize>> = None;
    loop {
        let at = rest
            .iter()
            .position(|c| matches!(c, BoundClause::Insert { .. }));
        let Some(at) = at else {
            let part = compile(query, rest.to_vec(), query.columns.clone(), base, schema)?;
            parts.push(Part {
                query: part.0,
                plan: part.1,
                write: None,
            });
            return Ok(Some(parts));
        };
        let BoundClause::Insert { nodes, rels, carry } = &rest[at] else {
            unreachable!("the position that matched");
        };
        let mut clauses = rest[..at].to_vec();
        let exprs = carry
            .iter()
            .map(|slot| BoundExpr::Var(*slot))
            .chain(crate::insert::value_exprs(nodes));
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
        let created: Vec<usize> = nodes
            .iter()
            .map(|node| node.slot)
            .chain(rels.iter().map(|rel| rel.slot))
            .collect();
        base = Some(
            carry
                .iter()
                .copied()
                .chain(created.iter().copied())
                .collect(),
        );
        parts.push(Part {
            query: part.0,
            plan: part.1,
            write: Some(Write {
                nodes: nodes.clone(),
                rels: rels.clone(),
                carry: carry.clone(),
                created,
            }),
        });
        rest = &rest[at + 1..];
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
