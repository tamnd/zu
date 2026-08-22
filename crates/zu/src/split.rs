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

use std::collections::BTreeSet;

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};
use zu_query::binder::{
    BoundClause, BoundExpr, BoundInsertNode, BoundInsertRel, BoundItem, BoundPath, BoundQuery,
    BoundSetItem, ForkBranch, MatchKind, Schema, Type,
};
use zu_query::exec::{OrdValue, QueryResult, Value};
use zu_query::plan::LogicalPlan;
use zu_query::{optimizer, plan};

/// One part of a split statement: the clauses that run together, and
/// the seam they end in, if they end in one. The last part ends in
/// none of them and answers the statement's rows.
pub(crate) struct Part {
    pub(crate) query: BoundQuery,
    pub(crate) plan: LogicalPlan,
    pub(crate) seam: Option<Seam>,
}

/// What one part ends in.
///
/// A write is one seam and a match written several ways is the other,
/// and they are split at for the same reason: what the clauses after
/// them read is not what one pipeline over the store would answer, so
/// the rows are handed across rather than walked through.
pub(crate) enum Seam {
    Write(Write),
    Fork(Fork),
}

/// A match written several ways, ISO 16.7 and features G030 and G032.
///
/// Each way is a plan of its own over the rows the part before it
/// carried, and the rows they answer are put end to end. That is what
/// the shape is for: the ways walk differently and bind slots of their
/// own, and the row is where they meet.
pub(crate) struct Fork {
    pub(crate) branches: Vec<Branch>,
    /// Whether a path two ways both found is answered once, which is
    /// the path pattern union of G032, or twice, which is the multiset
    /// alternation of G030.
    pub(crate) distinct: bool,
}

