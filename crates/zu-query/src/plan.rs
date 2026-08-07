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

use zu_common::{Result, ZuError};

use crate::ast::{BinaryOp, Literal, RelDirection, UnaryOp};
use crate::binder::{BoundClause, BoundExpr, BoundItem, BoundQuery, Func, Schema};

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// One operator tree. Children are boxed inputs; `Empty` is the single
/// starting row.
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// One row, no columns: the seed under the first scan or unwind.
    Empty,
    /// Introduces every row of a node slot's candidate tables.
    /// `optional` carries the OPTIONAL MATCH group this operator
    /// belongs to, `None` for a required match. Operators and filters
    /// sharing a group form one left-outer unit at execution: when the
    /// group produces nothing for an outer row, its slots bind null.
    ScanNodes {
        input: Box<LogicalPlan>,
        slot: usize,
        optional: Option<usize>,
    },
    /// Walks a rel from a bound slot, introducing (or joining into)
    /// the far node.
    Expand {
        input: Box<LogicalPlan>,
        rel: usize,
        from: usize,
        to: usize,
        direction: RelDirection,
        range: Option<(Option<u64>, Option<u64>)>,
        /// True when the far node was already bound: the expand joins
        /// into it instead of introducing it.
        into: bool,
        optional: Option<usize>,
    },
    /// `optional` ties a filter into its OPTIONAL MATCH group: inline
    /// props and the clause's WHERE gate matches inside the group
    /// rather than dropping the null row the group emits on a miss.
    Filter {
        input: Box<LogicalPlan>,
        expr: BoundExpr,
        optional: Option<usize>,
    },
    Unwind {
        input: Box<LogicalPlan>,
        expr: BoundExpr,
        slot: usize,
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
        keys: Vec<(BoundExpr, bool)>,
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
    let mut plan = LogicalPlan::Empty;
    // Slots a plan operator has already introduced.
    let mut bound: HashSet<usize> = HashSet::new();
    // Each OPTIONAL MATCH clause is its own left-outer group.
    let mut opt_groups = 0;
    for clause in &query.clauses {
        match clause {
            BoundClause::Match {
                optional,
                patterns,
                filter,
            } => {
                let group = optional.then(|| {
                    opt_groups += 1;
                    opt_groups - 1
                });
                for path in patterns {
                    plan = build_path(plan, path, &mut bound, group)?;
                }
                if let Some(expr) = filter {
                    plan = LogicalPlan::Filter {
                        input: plan.boxed(),
                        expr: expr.clone(),
                        optional: group,
                    };
                }
            }
            BoundClause::Unwind { expr, slot } => {
                bound.insert(*slot);
                plan = LogicalPlan::Unwind {
                    input: plan.boxed(),
                    expr: expr.clone(),
                    slot: *slot,
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
                        optional: None,
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
    optional: Option<usize>,
) -> Result<LogicalPlan> {
    if path.slot.is_some() {
        return Err(invalid(
            "path variables are not planned yet, they arrive with path returns".into(),
        ));
    }
    if !bound.contains(&path.start.slot) {
        bound.insert(path.start.slot);
        plan = LogicalPlan::ScanNodes {
            input: plan.boxed(),
            slot: path.start.slot,
            optional,
        };
    }
    plan = prop_filters(plan, path.start.slot, &path.start.props, optional);
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
            range: rel.range,
            into,
            optional,
        };
        plan = prop_filters(plan, rel.slot, &rel.props, optional);
        plan = prop_filters(plan, node.slot, &node.props, optional);
        from = node.slot;
    }
    Ok(plan)
}

/// Inline `{key: value}` predicates become equality filters right where
/// the slot appears; this is the pushdown the props form guarantees.
/// Inside an OPTIONAL MATCH they carry the group so they gate matches
/// within the group instead of dropping its null row.
fn prop_filters(
    mut plan: LogicalPlan,
    slot: usize,
    props: &[(String, BoundExpr)],
    optional: Option<usize>,
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
            optional,
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
        LogicalPlan::ScanNodes {
            input,
            slot,
            optional,
        } => {
            let opt = if optional.is_some() { "Optional" } else { "" };
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
            optional,
        } => {
            let hops = match range {
                None => String::new(),
                Some((min, max)) => {
                    let min = min.map_or(String::new(), |v| v.to_string());
                    let max = max.map_or(String::new(), |v| v.to_string());
                    format!("*{min}..{max}")
                }
            };
            let (open, close) = match direction {
                RelDirection::Out => ("-", "->"),
                RelDirection::In => ("<-", "-"),
                RelDirection::Undirected => ("-", "-"),
            };
            let opt = if optional.is_some() { "Optional" } else { "" };
            let kind = if *into { "ExpandInto" } else { "Expand" };
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
            optional,
        } => {
            let opt = if optional.is_some() { "Optional" } else { "" };
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
                .map(|(expr, asc)| {
                    format!(
                        "{} {}",
                        expr_text(expr, query),
                        if *asc { "ASC" } else { "DESC" }
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
        BoundExpr::Param(ix) => format!("${}", query.params[*ix]),
        BoundExpr::Var(slot) => slot_name(query, *slot).to_string(),
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
                },
                NodeDef {
                    id: 1,
                    name: "Place".into(),
                    node_count: 1400,
                },
            ],
            vec![
                RelDef {
                    id: 2,
                    name: "KNOWS".into(),
                    from: 0,
                    to: 0,
                    edge_count: 180_000,
                },
                RelDef {
                    id: 3,
                    name: "IS_LOCATED_IN".into(),
                    from: 0,
                    to: 1,
                    edge_count: 9000,
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
    fn disconnected_paths_scan_separately() {
        let text = explained("MATCH (a:Person), (b:Place) RETURN a, b");
        assert_eq!(text.matches("ScanNodes").count(), 2, "got:\n{text}");
    }

    #[test]
    fn unlabeled_scan_lists_every_candidate() {
        let text = explained("MATCH (n) RETURN n LIMIT 1");
        assert!(text.contains("ScanNodes n: Person|Place"), "got:\n{text}");
    }

    #[test]
    fn path_variables_are_rejected_for_now() {
        let query = binder::bind(
            &parse("MATCH p = (a:Person)-[:KNOWS]->(b) RETURN p").expect("parse"),
            &schema(),
        )
        .expect("bind");
        let err = build(&query).expect_err("path plans later");
        assert!(err.to_string().contains("path variables"), "got: {err}");
    }
}
