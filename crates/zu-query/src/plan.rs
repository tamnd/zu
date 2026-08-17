//! Logical plan: the graph algebra a bound query compiles into before
//! rewrites, join ordering, and physical planning (docs/07 §2).
//!
//! The builder walks the bound clauses in order and produces a
//! left-deep tree: scans introduce nodes, expands walk rels, inline
//! property predicates filter at the earliest point their slot exists,
//! and projections split into Project or Aggregate by whether any item
//! aggregates. Join ordering rewrites this tree in a later pass; the
//! shape here is the written order, which is also what EXPLAIN prints
//! before the optimizer exists.

use std::collections::HashSet;
use std::fmt::Write as _;

use zu_common::Result;

use crate::ast::{BinaryOp, Literal, PathMode, RelDirection, Selector, SortKey, UnaryOp};
use crate::binder::{
    BoundClause, BoundExpr, BoundInsertNode, BoundInsertRel, BoundItem, BoundQuery, BoundSetItem,
    Func, MatchKind, Schema, TableFunc,
};

/// What a bracket does with an outer row the operators inside it
/// found nothing for, and which bracket in the query this is.
///
/// The operators of one bracket share the id and nothing else does, so
/// a run of them is found by looking at the ids alone. The kinds are
/// the things a match can be other than plain: OPTIONAL MATCH, the two
/// halves of an existence predicate, and the mark an existence
/// predicate becomes where it has to answer with a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bracket {
    pub id: usize,
    pub kind: BracketKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BracketKind {
    /// The outer row survives a miss, the bracket's slots bound null,
    /// and a hit is handed up as many times as it matched.
    Optional,
    /// The outer row survives a hit, once, however many there were,
    /// and the bracket's slots are never read above it.
    Semi,
    /// The outer row survives a miss, and a hit ends it.
    Anti,
    /// The outer row survives either way, once, carrying whether there
    /// was a hit in the slot named here. That is what a block written
    /// under an OR asks for: the other side of the OR may still keep a
    /// row this one found nothing for, so the answer is a value the
    /// predicate reads rather than a decision about the row. `negated`
    /// is a NOT in front of the block, folded into what gets written
    /// down the way an anti bracket folds one into what it keeps.
    Mark { slot: usize, negated: bool },
}

impl Bracket {
    /// True where the bracket keeps a row it found nothing for, which
    /// is where a null can reach the operators above.
    pub fn nulls_on_miss(&self) -> bool {
        matches!(self.kind, BracketKind::Optional)
    }

    /// What the plan text writes in front of a bracketed operator.
    pub fn prefix(&self) -> &'static str {
        match self.kind {
            BracketKind::Optional => "Optional",
            BracketKind::Semi => "Semi",
            BracketKind::Anti => "Anti",
            BracketKind::Mark { .. } => "Mark",
        }
    }
}

/// The hop range of a variable-length expand together with the path
/// mode and selector that govern its enumeration (docs/07 §1, §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarLength {
    pub min: Option<u64>,
    pub max: Option<u64>,
    pub mode: PathMode,
    pub selector: Option<Selector>,
}