/// One way of a [`Fork`], compiled.
pub(crate) struct Branch {
    pub(crate) query: BoundQuery,
    pub(crate) plan: LogicalPlan,
    /// How many of the columns of a row this way answers the next part
    /// reads. Under `|` the elements the way walked follow them, so
    /// that two ways that found the very same path answer one row and
    /// two ways that found different paths through the same pair of
    /// endpoints answer two; they are dropped once that is settled.
    pub(crate) width: usize,
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
/// answers `None` for a statement that runs as one plan the way every
/// ordinary read does.
pub(crate) fn split(
    query: &BoundQuery,
    schema: &Schema,
    session_schema: &str,
) -> Result<Option<Vec<Part>>> {
    if !query.clauses.iter().any(is_seam) {
        return Ok(None);
    }
    let mut parts: Vec<Part> = Vec::new();
    let mut rest = query.clauses.as_slice();
    // The slots the next part reads back in, which is nothing for the
    // first one: it runs over the store the way a read does.
    let mut base: Option<Vec<usize>> = None;
    loop {
        let Some(at) = rest.iter().position(is_seam) else {
            let part = compile(query, rest.to_vec(), query.columns.clone(), base, schema)?;
            parts.push(Part {
                query: part.0,
                plan: part.1,
                seam: None,
            });
            return Ok(Some(parts));
        };
        let mut clauses = rest[..at].to_vec();
        let (seam, exprs, into) = match &rest[at] {
            BoundClause::Fork {
                branches,
                distinct,
                carry,
                base,
            } => {
                let held: Vec<usize> = carry[..*base].to_vec();
                let exprs: Vec<BoundExpr> = held.iter().map(|slot| BoundExpr::Var(*slot)).collect();
                let fork = fork_of(query, branches, *distinct, held, schema)?;
                (Seam::Fork(fork), exprs, carry.clone())
            }
            other => {
                let write = write_of(other, schema, session_schema)?;
                clauses.extend(write.before());
                let exprs: Vec<BoundExpr> = write
                    .carry()
                    .iter()
                    .map(|slot| BoundExpr::Var(*slot))
                    .chain(write.values())
                    .collect();
                let into: Vec<usize> = write
                    .carry()
                    .iter()
                    .copied()
                    .chain(write.created().iter().copied())
                    .collect();
                (Seam::Write(write), exprs, into)
            }
        };
        let items: Vec<BoundItem> = exprs
            .into_iter()
            .enumerate()
            .map(|(i, expr)| item(expr, i))
            .collect();
        let columns = (0..items.len()).map(|i| format!("#{i}")).collect();
        clauses.push(BoundClause::Project {
            distinct: false,
            items,
            order_by: Vec::new(),
            order_aggs: Vec::new(),
            skip: None,
            limit: None,
            filter: None,
        });
        let part = compile(query, clauses, columns, base, schema)?;
        base = Some(into);
        parts.push(Part {
            query: part.0,
            plan: part.1,
            seam: Some(seam),
        });
        rest = &rest[at + 1..];
    }
}

/// How a caller runs one part: the plan, the query it belongs to, and
/// the arguments, in and the rows out. Whoever owns the store passes
/// one of these in, because a part is read the same way whether it is
/// read by a session or by a one-shot call.
pub(crate) type ReadPart<'a> =
    &'a mut dyn FnMut(&LogicalPlan, &BoundQuery, &[Value]) -> Result<QueryResult>;

/// Runs a statement split at forks and at nothing else, which is what a
/// statement that only reads is split at. The part in front of a fork
/// answers the rows its ways walk over, the ways walk them, and the
/// part with no seam behind it answers the statement's rows.
pub(crate) fn read_parts(
    parts: &[Part],
    args: &[Value],
    read: ReadPart<'_>,
) -> Result<QueryResult> {
    let mut carried: Option<Value> = None;
    for part in parts {
        let mut held = args.to_vec();
        held.extend(carried.take());
        match &part.seam {
            Some(Seam::Fork(fork)) => {
                let rows = read(&part.plan, &part.query, &held)?.rows;
                let seed = Value::List(rows.into_iter().map(Value::List).collect());
                carried = Some(fork_rows(fork, seed, args, read)?);
            }
            // A write needs the log and the overlay a session owns.
            // The one-shot entry point says so before it plans
            // anything, so nothing that got this far holds one.
            Some(Seam::Write(_)) => {
                return Err(ZuError::InvalidArgument(
                    "a statement that writes needs a session, which owns the log a write goes through: open one with zu::db::Database or zu::session::Session".into(),
                ));
            }
            None => return read(&part.plan, &part.query, &held),
        }
    }
    Err(ZuError::InvalidArgument(
        "a split statement ends in the part that answers its rows".into(),
    ))
}

/// Runs the ways of a fork over the rows the part in front of it
/// answered, and answers the rows they found between them, put end to
/// end in written order.
///
/// Under `|+|` that is the whole of it, because the alternation is of
/// multisets and a path found twice is answered twice. Under `|` it is
/// of sets, so a path two ways both found is answered once, and what
/// says whether two rows are the same path is the elements the way
/// walked, which each way projects behind its row for the purpose. Two
/// ways that reached the same pair of nodes over different edges walked
/// different paths and stay two rows.
pub(crate) fn fork_rows(
    fork: &Fork,
    seed: Value,
    args: &[Value],
    read: ReadPart<'_>,
) -> Result<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<Vec<OrdValue>> = BTreeSet::new();
    for branch in &fork.branches {
        let mut args = args.to_vec();
        args.push(seed.clone());
        let rows = read(&branch.plan, &branch.query, &args)?.rows;
        for mut row in rows {
            if fork.distinct && !seen.insert(row.iter().cloned().map(OrdValue).collect()) {
                continue;
            }
            row.truncate(branch.width);
            out.push(Value::List(row));
        }
    }
    Ok(Value::List(out))
}

/// Whether any part of a split statement changes the graph, which is
/// what says whether the run needs a transaction around it.
pub(crate) fn writes(parts: &[Part]) -> bool {
    parts
        .iter()
        .any(|part| matches!(part.seam, Some(Seam::Write(_))))
}

