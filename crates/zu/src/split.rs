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

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};
use zu_query::binder::{
    BoundClause, BoundExpr, BoundInsertNode, BoundInsertRel, BoundItem, BoundQuery, BoundSetItem,
    MatchKind, Schema, Type,
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
    /// Boxed, because a merge carries the walk it runs as well as the
    /// write, and every other arm would be as big as that one.
    Merge(Box<Merge>),
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
            Write::Merge(merge) => &merge.insert.carry,
        }
    }

    /// The clauses the write puts at the end of the part before it.
    ///
    /// A `MERGE` is the one write that reads something of its own: the
    /// walk that decides whether it writes anything is part of the read
    /// half rather than of the write, so it goes in here and the rows
    /// arrive with the answer already in them.
    fn before(&self) -> Option<BoundClause> {
        match self {
            Write::Merge(merge) => Some(merge.probe.clone()),
            _ => None,
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
            // The properties the written elements take, and then what
            // `ON MATCH SET` assigns. A row gets one or the other used
            // and both worked out, because which of the two a row is
            // for is not known until the walk has run and the walk is
            // what produced the row.
            Write::Merge(merge) => {
                crate::insert::value_exprs(&merge.insert.nodes, &merge.insert.rels)
                    .into_iter()
                    .chain(merge.matched.items.iter().map(|item| item.value.clone()))
                    .collect()
            }
        }
    }

    /// The slots the write binds, which the next part reads on the end
    /// of the row. A `SET` binds nothing: it changes what a name
    /// already stands for.
    fn created(&self) -> &[usize] {
        match self {
            Write::Insert(insert) => &insert.created,
            // A merge binds the pattern's slots, and the walk in front
            // of it already put them in the row, so they are in the
            // carry rather than behind it.
            Write::Set(_) | Write::Delete(_) | Write::Merge(_) => &[],
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
    /// The delete items written as `VALUE { ... }`, compiled. Each one
    /// runs on its own and answers the one element that item takes
    /// away.
    pub(crate) queries: Vec<Subquery>,
    /// The slots the part before the write projects. A delete carries
    /// nothing behind them.
    pub(crate) carry: Vec<usize>,
    /// Whether the edges on the elements go with them.
    pub(crate) detach: bool,
}

/// A `MERGE`: the walk that looks for the pattern, the elements written
/// when it finds none, and the assignments made when it finds one.
///
/// The insert is over the pattern's own slots, and those slots are in
/// the carry rather than behind it, because the walk in front of the
/// write already put them in the row. That is also what says which of
/// the two halves a row is for.
pub(crate) struct Merge {
    /// The walk, as an optional match, run at the end of the part
    /// before the write.
    pub(crate) probe: BoundClause,
    pub(crate) insert: Insert,
    /// `ON MATCH SET`, over the same carry. Its values follow the
    /// insert's in the row.
    pub(crate) matched: Set,
    /// Where the pattern's own slots start in the row. The walk is
    /// optional, so null there is a row it found nothing for, and that
    /// is the row the insert runs for.
    pub(crate) at: usize,
    /// The positions in the row of the endpoints the pattern was given
    /// rather than wrote. Two rows that agree on these and on the
    /// properties are merging one thing, which is what stops a
    /// statement writing it twice.
    pub(crate) ends: Vec<usize>,
    /// How many of the values behind the row are the insert's. The rest
    /// are what `ON MATCH SET` assigns.
    pub(crate) props: usize,
}

/// A query nested inside a statement, compiled the way the statement
/// around it was and run on its own. `DELETE VALUE { ... }` is the one
/// place a statement holds one.
pub(crate) struct Subquery {
    pub(crate) query: BoundQuery,
    pub(crate) plan: LogicalPlan,
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
        let write = write_of(&rest[at], schema)?;
        let mut clauses = rest[..at].to_vec();
        clauses.extend(write.before());
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
        BoundClause::Insert { .. }
            | BoundClause::Set { .. }
            | BoundClause::Delete { .. }
            | BoundClause::Merge { .. }
    )
}

/// The write a clause [`is_write`] answered for describes.
fn write_of(clause: &BoundClause, schema: &Schema) -> Result<Write> {
    Ok(match clause {
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
        BoundClause::Delete {
            slots,
            queries,
            carry,
            detach,
        } => Write::Delete(Delete {
            slots: slots.clone(),
            queries: queries
                .iter()
                .map(|nested| nested_query(nested, schema))
                .collect::<Result<Vec<_>>>()?,
            carry: carry.clone(),
            detach: *detach,
        }),
        BoundClause::Merge {
            probe,
            filter,
            nodes,
            rels,
            on_match,
            carry,
            at,
        } => {
            let written: Vec<usize> = nodes
                .iter()
                .map(|node| node.slot)
                .chain(rels.iter().map(|rel| rel.slot))
                .collect();
            // An end the pattern did not write is one it was given, and
            // where it sits in the row is where its slot sits in the
            // carry.
            let mut ends: Vec<usize> = rels
                .iter()
                .flat_map(|rel| [rel.src, rel.dst])
                .filter(|slot| !written.contains(slot))
                .filter_map(|slot| carry.iter().position(|held| *held == slot))
                .collect();
            ends.sort_unstable();
            ends.dedup();
            Write::Merge(Box::new(Merge {
                probe: BoundClause::Match {
                    kind: MatchKind::Optional,
                    patterns: vec![probe.clone()],
                    filter: filter.clone(),
                },
                insert: Insert {
                    nodes: nodes.clone(),
                    rels: rels.clone(),
                    carry: carry.clone(),
                    created: written,
                },
                matched: Set {
                    items: on_match.clone(),
                    carry: carry.clone(),
                },
                at: *at,
                ends,
                props: nodes
                    .iter()
                    .map(|node| node.props.len())
                    .chain(rels.iter().map(|rel| rel.props.len()))
                    .sum(),
            }))
        }
        _ => unreachable!("the position that matched"),
    })
}

/// Compiles the query inside a `DELETE VALUE { ... }` against the same
/// schema the statement around it is compiled against, so the nested
/// query reads the graph the outer `USE` named.
///
/// A nested query that writes is refused here rather than run. The
/// delete item is a value expression and a value expression does not
/// change the graph, and an engine that ran one anyway would be
/// deciding on its own whether the inner write happened before or after
/// the outer delete.
fn nested_query(parsed: &zu_query::ast::Query, schema: &Schema) -> Result<Subquery> {
    let (query, plan, _) = crate::query::compile_parsed(parsed, schema)?;
    if query.clauses.iter().any(is_write) {
        return Err(ZuError::gql(
            codes::C42001,
            "the query inside DELETE VALUE answers the element to delete, so it reads and does not write",
        ));
    }
    Ok(Subquery { query, plan })
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
        // A part is one pipeline of the statement being taken apart at
        // its write, and a statement that writes is one operand: the
        // binder refuses a write in a composite.
        conjoined: Vec::new(),
        // Every part carries the statement's value query expressions,
        // because a part is a slice of the same statement and
        // [`BoundExpr::Scalar`] indexes them by position.
        scalars: query.scalars.clone(),
        // A part is a slice of a statement and not a query written
        // inside one, so it reads nothing from a query around it and
        // nothing around it asks whether it answered a row.
        captures: Vec::new(),
        exists: false,
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