/// One operator tree. Children are boxed inputs; `Empty` is the single
/// starting row.
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// One row, no columns: the seed under the first scan or unwind.
    Empty,
    /// Rows that are already values, read from the argument at `arg`
    /// as a list of lists, one column per slot in `slots`.
    ///
    /// This is what a statement that wrote something runs its later
    /// clauses over. The write reads the rows its own clauses answered
    /// and then writes, so the clauses after it cannot read through
    /// the same operators again: those operators would answer over the
    /// store the write just changed and a row would meet what it
    /// created. So the rows are carried across the write as values,
    /// and this is the operator that reads them back in.
    Rows {
        slots: Vec<usize>,
        arg: usize,
    },
    /// Introduces every row of a node slot's candidate tables.
    /// `bracket` carries the group this operator belongs to, `None`
    /// for a plain match. Operators and filters sharing a group run as
    /// one unit against each outer row, and the kind says what becomes
    /// of an outer row the unit found nothing for.
    ScanNodes {
        input: Box<LogicalPlan>,
        slot: usize,
        bracket: Option<Bracket>,
    },
    /// Walks a rel from a bound slot, introducing (or joining into)
    /// the far node.
    Expand {
        input: Box<LogicalPlan>,
        rel: usize,
        from: usize,
        to: usize,
        direction: RelDirection,
        range: Option<VarLength>,
        /// True when the far node was already bound: the expand joins
        /// into it instead of introducing it.
        into: bool,
        /// Set by the optimizer on closing expands whose estimated
        /// probe count exceeds the rel's edge count: the executor
        /// accumulates the edge set once and probes a hash table
        /// instead of storage (docs/07, ASPJoin).
        asp: bool,
        /// Set by the optimizer on closing expands eligible for the
        /// multiway intersection (docs/07 §4). A closing expand is a
        /// cycle close by construction, both endpoints already bound,
        /// which is exactly where the WCOJ pays off. The physical
        /// compiler fuses the close with the expand that feeds it when
        /// the two are adjacent; when they are not, `asp` still
        /// decides the binary fallback.
        wcoj: bool,
        bracket: Option<Bracket>,
    },
    /// `bracket` ties a filter into its group: inline props and the
    /// clause's WHERE gate matches inside the group rather than
    /// dropping the row the group hands up for a miss.
    Filter {
        input: Box<LogicalPlan>,
        expr: BoundExpr,
        bracket: Option<Bracket>,
    },
    Unwind {
        input: Box<LogicalPlan>,
        expr: BoundExpr,
        slot: usize,
    },
    /// Creates the elements an `INSERT` wrote, binding each one to its
    /// slot, one row out for every row in.
    ///
    /// This is the shape of the statement rather than something the
    /// executor runs. The session splits a statement at its write: the
    /// operators under this one answer the rows the write runs for, the
    /// write runs once for each of them, and the operators above run
    /// over [`LogicalPlan::Rows`] holding what came of it. That is a
    /// seam and not a design: it is here because the executor reads
    /// through a graph and a graph reads. When the read path consults
    /// overlays and the executor can stage a change of its own, the
    /// staging moves in here and the split goes away, and the plan a
    /// query compiles to does not change.
    Insert {
        input: Box<LogicalPlan>,
        nodes: Vec<BoundInsertNode>,
        rels: Vec<BoundInsertRel>,
    },
    /// Changes what the elements a row already holds hold, one row at a
    /// time, and hands the row on unchanged.
    ///
    /// This is the shape of the statement the same way
    /// [`LogicalPlan::Insert`] is, and the session runs it the same
    /// way: the operators under it answer the rows the write runs for,
    /// the write runs once for each of them, and the operators above it
    /// read the rows back. A row out is the row in, because a statement
    /// that changes an element binds nothing new.
    Set {
        input: Box<LogicalPlan>,
        items: Vec<BoundSetItem>,
    },
    /// Takes the elements a row holds out of the graph, one row at a
    /// time, and hands the row on unchanged.
    ///
    /// The same shape as [`LogicalPlan::Set`], for the same reason: a
    /// delete binds nothing, so a row out is the row in, and the slots
    /// it names stay named. What the reader does with them afterwards
    /// is the reader's business, and reading a deleted element is
    /// 22G11.
    Delete {
        input: Box<LogicalPlan>,
        slots: Vec<usize>,
        /// Whether the edges on the elements go with them, which is the
        /// `DETACH` the statement was written with.
        detach: bool,
    },
    /// A table function source: the engine kernel runs once over `rel`
    /// and yields one row per node of its domain, node slot first.
    TableFunction {
        input: Box<LogicalPlan>,
        func: TableFunc,
        rel: u32,
        table: u32,
        args: Vec<BoundExpr>,
        slots: Vec<usize>,
    },
    Project {
        input: Box<LogicalPlan>,
        items: Vec<BoundItem>,
    },
    /// Grouped aggregation: non-aggregate items are the keys.
    Aggregate {
        input: Box<LogicalPlan>,
        keys: Vec<BoundItem>,
        aggs: Vec<BoundItem>,
    },
    Distinct {
        input: Box<LogicalPlan>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<SortKey<BoundExpr>>,
    },
    Skip {
        input: Box<LogicalPlan>,
        expr: BoundExpr,
    },
    Limit {
        input: Box<LogicalPlan>,
        expr: BoundExpr,
    },
}

impl LogicalPlan {
    fn boxed(self) -> Box<LogicalPlan> {
        Box::new(self)
    }
}

/// Builds the logical plan for a bound query.
pub fn build(query: &BoundQuery) -> Result<LogicalPlan> {
    build_over(query, LogicalPlan::Empty)
}