/// Whether a clause is one the statement has to be split at.
fn is_seam(clause: &BoundClause) -> bool {
    is_write(clause) || matches!(clause, BoundClause::Fork { .. })
}

/// Whether a clause changes the graph.
fn is_write(clause: &BoundClause) -> bool {
    matches!(
        clause,
        BoundClause::Insert { .. }
            | BoundClause::Set { .. }
            | BoundClause::Delete { .. }
            | BoundClause::Merge { .. }
    )
}

/// Compiles the ways of a match written several ways, each into a plan
/// of its own over the rows the part in front of it carried.
fn fork_of(
    query: &BoundQuery,
    branches: &[ForkBranch],
    distinct: bool,
    base: Vec<usize>,
    schema: &Schema,
) -> Result<Fork> {
    let mut out = Vec::with_capacity(branches.len());
    for branch in branches {
        let width = branch.slots.len();
        let mut exprs: Vec<BoundExpr> = branch
            .slots
            .iter()
            .map(|slot| BoundExpr::Var(*slot))
            .collect();
        if distinct {
            exprs.extend(elements(&branch.patterns).into_iter().map(BoundExpr::Var));
        }
        let items: Vec<BoundItem> = exprs
            .into_iter()
            .enumerate()
            .map(|(i, expr)| item(expr, i))
            .collect();
        let columns = (0..items.len()).map(|i| format!("#{i}")).collect();
        let clauses = vec![
            BoundClause::Match {
                kind: MatchKind::Required,
                patterns: branch.patterns.clone(),
                filter: branch.filter.clone(),
            },
            BoundClause::Project {
                distinct: false,
                items,
                order_by: Vec::new(),
                order_aggs: Vec::new(),
                skip: None,
                limit: None,
                filter: None,
            },
        ];
        let (query, plan) = compile(query, clauses, columns, Some(base.clone()), schema)?;
        out.push(Branch { query, plan, width });
    }
    Ok(Fork {
        branches: out,
        distinct,
    })
}

/// Every element one way walked, in written order.
///
/// A path is what it went through and not only what it named, so this
/// counts the anonymous elements too: `(a)-[:KNOWS]->(b)` and
/// `(a)-[:WORKS_AT]->(b)` over the same pair are two paths, and under
/// `|` the set is a set of paths.
fn elements(patterns: &[BoundPath]) -> Vec<usize> {
    let mut out = Vec::new();
    for path in patterns {
        out.push(path.start.slot);
        for (rel, node) in &path.steps {
            out.push(rel.slot);
            out.push(node.slot);
        }
    }
    out
}

/// The write a clause [`is_write`] answered for describes.
fn write_of(clause: &BoundClause, schema: &Schema, session_schema: &str) -> Result<Write> {
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
                .map(|nested| nested_query(nested, schema, session_schema))
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
fn nested_query(
    parsed: &zu_query::ast::Query,
    schema: &Schema,
    session_schema: &str,
) -> Result<Subquery> {
    let (query, plan, _) = crate::query::compile_parsed(parsed, schema, session_schema)?;
    if query.clauses.iter().any(is_write) {
        return Err(ZuError::gql(
            codes::C42001,
            "the query inside DELETE VALUE answers the element to delete, so it reads and does not write",
        ));
    }
    // The nested query is run whole where the delete item is read, so
    // there is nowhere to hand the rows of one way across to the next.
    if query.clauses.iter().any(zu_query::binder::forks) {
        return Err(ZuError::gql(
            codes::C42001,
            "the query inside DELETE VALUE answers one element, and a match written several ways is a walk per alternative; write the alternatives as operands of a composite query",
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
        // The statement's binding variables, carried for the same
        // reason its value query expressions are: a part reads the same
        // parameter positions the whole statement does, so each part
        // fills them before it runs. That works out a definition once
        // per part rather than once per statement, which is why a
        // definition that writes is refused: re-reading a graph twice
        // answers the same thing and writing to it twice does not.
        bindings: query.bindings.clone(),
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