/// Builds the plan for a bound query over something other than the one
/// empty row, which is what the clauses after a write run over: the
/// rows the write carried across it, already bound to their slots.
pub fn build_over(query: &BoundQuery, base: LogicalPlan) -> Result<LogicalPlan> {
    // Slots a plan operator has already introduced.
    let mut bound: HashSet<usize> = HashSet::new();
    if let LogicalPlan::Rows { slots, .. } = &base {
        bound.extend(slots.iter().copied());
    }
    let mut plan = base;
    // Every bracketed clause is a group of its own, counted in written
    // order.
    let mut groups = 0;
    for clause in &query.clauses {
        match clause {
            BoundClause::Match {
                kind,
                patterns,
                filter,
            } => {
                let kind = match kind {
                    MatchKind::Required => None,
                    MatchKind::Optional => Some(BracketKind::Optional),
                    MatchKind::Semi => Some(BracketKind::Semi),
                    MatchKind::Anti => Some(BracketKind::Anti),
                    MatchKind::Mark { slot, negated } => Some(BracketKind::Mark {
                        slot: *slot,
                        negated: *negated,
                    }),
                };
                let group = kind.map(|kind| {
                    groups += 1;
                    Bracket {
                        id: groups - 1,
                        kind,
                    }
                });
                for path in patterns {
                    plan = build_path(plan, path, &mut bound, group)?;
                }
                if let Some(expr) = filter {
                    plan = LogicalPlan::Filter {
                        input: plan.boxed(),
                        expr: expr.clone(),
                        bracket: group,
                    };
                }
            }
            BoundClause::Insert { nodes, rels, .. } => {
                for node in nodes {
                    bound.insert(node.slot);
                }
                for rel in rels {
                    bound.insert(rel.slot);
                }
                plan = LogicalPlan::Insert {
                    input: plan.boxed(),
                    nodes: nodes.clone(),
                    rels: rels.clone(),
                };
            }
            BoundClause::Set { items, .. } => {
                plan = LogicalPlan::Set {
                    input: plan.boxed(),
                    items: items.clone(),
                };
            }
            BoundClause::Delete { slots, detach, .. } => {
                plan = LogicalPlan::Delete {
                    input: plan.boxed(),
                    slots: slots.clone(),
                    detach: *detach,
                };
            }
            BoundClause::Unwind { expr, slot } => {
                bound.insert(*slot);
                plan = LogicalPlan::Unwind {
                    input: plan.boxed(),
                    expr: expr.clone(),
                    slot: *slot,
                };
            }
            BoundClause::Call {
                func,
                rel,
                table,
                args,
                slots,
            } => {
                bound.extend(slots.iter().copied());
                plan = LogicalPlan::TableFunction {
                    input: plan.boxed(),
                    func: *func,
                    rel: *rel,
                    table: *table,
                    args: args.clone(),
                    slots: slots.clone(),
                };
            }
            BoundClause::Project {
                distinct,
                items,
                order_by,
                skip,
                limit,
                filter,
            } => {
                let (aggs, keys): (Vec<_>, Vec<_>) =
                    items.iter().cloned().partition(|i| i.aggregate);
                plan = if aggs.is_empty() {
                    LogicalPlan::Project {
                        input: plan.boxed(),
                        items: items.clone(),
                    }
                } else {
                    LogicalPlan::Aggregate {
                        input: plan.boxed(),
                        keys,
                        aggs,
                    }
                };
                for item in items {
                    if let Some(slot) = item.slot {
                        bound.insert(slot);
                    }
                }
                if *distinct {
                    plan = LogicalPlan::Distinct {
                        input: plan.boxed(),
                    };
                }
                if let Some(expr) = filter {
                    plan = LogicalPlan::Filter {
                        input: plan.boxed(),
                        expr: expr.clone(),
                        bracket: None,
                    };
                }
                if !order_by.is_empty() {
                    plan = LogicalPlan::Sort {
                        input: plan.boxed(),
                        keys: order_by.clone(),
                    };
                }
                if let Some(expr) = skip {
                    plan = LogicalPlan::Skip {
                        input: plan.boxed(),
                        expr: expr.clone(),
                    };
                }
                if let Some(expr) = limit {
                    plan = LogicalPlan::Limit {
                        input: plan.boxed(),
                        expr: expr.clone(),
                    };
                }
            }
        }
    }
    Ok(plan)
}

fn build_path(
    mut plan: LogicalPlan,
    path: &crate::binder::BoundPath,
    bound: &mut HashSet<usize>,
    bracket: Option<Bracket>,
) -> Result<LogicalPlan> {
    // A path variable adds no operator: the binder records its shape
    // and the executor assembles the value from the pattern's slots.
    if !bound.contains(&path.start.slot) {
        bound.insert(path.start.slot);
        plan = LogicalPlan::ScanNodes {
            input: plan.boxed(),
            slot: path.start.slot,
            bracket,
        };
    }
    plan = label_filter(plan, &path.start, bracket);
    plan = prop_filters(plan, path.start.slot, &path.start.props, bracket);
    let mut from = path.start.slot;
    for (rel, node) in &path.steps {
        let into = bound.contains(&node.slot);
        bound.insert(node.slot);
        bound.insert(rel.slot);
        plan = LogicalPlan::Expand {
            input: plan.boxed(),
            rel: rel.slot,
            from,
            to: node.slot,
            direction: rel.direction,
            range: rel.range.map(|(min, max)| VarLength {
                min,
                max,
                mode: rel.mode,
                selector: rel.selector,
            }),
            into,
            asp: false,
            wcoj: false,
            bracket,
        };
        plan = prop_filters(plan, rel.slot, &rel.props, bracket);
        plan = label_filter(plan, node, bracket);
        plan = prop_filters(plan, node.slot, &node.props, bracket);
        from = node.slot;
    }
    Ok(plan)
}

/// The bitset test a label set leaves behind, right where the slot
/// appears. Most patterns leave none: the binder drops the tables that
/// cannot satisfy the labels and the tables that are left carry them on
/// every row, so this plants an operator only for a label some rows of
/// a candidate table hold and others do not.
fn label_filter(
    plan: LogicalPlan,
    node: &crate::binder::BoundNode,
    bracket: Option<Bracket>,
) -> LogicalPlan {
    match &node.label {
        None => plan,
        Some(test) => LogicalPlan::Filter {
            input: plan.boxed(),
            expr: BoundExpr::HasLabels {
                slot: node.slot,
                test: test.clone(),
            },
            bracket,
        },
    }
}

/// Inline `{key: value}` predicates become equality filters right where
/// the slot appears; this is the pushdown the props form guarantees.
/// Inside an OPTIONAL MATCH they carry the group so they gate matches
/// within the group instead of dropping its null row.
fn prop_filters(
    mut plan: LogicalPlan,
    slot: usize,
    props: &[(String, BoundExpr)],
    bracket: Option<Bracket>,
) -> LogicalPlan {
    for (key, value) in props {
        let expr = BoundExpr::Binary {
            op: BinaryOp::Eq,
            lhs: Box::new(BoundExpr::Property {
                base: Box::new(BoundExpr::Var(slot)),
                key: key.clone(),
            }),
            rhs: Box::new(value.clone()),
        };
        plan = LogicalPlan::Filter {
            input: plan.boxed(),
            expr,
            bracket,
        };
    }
    plan
}

/// Renders a plan as the indented tree EXPLAIN prints, top operator
/// first, one child per line under it.
pub fn explain(plan: &LogicalPlan, query: &BoundQuery, schema: &Schema) -> String {
    let mut out = String::new();
    render(plan, query, schema, 0, &mut out);
    out
}

fn slot_name(query: &BoundQuery, slot: usize) -> &str {
    &query.variables[slot].name
}

fn node_tables(query: &BoundQuery, schema: &Schema, slot: usize) -> String {
    let names: Vec<&str> = query.variables[slot]
        .node_tables
        .iter()
        .filter_map(|id| schema.node_by_id(*id).map(|n| n.name.as_str()))
        .collect();
    names.join("|")
}

fn rel_tables(query: &BoundQuery, schema: &Schema, slot: usize) -> String {
    let names: Vec<&str> = query.variables[slot]
        .rel_tables
        .iter()
        .filter_map(|id| schema.rel_by_id(*id).map(|r| r.name.as_str()))
        .collect();
    names.join("|")
}

fn render(plan: &LogicalPlan, query: &BoundQuery, schema: &Schema, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    match plan {
        LogicalPlan::Empty => {}
        LogicalPlan::Rows { slots, .. } => {
            let names: Vec<&str> = slots.iter().map(|slot| slot_name(query, *slot)).collect();
            let _ = writeln!(out, "{pad}Rows {}", names.join(", "));
        }
        LogicalPlan::ScanNodes {
            input,
            slot,
            bracket,
        } => {
            let opt = bracket.map_or("", |b| b.prefix());
            let _ = writeln!(
                out,
                "{pad}{opt}ScanNodes {}: {}",
                slot_name(query, *slot),
                node_tables(query, schema, *slot)
            );
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Expand {
            input,
            rel,
            from,
            to,
            direction,
            range,
            into,
            asp,
            wcoj: _,
            bracket,
        } => {
            let hops = match range {
                None => String::new(),
                Some(v) => {
                    let min = v.min.map_or(String::new(), |v| v.to_string());
                    let max = v.max.map_or(String::new(), |v| v.to_string());
                    let mut s = format!("*{min}..{max}");
                    match v.mode {
                        PathMode::Walk => s.push_str(" walk"),
                        PathMode::Trail => {}
                        PathMode::Acyclic => s.push_str(" acyclic"),
                    }
                    match v.selector {
                        Some(Selector::AnyShortest) => s.push_str(" any shortest"),
                        Some(Selector::AllShortest) => s.push_str(" all shortest"),
                        None => {}
                    }
                    s
                }
            };
            let (open, close) = direction.spelling();
            let opt = bracket.map_or("", |b| b.prefix());
            let kind = if *asp {
                "AspJoin"
            } else if *into {
                "ExpandInto"
            } else {
                "Expand"
            };
            let _ = writeln!(
                out,
                "{pad}{opt}{kind} ({}){open}[{}:{}{hops}]{close}({})",
                slot_name(query, *from),
                slot_name(query, *rel),
                rel_tables(query, schema, *rel),
                slot_name(query, *to),
            );
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Filter {
            input,
            expr,
            bracket,
        } => {
            let opt = bracket.map_or("", |b| b.prefix());
            let _ = writeln!(out, "{pad}{opt}Filter {}", expr_text(expr, query));
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Unwind { input, expr, slot } => {
            let _ = writeln!(
                out,
                "{pad}Unwind {} AS {}",
                expr_text(expr, query),
                slot_name(query, *slot)
            );
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Insert { input, nodes, rels } => {
            let mut written: Vec<String> = nodes
                .iter()
                .map(|node| {
                    let table = schema
                        .node_by_id(node.table)
                        .map(|n| n.name.as_str())
                        .unwrap_or("?");
                    let props: Vec<String> = node
                        .props
                        .iter()
                        .map(|(key, expr)| format!("{key}: {}", expr_text(expr, query)))
                        .collect();
                    match props.is_empty() {
                        true => format!("({}:{table})", slot_name(query, node.slot)),
                        false => format!(
                            "({}:{table} {{{}}})",
                            slot_name(query, node.slot),
                            props.join(", ")
                        ),
                    }
                })
                .collect();
            // An edge prints as the step it was written as, with the
            // ends named rather than repeated in full, since the
            // elements themselves are listed just above.
            written.extend(rels.iter().map(|rel| {
                let table = schema
                    .rel_by_id(rel.table)
                    .map(|r| r.name.as_str())
                    .unwrap_or("?");
                format!(
                    "({})-[{}:{table}]->({})",
                    slot_name(query, rel.src),
                    slot_name(query, rel.slot),
                    slot_name(query, rel.dst)
                )
            }));
            let _ = writeln!(out, "{pad}Insert {}", written.join(", "));
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Set { input, items } => {
            let written: Vec<String> = items
                .iter()
                .map(|item| {
                    format!(
                        "{}.{} = {}",
                        slot_name(query, item.target),
                        item.key,
                        expr_text(&item.value, query)
                    )
                })
                .collect();
            let _ = writeln!(out, "{pad}Set {}", written.join(", "));
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Delete {
            input,
            slots,
            detach,
        } => {
            let taken: Vec<&str> = slots.iter().map(|&s| slot_name(query, s)).collect();
            let word = if *detach { "DetachDelete" } else { "Delete" };
            let _ = writeln!(out, "{pad}{word} {}", taken.join(", "));
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::TableFunction {
            input,
            func,
            rel,
            slots,
            ..
        } => {
            let rel_name = schema
                .rel_by_id(*rel)
                .map(|r| r.name.as_str())
                .unwrap_or("?");
            let cols: Vec<&str> = slots.iter().map(|&s| slot_name(query, s)).collect();
            let _ = writeln!(
                out,
                "{pad}Call {}({rel_name}) YIELD {}",
                func.name(),
                cols.join(", ")
            );
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Project { input, items } => {
            let rendered: Vec<String> = items.iter().map(|i| item_text(i, query)).collect();
            let _ = writeln!(out, "{pad}Project {}", rendered.join(", "));
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Aggregate { input, keys, aggs } => {
            let rendered: Vec<String> = aggs.iter().map(|i| item_text(i, query)).collect();
            let grouped: Vec<String> = keys.iter().map(|i| item_text(i, query)).collect();
            if grouped.is_empty() {
                let _ = writeln!(out, "{pad}Aggregate {}", rendered.join(", "));
            } else {
                let _ = writeln!(
                    out,
                    "{pad}Aggregate {} BY {}",
                    rendered.join(", "),
                    grouped.join(", ")
                );
            }
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Distinct { input } => {
            let _ = writeln!(out, "{pad}Distinct");
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Sort { input, keys } => {
            let rendered: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{} {}{}",
                        expr_text(&key.expr, query),
                        if key.ascending { "ASC" } else { "DESC" },
                        if key.nulls_first() {
                            " NULLS FIRST"
                        } else {
                            ""
                        }
                    )
                })
                .collect();
            let _ = writeln!(out, "{pad}Sort {}", rendered.join(", "));
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Skip { input, expr } => {
            let _ = writeln!(out, "{pad}Skip {}", expr_text(expr, query));
            render(input, query, schema, depth + 1, out);
        }
        LogicalPlan::Limit { input, expr } => {
            let _ = writeln!(out, "{pad}Limit {}", expr_text(expr, query));
            render(input, query, schema, depth + 1, out);
        }
    }
}

fn item_text(item: &BoundItem, query: &BoundQuery) -> String {
    let rendered = expr_text(&item.expr, query);
    if rendered == item.name {
        rendered
    } else {
        format!("{} AS {}", rendered, item.name)
    }
}

/// Renders a bound expression back to query-shaped text for EXPLAIN and
/// filter display.
pub fn expr_text(expr: &BoundExpr, query: &BoundQuery) -> String {
    match expr {
        BoundExpr::Literal(Literal::Null) => "NULL".into(),
        BoundExpr::Literal(Literal::Bool(b)) => if *b { "TRUE" } else { "FALSE" }.into(),
        BoundExpr::Literal(Literal::Int(v)) => v.to_string(),
        BoundExpr::Literal(Literal::Float(v)) => v.to_string(),
        BoundExpr::Literal(Literal::Str(s)) => format!("'{s}'"),
        BoundExpr::Literal(Literal::Temporal(t)) => t.to_string(),
        BoundExpr::Param(ix) => format!("${}", query.params[*ix]),
        BoundExpr::Var(slot) => slot_name(query, *slot).to_string(),
        BoundExpr::HasLabels { slot, test } => {
            format!("{}:{}", slot_name(query, *slot), test.text(&query.labels))
        }
        BoundExpr::Property { base, key } => format!("{}.{key}", expr_text(base, query)),
        BoundExpr::Unary { op, expr } => match op {
            UnaryOp::Not => format!("NOT {}", expr_text(expr, query)),
            UnaryOp::Neg => format!("-{}", expr_text(expr, query)),
        },
        BoundExpr::Binary { op, lhs, rhs } => {
            let symbol = match op {
                BinaryOp::Or => "OR",
                BinaryOp::Xor => "XOR",
                BinaryOp::And => "AND",
                BinaryOp::Eq => "=",
                BinaryOp::Ne => "<>",
                BinaryOp::Lt => "<",
                BinaryOp::Le => "<=",
                BinaryOp::Gt => ">",
                BinaryOp::Ge => ">=",
                BinaryOp::Add => "+",
                BinaryOp::Sub => "-",
                BinaryOp::Mul => "*",
                BinaryOp::Div => "/",
                BinaryOp::Mod => "%",
                BinaryOp::In => "IN",
                BinaryOp::StartsWith => "STARTS WITH",
                BinaryOp::EndsWith => "ENDS WITH",
                BinaryOp::Contains => "CONTAINS",
            };
            format!(
                "{} {symbol} {}",
                expr_text(lhs, query),
                expr_text(rhs, query)
            )
        }
        BoundExpr::IsNull { expr, negated } => {
            if *negated {
                format!("{} IS NOT NULL", expr_text(expr, query))
            } else {
                format!("{} IS NULL", expr_text(expr, query))
            }
        }
        BoundExpr::IsTyped { expr, ty, negated } => {
            let not = if *negated { "NOT " } else { "" };
            format!("{} IS {not}TYPED {ty}", expr_text(expr, query))
        }
        BoundExpr::Call {
            func,
            distinct,
            star,
            args,
        } => {
            let name = match func {
                Func::Count => "count",
                Func::Sum => "sum",
                Func::Avg => "avg",
                Func::Min => "min",
                Func::Max => "max",
                Func::Collect => "collect",
                Func::Id => "id",
                Func::Size => "size",
                Func::Cardinality => "cardinality",
                Func::PathLength => "path_length",
            };
            let inner = if *star {
                "*".to_string()
            } else {
                let rendered: Vec<String> = args.iter().map(|a| expr_text(a, query)).collect();
                rendered.join(", ")
            };
            if *distinct {
                format!("{name}(DISTINCT {inner})")
            } else {
                format!("{name}({inner})")
            }
        }
        BoundExpr::List(items) => {
            let rendered: Vec<String> = items.iter().map(|i| expr_text(i, query)).collect();
            format!("[{}]", rendered.join(", "))
        }
        BoundExpr::Map(entries) => {
            let rendered: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{k}: {}", expr_text(v, query)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
        BoundExpr::Path(elements) => {
            let rendered: Vec<String> = elements.iter().map(|e| expr_text(e, query)).collect();
            format!("PATH [{}]", rendered.join(", "))
        }
        BoundExpr::Cast { expr, ty } => format!("CAST({} AS {ty})", expr_text(expr, query)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{self, NodeDef, RelDef};
    use crate::parser::parse;

    fn schema() -> Schema {
        Schema::new(
            vec![
                NodeDef {
                    id: 0,
                    name: "Person".into(),
                    node_count: 9000,
                    labels: Vec::new(),
                },
                NodeDef {
                    id: 1,
                    name: "Place".into(),
                    node_count: 1400,
                    labels: Vec::new(),
                },
            ],
            vec![
                RelDef {
                    id: 2,
                    name: "KNOWS".into(),
                    from: 0,
                    to: 0,
                    edge_count: 180_000,
                    undirected: false,
                },
                RelDef {
                    id: 3,
                    name: "IS_LOCATED_IN".into(),
                    from: 0,
                    to: 1,
                    edge_count: 9000,
                    undirected: false,
                },
            ],
        )
        .expect("schema")
    }

    fn planned(source: &str) -> (LogicalPlan, BoundQuery) {
        let query = binder::bind(&parse(source).expect("parse"), &schema()).expect("bind");
        let plan = build(&query).expect("plan");
        (plan, query)
    }

    fn explained(source: &str) -> String {
        let (plan, query) = planned(source);
        explain(&plan, &query, &schema())
    }

    #[test]
    fn point_lookup_plans_scan_filter_expand() {
        let text = explained(
            "MATCH (a:Person {id: $src})-[:KNOWS]->(b) \
             RETURN b.id AS friend ORDER BY friend LIMIT 10",
        );
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            [
                "Limit 10",
                "Sort friend ASC",
                "Project b.id AS friend",
                "Expand (a)-[#1:KNOWS]->(b)",
                "Filter a.id = $src",
                "ScanNodes a: Person",
            ],
            "got:\n{text}"
        );
    }

    /// An insert prints what it writes, so that a plan says the
    /// statement changes something rather than looking like a read of
    /// nowhere.
    #[test]
    fn an_insert_prints_the_elements_it_writes() {
        let text = explained("INSERT (x:Person {name: $who}), (y:Person) RETURN x.id AS id");
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            [
                "Project x.id AS id",
                "Insert (x:Person {name: $who}), (y:Person)",
            ],
            "got:\n{text}"
        );

        // An edge prints as the step it was written as, pointing the
        // way it is stored whichever way round it was written.
        let text = explained("INSERT (x:Person)<-[k:KNOWS]-(y:Person) RETURN k");
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            [
                "Project k",
                "Insert (x:Person), (y:Person), (y)-[k:KNOWS]->(x)",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn shared_variables_expand_into_a_bound_slot() {
        // The triangle closes with an ExpandInto on the reused node.
        let text = explained(
            "MATCH (a:Person)-[r1:KNOWS]->(b)-[r2:KNOWS]->(c), (a)-[r3:KNOWS]->(c) RETURN a",
        );
        assert!(
            text.contains("ExpandInto (a)-[r3:KNOWS]->(c)"),
            "got:\n{text}"
        );
        // Only one scan: every later node arrives by expansion.
        assert_eq!(text.matches("ScanNodes").count(), 1, "got:\n{text}");
    }

    #[test]
    fn aggregation_splits_keys_from_aggregates() {
        let text = explained(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS friends \
             WHERE friends > 5 RETURN a.id AS id, friends",
        );
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            [
                "Project a.id AS id, friends",
                "Filter friends > 5",
                "Aggregate count(b) AS friends BY a",
                "Expand (a)-[#1:KNOWS]->(b)",
                "ScanNodes a: Person",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn var_length_and_direction_render() {
        let text = explained("MATCH (a:Person)<-[:KNOWS*1..3]-(b) RETURN b LIMIT 1");
        assert!(
            text.contains("Expand (a)<-[#1:KNOWS*1..3]-(b)"),
            "got:\n{text}"
        );
        let text = explained("MATCH ACYCLIC (a:Person)-[:KNOWS*1..3]->(b) RETURN b LIMIT 1");
        assert!(
            text.contains("Expand (a)-[#1:KNOWS*1..3 acyclic]->(b)"),
            "got:\n{text}"
        );
        let text = explained("MATCH ANY SHORTEST (a:Person)-[:KNOWS*]->(b) RETURN b LIMIT 1");
        assert!(
            text.contains("Expand (a)-[#1:KNOWS*.. any shortest]->(b)"),
            "got:\n{text}"
        );
        let text = explained("MATCH (a:Person)--(b:Person) RETURN a");
        assert!(
            text.contains("]-(b)") && !text.contains("]->(b)"),
            "got:\n{text}"
        );
    }

    #[test]
    fn unwind_distinct_and_props_on_rels() {
        let text = explained(
            "UNWIND [1, 2] AS x MATCH (a:Person)-[r:KNOWS {since: x}]->(b) \
             RETURN DISTINCT a.id AS id",
        );
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            [
                "Distinct",
                "Project a.id AS id",
                "Filter r.since = x",
                "Expand (a)-[r:KNOWS]->(b)",
                "ScanNodes a: Person",
                "Unwind [1, 2] AS x",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn optional_match_marks_its_operators() {
        let text =
            explained("MATCH (a:Person) OPTIONAL MATCH (a)-[:IS_LOCATED_IN]->(p) RETURN a, p");
        assert!(text.contains("OptionalExpand (a)-"), "got:\n{text}");
        assert!(
            !text.contains("OptionalScanNodes"),
            "a was already bound:\n{text}"
        );
    }

    #[test]
    fn existence_blocks_mark_their_operators() {
        let text = explained(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 } \
             AND NOT EXISTS { MATCH (a)-[:IS_LOCATED_IN]->(p) } RETURN a",
        );
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            [
                "Project a",
                "AntiExpand (a)-[#3:IS_LOCATED_IN]->(p)",
                "SemiFilter b.id > 3",
                "SemiExpand (a)-[#1:KNOWS]->(b)",
                "ScanNodes a: Person",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn disconnected_paths_scan_separately() {
        let text = explained("MATCH (a:Person), (b:Place) RETURN a, b");
        assert_eq!(text.matches("ScanNodes").count(), 2, "got:\n{text}");
    }

    #[test]
    fn unlabeled_scan_lists_every_candidate() {
        let text = explained("MATCH (n) RETURN n LIMIT 1");
        assert!(text.contains("ScanNodes n: Person|Place"), "got:\n{text}");
    }

    /// The graph type is what decides whether a label costs anything:
    /// a label every row of the scanned table carries is a fact about
    /// the table, and the plan holds no operator for it.
    #[test]
    fn a_declared_label_filters_and_a_table_label_does_not() {
        let schema = Schema::with_labels(
            vec![NodeDef {
                id: 0,
                name: "Person".into(),
                node_count: 9000,
                labels: vec![0, 1, 2],
            }],
            Vec::new(),
            vec!["Person".into(), "Employee".into(), "Manager".into()],
        )
        .expect("schema");
        let explained = |source: &str| {
            let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
            let plan = build(&query).expect("plan");
            explain(&plan, &query, &schema)
        };
        let text = explained("MATCH (n:Person) RETURN n");
        assert!(!text.contains("Filter"), "got:\n{text}");
        let text = explained("MATCH (n:Person:Employee) RETURN n");
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        assert_eq!(
            lines,
            ["Project n", "Filter n:Employee", "ScanNodes n: Person"],
            "got:\n{text}"
        );
        // An expression reads back the way it was written, with the
        // parentheses the precedence needs and no others.
        let filter = |source: &str| {
            explained(source)
                .lines()
                .find(|l| l.trim_start().starts_with("Filter"))
                .map(|l| l.trim_start().to_string())
                .expect("a filter")
        };
        assert_eq!(filter("MATCH (n:!Employee) RETURN n"), "Filter n:!Employee");
        assert_eq!(
            filter("MATCH (n:!(Employee&Manager)) RETURN n"),
            "Filter n:!(Employee&Manager)"
        );
        assert_eq!(
            filter("MATCH (n:Employee|Manager&Person) RETURN n"),
            "Filter n:Employee|Manager"
        );
        // The label every row carries comes out of the test wherever
        // it sits, so this asks about Employee and nothing else.
        assert_eq!(
            filter("MATCH (n:!(Employee&Person)) RETURN n"),
            "Filter n:!Employee"
        );
    }

    #[test]
    fn path_variables_add_no_operators() {
        let text = explained("MATCH p = (a:Person)-[:KNOWS]->(b) RETURN p");
        assert!(text.contains("Expand"), "got:\n{text}");
        assert!(
            !text.contains("Path"),
            "a path variable assembles at eval time, got:\n{text}"
        );
    }
}
