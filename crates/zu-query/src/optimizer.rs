//! Plan rewrites: DP join ordering driven by degree statistics and
//! filter placement at the earliest bound point (docs/07 §3).
//!
//! The optimizer rewrites maximal runs of non-optional ScanNodes,
//! Expand, and Filter operators. Within a run it splits AND
//! conjunctions into single predicates, builds the join graph over node
//! slots, and orders every connected component by dynamic programming
//! over relationship subsets. Components touching a slot a previous
//! clause already bound expand out from those rows without a scan;
//! the rest try every node as the scan seed and keep the cheapest.
//! Cost is the running sum of estimated intermediate cardinalities:
//! scans cost their candidate table counts, expands multiply by the
//! average degree computed from the catalog's node and edge counts,
//! closing a cycle multiplies by edge probability, and filters apply
//! fixed selectivities (equality on `id` is a point lookup, other
//! equality 0.1, ranges 0.3, the rest 0.5) until the column catalog
//! carries real statistics. Filters are emitted the moment every slot
//! they touch is bound, which puts them directly above the scan or
//! expand that completes them.
//!
//! Components with more than twelve relationships keep their written
//! order, and optional operators are never reordered: left-outer
//! semantics pin them where the query put them.

use std::collections::{BTreeMap, HashMap, HashSet};

use zu_common::Result;

use crate::ast::{BinaryOp, Literal, RelDirection};
use crate::binder::{BoundExpr, BoundQuery, ColStats, ColorSummary, Schema};
use crate::plan::{LogicalPlan, VarLength};

/// Components larger than this keep their written join order.
const MAX_DP_RELS: usize = 12;

/// One line EXPLAIN prints above the plan when the optimizer took a
/// decision the tree itself does not show.
pub type Note = String;

/// Rewrites a built plan with join ordering and filter placement,
/// then marks closing expands: ASP hash joins where the estimates
/// justify the accumulate sweep, and the WCOJ intersection where the
/// close completes a cycle the fusion can take.
pub fn optimize(plan: LogicalPlan, query: &BoundQuery, schema: &Schema) -> Result<LogicalPlan> {
    Ok(optimize_noted(plan, query, schema)?.0)
}

/// The same optimization handing back the notes the run produced, for
/// callers that render EXPLAIN and want to show what the tree cannot:
/// today that is only the dual run of perf/12 §2.4 firing.
pub fn optimize_noted(
    plan: LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
) -> Result<(LogicalPlan, Vec<Note>)> {
    let mut notes = Vec::new();
    let plan = rewrite(plan, query, schema, &mut notes)?;
    Ok((mark_asp(plan, query, schema).0, notes))
}

/// Bottom-up estimated-cardinality walk that turns closing expands
/// into ASP hash joins (docs/07). A closing expand probes storage once
/// per input row while the hash join pays one accumulate sweep over
/// the rel's edges before probes get cheap, so the flag is set exactly
/// when the estimated input rows exceed the closing rel's edge count.
/// Optional groups keep their written probe semantics and var-length
/// closes do not execute yet, so both stay unmarked. Estimates reuse
/// the DP helpers; an aggregation resets the running estimate to one
/// row because grouped cardinality is unknown until the column catalog
/// carries statistics, which only understates and keeps ExpandInto.
fn mark_asp(plan: LogicalPlan, query: &BoundQuery, schema: &Schema) -> (LogicalPlan, f64) {
    mark_asp_walk(
        plan,
        query,
        schema,
        &mut BTreeMap::new(),
        &mut Ceiling::default(),
        &mut Vec::new(),
    )
}

/// What the optimizer expects of one operator: the estimate it
/// settled on and the pessimistic ceiling that clamped it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    /// The rows this operator is expected to produce.
    pub est: f64,
    /// The most rows it can produce, when the statistics are there to
    /// say. Real rows above this is a bound violation, which perf/12
    /// §6 makes a hard fail: the ceiling is a promise, not a guess.
    pub bnd: Option<f64>,
}

/// The per-operator row estimate the optimizer settled on, one entry
/// per operator, bottom-up, in the same order `exec` linearizes the
/// plan into its operator pipeline. This is what EXPLAIN ANALYZE holds
/// against the measured row counts to report q-error (perf/12 §4).
///
/// It reruns the marking walk on a clone rather than caching the
/// numbers on the plan. The walk is a few dozen float operations over
/// a tree that has already survived the DP, and keeping the estimate
/// off `LogicalPlan` keeps plan equality meaning what it means today.
pub fn estimates(plan: &LogicalPlan, query: &BoundQuery, schema: &Schema) -> Vec<Estimate> {
    let mut out = Vec::new();
    mark_asp_walk(
        plan.clone(),
        query,
        schema,
        &mut BTreeMap::new(),
        &mut Ceiling::default(),
        &mut out,
    );
    out
}

/// The running pessimistic ceiling the marking walk carries, so the
/// estimate it reports is clamped by the same bounds the DP ordered
/// with (perf/12 §2.4). The DP clamps inside its own search and the
/// walk runs afterwards over the plan the search picked, so without
/// this the two disagree and it is the walk's number EXPLAIN prints.
#[derive(Debug)]
struct Ceiling {
    /// None once any operator has no ceiling to offer, which turns the
    /// clamp off for everything above it.
    bnd: Option<f64>,
    /// The slot whose values are still all different, the seed scan
    /// and nothing above it.
    distinct: Option<usize>,
    /// How the rows on each slot an expand has walked are spread over
    /// that slot's nodes. Slots not in here are still flat.
    spread: BTreeMap<usize, Spread>,
}

impl Ceiling {
    /// Records what a step over `e` from `src` to `dst` leaves on both
    /// ends: one row per edge, each end spread by the degree its own
    /// side of the step read.
    fn walked(&mut self, e: &ExpandOp, src: usize, dst: usize, query: &BoundQuery) {
        let rel = lone_rel(e, query);
        let near = walk_sides(e, src);
        let far: Vec<usize> = near.iter().map(|d| 1 - d).collect();
        self.spread.insert(src, Spread::after(rel, near));
        self.spread.insert(dst, Spread::after(rel, &far));
    }
}

impl Default for Ceiling {
    /// The source is one row and that is a ceiling as much as an
    /// estimate, so the walk starts with the clamp armed.
    fn default() -> Self {
        Ceiling {
            bnd: Some(1.0),
            distinct: None,
            spread: BTreeMap::new(),
        }
    }
}

/// The [`mark_asp`] recursion, threading each slot's color
/// distribution so a colored close sees the frontier its probe side
/// actually carries, and recording every operator's estimate into
/// `out` on the way back up. `Empty` records nothing because the
/// linearization `exec` builds does not carry it either.
fn mark_asp_walk(
    plan: LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    dists: &mut BTreeMap<usize, (u32, Vec<f64>)>,
    ceil: &mut Ceiling,
    out: &mut Vec<Estimate>,
) -> (LogicalPlan, f64) {
    let (plan, est) = mark_asp_node(plan, query, schema, dists, ceil, out);
    if !matches!(plan, LogicalPlan::Empty) {
        out.push(Estimate { est, bnd: ceil.bnd });
    }
    (plan, est)
}

fn mark_asp_node(
    plan: LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    dists: &mut BTreeMap<usize, (u32, Vec<f64>)>,
    ceil: &mut Ceiling,
    out: &mut Vec<Estimate>,
) -> (LogicalPlan, f64) {
    match plan {
        LogicalPlan::Empty => (LogicalPlan::Empty, 1.0),
        LogicalPlan::ScanNodes {
            input,
            slot,
            optional,
        } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            let card = slot_card(slot, query, schema);
            // The seed scan is the one place rows are known to hold
            // every node once. A scan above anything else is a cross
            // product and its slot repeats per input row.
            ceil.distinct = (est == 1.0).then_some(slot);
            ceil.bnd = ceil.bnd.map(|b| b * card);
            let est = est * card;
            (
                LogicalPlan::ScanNodes {
                    input: Box::new(input),
                    slot,
                    optional,
                },
                est,
            )
        }
        LogicalPlan::Expand {
            input,
            rel,
            from,
            to,
            direction,
            range,
            into,
            asp: _,
            wcoj: _,
            optional,
        } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            let e = ExpandOp {
                rel,
                from,
                to,
                direction,
                range,
                into,
            };
            let (asp, wcoj, est) = if into {
                let edges: f64 = query.variables[rel]
                    .rel_tables
                    .iter()
                    .filter_map(|id| schema.rel_by_id(*id))
                    .map(|rd| rd.edge_count as f64)
                    .sum();
                let asp = optional.is_none() && range.is_none() && est > edges.max(1.0);
                // A closing expand completes a cycle in the join graph
                // by construction, so docs/07 §4 injects the multiway
                // intersection here. The fusion reads one sorted list
                // per side, so undirected closes and multi-table rels
                // keep the binary probe; the 16x intermediate-to-output
                // ratio for acyclic marks arrives with the §6
                // histograms. Optional closes are marked too: the
                // compiler only fuses within one group, which keeps
                // left-outer semantics exact.
                let wcoj = range.is_none()
                    && !matches!(direction, RelDirection::Undirected)
                    && query.variables[rel].rel_tables.len() == 1;
                // A close keeps or drops rows, never adds, so the
                // ceiling under it stands and only the spread moves.
                ceil.walked(&e, from, to, query);
                (asp, wcoj, est * into_prob(&e, query, schema))
            } else {
                let (factor, reached) = expand_estimate(
                    &e,
                    from,
                    ceil.spread.get(&from).copied().unwrap_or_default(),
                    dists.get(&from).cloned().as_ref(),
                    query,
                    schema,
                );
                if let Some(d) = reached {
                    dists.insert(to, d);
                }
                let distinct = ceil.distinct == Some(from);
                ceil.walked(&e, from, to, query);
                ceil.bnd = ceil
                    .bnd
                    .and_then(|b| step_bound(&e, from, b, distinct, query, schema));
                ceil.distinct = None;
                (false, false, est * factor)
            };
            let est = ceil.bnd.map_or(est, |b| est.min(b));
            (
                LogicalPlan::Expand {
                    input: Box::new(input),
                    rel,
                    from,
                    to,
                    direction,
                    range,
                    into,
                    asp,
                    wcoj,
                    optional,
                },
                est.max(1e-6),
            )
        }
        LogicalPlan::Filter {
            input,
            expr,
            optional,
        } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            let est = (est * selectivity(&expr, query, schema)).max(1e-6);
            // A filter only removes rows, so the ceiling under it still
            // holds over its output.
            let est = ceil.bnd.map_or(est, |b| est.min(b));
            (
                LogicalPlan::Filter {
                    input: Box::new(input),
                    expr,
                    optional,
                },
                est,
            )
        }
        LogicalPlan::Unwind { input, expr, slot } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            // Nothing bounds how wide a list is.
            ceil.bnd = None;
            (
                LogicalPlan::Unwind {
                    input: Box::new(input),
                    expr,
                    slot,
                },
                est * 10.0,
            )
        }
        LogicalPlan::TableFunction {
            input,
            func,
            rel,
            table,
            args,
            slots,
        } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            // One row per node of the rel's domain, exactly.
            let rows = schema
                .node_by_id(table)
                .map(|n| n.node_count as f64)
                .unwrap_or(1.0);
            ceil.bnd = ceil.bnd.map(|b| b * rows);
            ceil.distinct = None;
            (
                LogicalPlan::TableFunction {
                    input: Box::new(input),
                    func,
                    rel,
                    table,
                    args,
                    slots,
                },
                (est * rows).max(1.0),
            )
        }
        LogicalPlan::Project { input, items } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            (
                LogicalPlan::Project {
                    input: Box::new(input),
                    items,
                },
                est,
            )
        }
        LogicalPlan::Aggregate { input, keys, aggs } => {
            let (input, _) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            // Grouping changes what a slot's rows are, so the tracked
            // distributions go with the estimate. Nothing here says how
            // many groups there will be, so the ceiling goes too rather
            // than capping everything above at the one row this claims.
            dists.clear();
            ceil.bnd = None;
            ceil.distinct = None;
            (
                LogicalPlan::Aggregate {
                    input: Box::new(input),
                    keys,
                    aggs,
                },
                1.0,
            )
        }
        LogicalPlan::Distinct { input } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            (
                LogicalPlan::Distinct {
                    input: Box::new(input),
                },
                est,
            )
        }
        LogicalPlan::Sort { input, keys } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            (
                LogicalPlan::Sort {
                    input: Box::new(input),
                    keys,
                },
                est,
            )
        }
        LogicalPlan::Skip { input, expr } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            (
                LogicalPlan::Skip {
                    input: Box::new(input),
                    expr,
                },
                est,
            )
        }
        LogicalPlan::Limit { input, expr } => {
            let (input, est) = mark_asp_walk(*input, query, schema, dists, ceil, out);
            (
                LogicalPlan::Limit {
                    input: Box::new(input),
                    expr,
                },
                est,
            )
        }
    }
}

fn rewrite(
    plan: LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    notes: &mut Vec<Note>,
) -> Result<LogicalPlan> {
    if matches!(
        &plan,
        LogicalPlan::Filter { optional: None, .. }
            | LogicalPlan::ScanNodes { optional: None, .. }
            | LogicalPlan::Expand { optional: None, .. }
    ) {
        return reorder_run(plan, query, schema, notes);
    }
    match plan {
        LogicalPlan::Empty => Ok(LogicalPlan::Empty),
        LogicalPlan::Filter {
            input,
            expr,
            optional,
        } => Ok(LogicalPlan::Filter {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            expr,
            optional,
        }),
        LogicalPlan::ScanNodes {
            input,
            slot,
            optional,
        } => Ok(LogicalPlan::ScanNodes {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            slot,
            optional,
        }),
        LogicalPlan::Expand {
            input,
            rel,
            from,
            to,
            direction,
            range,
            into,
            asp,
            wcoj,
            optional,
        } => Ok(LogicalPlan::Expand {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            rel,
            from,
            to,
            direction,
            range,
            into,
            asp,
            wcoj,
            optional,
        }),
        LogicalPlan::Unwind { input, expr, slot } => Ok(LogicalPlan::Unwind {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            expr,
            slot,
        }),
        LogicalPlan::TableFunction {
            input,
            func,
            rel,
            table,
            args,
            slots,
        } => Ok(LogicalPlan::TableFunction {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            func,
            rel,
            table,
            args,
            slots,
        }),
        LogicalPlan::Project { input, items } => Ok(LogicalPlan::Project {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            items,
        }),
        LogicalPlan::Aggregate { input, keys, aggs } => Ok(LogicalPlan::Aggregate {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            keys,
            aggs,
        }),
        LogicalPlan::Distinct { input } => Ok(LogicalPlan::Distinct {
            input: Box::new(rewrite(*input, query, schema, notes)?),
        }),
        LogicalPlan::Sort { input, keys } => Ok(LogicalPlan::Sort {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            keys,
        }),
        LogicalPlan::Skip { input, expr } => Ok(LogicalPlan::Skip {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            expr,
        }),
        LogicalPlan::Limit { input, expr } => Ok(LogicalPlan::Limit {
            input: Box::new(rewrite(*input, query, schema, notes)?),
            expr,
        }),
    }
}

/// One operator of a reorderable run, in bottom-up execution order.
#[derive(Clone)]
enum RunOp {
    Scan(usize),
    Expand(ExpandOp),
    Filter(BoundExpr),
}

#[derive(Clone)]
struct ExpandOp {
    rel: usize,
    from: usize,
    to: usize,
    direction: RelDirection,
    range: Option<VarLength>,
    into: bool,
}

/// One planned placement decision inside a component.
#[derive(Clone)]
enum Step {
    Scan(usize),
    /// Expand `expands[ix]` from `from` to `to`; `into` when the far
    /// slot was already bound.
    Expand {
        ix: usize,
        from: usize,
        to: usize,
        into: bool,
    },
}

struct Component {
    nodes: Vec<usize>,
    rels: Vec<usize>,
    anchored: bool,
}

fn reorder_run(
    top: LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    notes: &mut Vec<Note>,
) -> Result<LogicalPlan> {
    let mut ops: Vec<RunOp> = Vec::new();
    let mut cur = top;
    let rest = loop {
        cur = match cur {
            LogicalPlan::Filter {
                input,
                expr,
                optional: None,
            } => {
                ops.push(RunOp::Filter(expr));
                *input
            }
            LogicalPlan::ScanNodes {
                input,
                slot,
                optional: None,
            } => {
                ops.push(RunOp::Scan(slot));
                *input
            }
            LogicalPlan::Expand {
                input,
                rel,
                from,
                to,
                direction,
                range,
                into,
                asp: _,
                wcoj: _,
                optional: None,
            } => {
                ops.push(RunOp::Expand(ExpandOp {
                    rel,
                    from,
                    to,
                    direction,
                    range,
                    into,
                }));
                *input
            }
            other => break other,
        };
    };
    let below = rewrite(rest, query, schema, notes)?;
    ops.reverse();

    let mut b0 = HashSet::new();
    bound_slots(&below, &mut b0);

    let mut scans: Vec<usize> = Vec::new();
    let mut expands: Vec<ExpandOp> = Vec::new();
    let mut filters: Vec<BoundExpr> = Vec::new();
    for op in &ops {
        match op {
            RunOp::Scan(slot) => scans.push(*slot),
            RunOp::Expand(e) => expands.push(e.clone()),
            RunOp::Filter(expr) => split_and(expr.clone(), &mut filters),
        }
    }

    // Node slots of the run in first-appearance order.
    let mut nodes: Vec<usize> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    for op in &ops {
        let touched: &[usize] = match op {
            RunOp::Scan(slot) => &[*slot],
            RunOp::Expand(e) => &[e.from, e.to],
            RunOp::Filter(_) => &[],
        };
        for slot in touched {
            if seen.insert(*slot) {
                nodes.push(*slot);
            }
        }
    }

    let components = connect(&nodes, &expands, &b0);
    if components.iter().any(|c| c.rels.len() > MAX_DP_RELS) {
        return Ok(rebuild(ops, below));
    }

    let mut planned: Vec<(Vec<Step>, f64, bool)> = Vec::new();
    for comp in &components {
        let (steps, card, apart) = order_component(comp, &expands, &filters, &b0, query, schema);
        if let Some(apart) = apart {
            notes.push(format!(
                "bound disagreement {apart:.0}x over the {:.0}x threshold, join order taken off the ceilings",
                schema.bound_disagreement()
            ));
        }
        planned.push((steps, card, comp.anchored));
    }
    // Anchored components extend existing rows and go first; the rest
    // are cross products, cheapest first.
    planned.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut plan = below;
    let mut bound = b0.clone();
    let mut placed = vec![false; filters.len()];
    plan = place_filters(plan, &filters, &mut placed, &bound);
    for (steps, _, _) in &planned {
        for step in steps {
            plan = match step {
                Step::Scan(slot) => {
                    bound.insert(*slot);
                    LogicalPlan::ScanNodes {
                        input: Box::new(plan),
                        slot: *slot,
                        optional: None,
                    }
                }
                Step::Expand { ix, from, to, into } => {
                    let e = &expands[*ix];
                    bound.insert(e.rel);
                    bound.insert(*from);
                    bound.insert(*to);
                    let direction = if *from == e.from {
                        e.direction
                    } else {
                        flip(e.direction)
                    };
                    LogicalPlan::Expand {
                        input: Box::new(plan),
                        rel: e.rel,
                        from: *from,
                        to: *to,
                        direction,
                        range: e.range,
                        into: *into,
                        asp: false,
                        wcoj: false,
                        optional: None,
                    }
                }
            };
            plan = place_filters(plan, &filters, &mut placed, &bound);
        }
    }
    // Anything left never became placeable; keep it on top so no
    // predicate is dropped.
    for (ix, filter) in filters.iter().enumerate() {
        if !placed[ix] {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                expr: filter.clone(),
                optional: None,
            };
        }
    }
    Ok(plan)
}

/// Rebuilds a run unchanged when it is too large to reorder.
fn rebuild(ops: Vec<RunOp>, below: LogicalPlan) -> LogicalPlan {
    let mut plan = below;
    for op in ops {
        plan = match op {
            RunOp::Scan(slot) => LogicalPlan::ScanNodes {
                input: Box::new(plan),
                slot,
                optional: None,
            },
            RunOp::Expand(e) => LogicalPlan::Expand {
                input: Box::new(plan),
                rel: e.rel,
                from: e.from,
                to: e.to,
                direction: e.direction,
                range: e.range,
                into: e.into,
                asp: false,
                wcoj: false,
                optional: None,
            },
            RunOp::Filter(expr) => LogicalPlan::Filter {
                input: Box::new(plan),
                expr,
                optional: None,
            },
        };
    }
    plan
}

fn flip(direction: RelDirection) -> RelDirection {
    match direction {
        RelDirection::Out => RelDirection::In,
        RelDirection::In => RelDirection::Out,
        RelDirection::Undirected => RelDirection::Undirected,
    }
}

/// Groups the run's node slots into connected components. Slots bound
/// below the run count as one shared root: rows carrying them arrive
/// together, so rels touching different bound slots never cross join.
fn connect(nodes: &[usize], expands: &[ExpandOp], b0: &HashSet<usize>) -> Vec<Component> {
    let index: HashMap<usize, usize> = nodes.iter().enumerate().map(|(i, s)| (*s, i)).collect();
    let mut parent: Vec<usize> = (0..nodes.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    let union = |parent: &mut [usize], a: usize, b: usize| {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[ra] = rb;
        }
    };
    for e in expands {
        union(&mut parent, index[&e.from], index[&e.to]);
    }
    let anchors: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, s)| b0.contains(s))
        .map(|(i, _)| i)
        .collect();
    for pair in anchors.windows(2) {
        union(&mut parent, pair[0], pair[1]);
    }
    let mut by_root: HashMap<usize, Component> = HashMap::new();
    let mut order: Vec<usize> = Vec::new();
    for (i, slot) in nodes.iter().enumerate() {
        let root = find(&mut parent, i);
        let comp = by_root.entry(root).or_insert_with(|| {
            order.push(root);
            Component {
                nodes: Vec::new(),
                rels: Vec::new(),
                anchored: false,
            }
        });
        comp.nodes.push(*slot);
        comp.anchored |= b0.contains(slot);
    }
    for (ix, e) in expands.iter().enumerate() {
        let root = find(&mut parent, index[&e.from]);
        by_root.get_mut(&root).expect("component").rels.push(ix);
    }
    order
        .into_iter()
        .map(|root| by_root.remove(&root).expect("component"))
        .collect()
}

/// DP over relationship subsets of one component. Returns the cheapest
/// step order, its estimated output cardinality, and how far the two
/// disagreed when that disagreement is what chose the order.
///
/// Every step also carries a pessimistic row ceiling from the degree
/// histograms (docs/07 §6): the worst degree any single row can
/// multiply by. The ceiling clamps the estimate, and when the summed
/// ceilings exceed the summed estimates by the schema's dual run
/// threshold the DP reruns minimizing the ceiling, with the estimate
/// only breaking near ties, so a skew-blind guess cannot pick an order
/// whose worst case is catastrophic. Steps without histograms have no
/// usable ceiling and disable the caps for their order.
fn order_component(
    comp: &Component,
    expands: &[ExpandOp],
    filters: &[BoundExpr],
    b0: &HashSet<usize>,
    query: &BoundQuery,
    schema: &Schema,
) -> (Vec<Step>, f64, Option<f64>) {
    #[derive(Clone)]
    struct Entry {
        cost: f64,
        card: f64,
        /// Pessimistic ceiling on `card`, None once any step lacks one.
        bnd: Option<f64>,
        /// The slot whose values are still known to be all different,
        /// which is the seed slot straight off its scan and nothing
        /// after that: an expand repeats its source once per neighbor
        /// and lands several sources on the same neighbor.
        distinct: Option<usize>,
        /// Running sum of the ceilings, the bound analog of `cost`.
        bcost: f64,
        /// Color distribution per slot a colored expand reached, tagged
        /// with the rel table whose color space it lives in; the state
        /// the next hop's estimate conditions on.
        dists: BTreeMap<usize, (u32, Vec<f64>)>,
        /// How the rows on each slot the order has walked are spread
        /// over that slot's nodes. Slots not in here are still flat.
        spread: BTreeMap<usize, Spread>,
        steps: Vec<Step>,
    }
    let filter_slots: Vec<HashSet<usize>> = filters
        .iter()
        .map(|f| {
            let mut s = HashSet::new();
            expr_slots(f, &mut s);
            s
        })
        .collect();
    // Selectivity of filters that become placeable when `bound` grows
    // into `grown`.
    let newly = |bound: &HashSet<usize>, grown: &HashSet<usize>| -> f64 {
        filter_slots
            .iter()
            .zip(filters)
            .filter(|(slots, _)| slots.is_subset(grown) && !slots.is_subset(bound))
            .map(|(_, f)| selectivity(f, query, schema))
            .product()
    };
    let seeds: Vec<Option<usize>> = if comp.anchored {
        vec![None]
    } else {
        comp.nodes.iter().map(|s| Some(*s)).collect()
    };
    let full: u32 = if comp.rels.is_empty() {
        0
    } else {
        (1u32 << comp.rels.len()) - 1
    };
    // In bound mode a poisoned order never wins, a clearly lower
    // ceiling always wins, and near ties within one percent fall to
    // the estimate: products taken in a different order drift in the
    // last bits, so exact ceiling ties cannot be trusted.
    let beats = |by_bound: bool, cand: &Entry, cur: &Entry| -> bool {
        if !by_bound {
            return cand.cost < cur.cost;
        }
        match (cand.bnd, cur.bnd) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(_), Some(_)) => {
                cand.bcost < cur.bcost * 0.99
                    || (cand.bcost <= cur.bcost * 1.01 && cand.cost < cur.cost)
            }
        }
    };
    let run = |by_bound: bool| -> Option<Entry> {
        let mut best: Option<Entry> = None;
        for seed in &seeds {
            let base_bound = |mask: u32| -> HashSet<usize> {
                let mut bound = b0.clone();
                if let Some(slot) = seed {
                    bound.insert(*slot);
                }
                for (i, rel) in comp.rels.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        let e = &expands[*rel];
                        bound.insert(e.from);
                        bound.insert(e.to);
                        bound.insert(e.rel);
                    }
                }
                bound
            };
            let mut dp: Vec<Option<Entry>> = vec![None; (full as usize) + 1];
            dp[0] = Some(match seed {
                None => Entry {
                    cost: 0.0,
                    card: 1.0,
                    bnd: Some(1.0),
                    distinct: None,
                    bcost: 0.0,
                    dists: BTreeMap::new(),
                    spread: BTreeMap::new(),
                    steps: Vec::new(),
                },
                Some(slot) => {
                    let grown = base_bound(0);
                    let card = (slot_card(*slot, query, schema) * newly(b0, &grown)).max(1e-6);
                    // A key point filter placeable right above the seed
                    // scan pins the ceiling to one row per candidate
                    // table; every other filter leaves it alone because
                    // the worst case keeps every row.
                    let pinned = filter_slots.iter().zip(filters).any(|(slots, f)| {
                        slots.is_subset(&grown)
                            && !slots.is_subset(b0)
                            && key_point(f, *slot, query)
                    });
                    let bnd = if pinned {
                        query.variables[*slot].node_tables.len() as f64
                    } else {
                        slot_card(*slot, query, schema)
                    };
                    Entry {
                        cost: card,
                        card,
                        bnd: Some(bnd),
                        distinct: Some(*slot),
                        bcost: bnd,
                        spread: BTreeMap::new(),
                        dists: BTreeMap::new(),
                        steps: vec![Step::Scan(*slot)],
                    }
                }
            });
            for mask in 0..=full {
                let Some(entry) = dp[mask as usize].clone() else {
                    continue;
                };
                let bound = base_bound(mask);
                for (i, rel) in comp.rels.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        continue;
                    }
                    let e = &expands[*rel];
                    let from_bound = bound.contains(&e.from);
                    let to_bound = bound.contains(&e.to);
                    if !from_bound && !to_bound {
                        continue;
                    }
                    // Absolute ceiling on the rows the step leaves,
                    // not a multiplier: the lp bound on a distinct
                    // source is not a per-row number.
                    let bound_after = |src: usize| {
                        entry.bnd.and_then(|b| {
                            let distinct = entry.distinct == Some(src);
                            step_bound(e, src, b, distinct, query, schema)
                        })
                    };
                    let spread_of =
                        |src: usize| entry.spread.get(&src).copied().unwrap_or_default();
                    // Both ends of a taken step hold one row per edge
                    // afterwards, each spread by the degree its own end
                    // of the step read.
                    let walked = |src: usize, dst: usize| {
                        let rel = lone_rel(e, query);
                        let near = walk_sides(e, src);
                        let far: Vec<usize> = near.iter().map(|d| 1 - d).collect();
                        let mut next = entry.spread.clone();
                        next.insert(src, Spread::after(rel, near));
                        next.insert(dst, Spread::after(rel, &far));
                        next
                    };
                    let (step, factor, bnd, reached, spread) = if from_bound && to_bound {
                        (
                            Step::Expand {
                                ix: *rel,
                                from: e.from,
                                to: e.to,
                                into: true,
                            },
                            into_prob(e, query, schema),
                            // A close keeps or drops rows, never adds.
                            entry.bnd,
                            None,
                            walked(e.from, e.to),
                        )
                    } else if from_bound {
                        let (factor, dist) = expand_estimate(
                            e,
                            e.from,
                            spread_of(e.from),
                            entry.dists.get(&e.from),
                            query,
                            schema,
                        );
                        (
                            Step::Expand {
                                ix: *rel,
                                from: e.from,
                                to: e.to,
                                into: false,
                            },
                            factor,
                            bound_after(e.from),
                            dist.map(|d| (e.to, d)),
                            walked(e.from, e.to),
                        )
                    } else {
                        let (factor, dist) = expand_estimate(
                            e,
                            e.to,
                            spread_of(e.to),
                            entry.dists.get(&e.to),
                            query,
                            schema,
                        );
                        (
                            Step::Expand {
                                ix: *rel,
                                from: e.to,
                                to: e.from,
                                into: false,
                            },
                            factor,
                            bound_after(e.to),
                            dist.map(|d| (e.from, d)),
                            walked(e.to, e.from),
                        )
                    };
                    let next = mask | (1 << i);
                    let grown = base_bound(next);
                    let bnd = bnd.map(|b| b.max(1e-6));
                    let card = (entry.card * factor * newly(&bound, &grown)).max(1e-6);
                    let card = bnd.map_or(card, |b| card.min(b));
                    let mut dists = entry.dists.clone();
                    if let Some((slot, d)) = reached {
                        dists.insert(slot, d);
                    }
                    let cand = Entry {
                        cost: entry.cost + card,
                        card,
                        bnd,
                        distinct: None,
                        bcost: entry.bcost + bnd.unwrap_or(0.0),
                        dists,
                        spread,
                        steps: Vec::new(),
                    };
                    let slot = &mut dp[next as usize];
                    if slot.as_ref().is_none_or(|s| beats(by_bound, &cand, s)) {
                        let mut steps = entry.steps.clone();
                        steps.push(step);
                        *slot = Some(Entry { steps, ..cand });
                    }
                }
            }
            let done = dp[full as usize].take();
            let improves = |done: &Entry| best.as_ref().is_none_or(|b| beats(by_bound, done, b));
            if done.as_ref().is_some_and(improves) {
                best = done;
            }
        }
        best
    };
    let best = run(false).expect("connected component always has an order");
    // Bound mode is there to catch an order whose estimate came out of
    // guessed selectivities. A seed pinned by a primary key is not a
    // guess, it is one row, so the estimate above it stands and the
    // ceiling does not get to overrule it: the ceiling has to assume
    // the worst node in the table at every hop, which reads as a
    // blowup next to a full scan whose own ceiling caps out at the
    // edge count, and the scan then wins an order of magnitude of
    // real work.
    let pinned_seed = match best.steps.first() {
        Some(Step::Scan(slot)) => filters.iter().any(|f| key_point(f, *slot, query)),
        _ => false,
    };
    // A zero cost order is free by the estimates and any ceiling above
    // it is infinitely far away, which is the disagreement the dual run
    // exists for, so the divide is left to hand back infinity.
    let apart = (!pinned_seed && best.bnd.is_some())
        .then(|| best.bcost / best.cost)
        .filter(|apart| *apart > schema.bound_disagreement());
    let best = match apart {
        Some(_) => run(true).unwrap_or(best),
        None => best,
    };
    (best.steps, best.card, apart)
}

/// Emits every not yet placed filter whose slots are all bound.
fn place_filters(
    mut plan: LogicalPlan,
    filters: &[BoundExpr],
    placed: &mut [bool],
    bound: &HashSet<usize>,
) -> LogicalPlan {
    for (ix, filter) in filters.iter().enumerate() {
        if placed[ix] {
            continue;
        }
        let mut slots = HashSet::new();
        expr_slots(filter, &mut slots);
        if slots.is_subset(bound) {
            placed[ix] = true;
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                expr: filter.clone(),
                optional: None,
            };
        }
    }
    plan
}

/// Slots a subtree has introduced and still exposes. Project and
/// Aggregate replace visibility, so the walk stops at them.
fn bound_slots(plan: &LogicalPlan, out: &mut HashSet<usize>) {
    match plan {
        LogicalPlan::Empty => {}
        LogicalPlan::ScanNodes { input, slot, .. } => {
            out.insert(*slot);
            bound_slots(input, out);
        }
        LogicalPlan::Expand {
            input,
            rel,
            from,
            to,
            ..
        } => {
            out.insert(*rel);
            out.insert(*from);
            out.insert(*to);
            bound_slots(input, out);
        }
        LogicalPlan::Unwind { input, slot, .. } => {
            out.insert(*slot);
            bound_slots(input, out);
        }
        LogicalPlan::TableFunction { input, slots, .. } => {
            out.extend(slots.iter().copied());
            bound_slots(input, out);
        }
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Limit { input, .. } => bound_slots(input, out),
        LogicalPlan::Project { items, .. } => {
            for item in items {
                if let Some(slot) = item.slot {
                    out.insert(slot);
                }
            }
        }
        LogicalPlan::Aggregate { keys, aggs, .. } => {
            for item in keys.iter().chain(aggs) {
                if let Some(slot) = item.slot {
                    out.insert(slot);
                }
            }
        }
    }
}

fn split_and(expr: BoundExpr, out: &mut Vec<BoundExpr>) {
    if let BoundExpr::Binary {
        op: BinaryOp::And,
        lhs,
        rhs,
    } = expr
    {
        split_and(*lhs, out);
        split_and(*rhs, out);
    } else {
        out.push(expr);
    }
}

fn expr_slots(expr: &BoundExpr, out: &mut HashSet<usize>) {
    match expr {
        BoundExpr::Literal(_) | BoundExpr::Param(_) => {}
        BoundExpr::Var(slot) => {
            out.insert(*slot);
        }
        BoundExpr::Property { base, key: _ } => expr_slots(base, out),
        BoundExpr::Unary { expr, .. } => expr_slots(expr, out),
        BoundExpr::Binary { lhs, rhs, .. } => {
            expr_slots(lhs, out);
            expr_slots(rhs, out);
        }
        BoundExpr::IsNull { expr, .. } => expr_slots(expr, out),
        BoundExpr::Call { args, .. } => {
            for arg in args {
                expr_slots(arg, out);
            }
        }
        BoundExpr::List(items) => {
            for item in items {
                expr_slots(item, out);
            }
        }
        BoundExpr::Map(entries) => {
            for (_, value) in entries {
                expr_slots(value, out);
            }
        }
    }
}

fn node_count(schema: &Schema, table: u32) -> f64 {
    schema
        .node_by_id(table)
        .map_or(1.0, |n| (n.node_count as f64).max(1.0))
}

/// Total rows a scan of this slot's candidate tables produces.
fn slot_card(slot: usize, query: &BoundQuery, schema: &Schema) -> f64 {
    query.variables[slot]
        .node_tables
        .iter()
        .map(|t| node_count(schema, *t))
        .sum::<f64>()
        .max(1.0)
}

/// Mean fan-out per source with at least one edge: the histogram's
/// total is exactly the number of sources holding edges, so the exact
/// positive-source mean needs no bucket arithmetic. Sources without
/// edges produce no rows, so this is the fan-out a pipeline actually
/// sees, where the count ratio dilutes it across isolated nodes.
fn hist_fanout(edges: f64, hist: &[u64]) -> Option<f64> {
    let sources: u64 = hist.iter().sum();
    if sources == 0 {
        return None;
    }
    Some(edges / sources as f64)
}

/// Average fan-out of one expand step taken from `source`, from the
/// engine's degree histograms when it carries them (docs/07 §6) and
/// the count ratios otherwise. Var-length steps raise the degree to
/// their minimum hop count as a heuristic.
fn degree(e: &ExpandOp, source: usize, spread: Spread, query: &BoundQuery, schema: &Schema) -> f64 {
    let hops = e.range.map_or(1, |v| v.min.unwrap_or(1).clamp(1, 8)) as i32;
    hop_degree(e, source, spread, query, schema)
        .max(1e-6)
        .powi(hops)
}

/// The degree sides an expand from `source` reads: out-degree is 0,
/// in-degree is 1, and an undirected step reads both.
fn walk_sides(e: &ExpandOp, source: usize) -> &'static [usize] {
    let reversed = source == e.to && source != e.from;
    match (e.direction, reversed) {
        (RelDirection::Out, false) | (RelDirection::In, true) => &[0],
        (RelDirection::Out, true) | (RelDirection::In, false) => &[1],
        (RelDirection::Undirected, _) => &[0, 1],
    }
}

/// How the rows on a slot are spread over that slot's nodes, which is
/// what decides whether an expand off it sees the plain mean degree or
/// the degree-weighted one.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Spread {
    /// One row per node, what a scan leaves behind.
    #[default]
    Flat,
    /// One row per edge, so the node a row sits on is drawn in
    /// proportion to a degree: `rel` names the table whose edges they
    /// are and `side` which of its two degrees, both `None` when the
    /// step that spread them read more than one table or both sides.
    Edges {
        rel: Option<u32>,
        side: Option<usize>,
    },
}

impl Spread {
    /// The rows a slot holds after a step over `rel` read its `side`
    /// degree: one per edge of that table, spread by that same side.
    fn after(rel: Option<u32>, side: &[usize]) -> Spread {
        Spread::Edges {
            rel,
            side: match side {
                [d] => Some(*d),
                _ => None,
            },
        }
    }

    /// The degree side the rows are spread by, as seen by a step about
    /// to read `side` of `rel`. `None` says one row per node, and a
    /// spread the step cannot name reads as its own side, which is the
    /// plain degree-weighted mean.
    fn seen_by(self, rel: u32, side: usize) -> Option<usize> {
        match self {
            Spread::Flat => None,
            Spread::Edges { rel: r, side: s } => Some(match s {
                Some(s) if r == Some(rel) => s,
                _ => side,
            }),
        }
    }
}

/// [`degree`] for a single hop, the per-step mean the color walk feeds
/// its first hop.
///
/// A source slot holding one row per edge has its rows sitting on nodes
/// drawn in proportion to a degree, so the mean the step sees is the
/// degree-weighted mean and not the plain one. That is what makes a two
/// hop count come out right: a chain through a hub is counted once per
/// edge into the hub, not once per hub, and the plain mean is far too
/// small. [`binder::DegreeStats::weighted`] has the arithmetic.
fn hop_degree(
    e: &ExpandOp,
    source: usize,
    spread: Spread,
    query: &BoundQuery,
    schema: &Schema,
) -> f64 {
    let mut deg = 0.0;
    for rid in &query.variables[e.rel].rel_tables {
        let Some(rd) = schema.rel_by_id(*rid) else {
            continue;
        };
        let edges = rd.edge_count as f64;
        let hists = schema.degree_hist(*rid);
        let norms = schema.degree_norm(*rid);
        let flat = [rd.from, rd.to].map(|t| edges / node_count(schema, t));
        for d in walk_sides(e, source) {
            deg += match (norms, spread.seen_by(*rid, *d)) {
                (Some(n), Some(by)) => n.weighted(by, *d),
                _ => hists
                    .and_then(|h| hist_fanout(edges, &h[*d]))
                    .unwrap_or(flat[*d]),
            };
        }
    }
    deg
}

/// The single rel table an expand walks, when the pattern named a type
/// that resolves to exactly one.
fn lone_rel(e: &ExpandOp, query: &BoundQuery) -> Option<u32> {
    match query.variables[e.rel].rel_tables[..] {
        [rid] => Some(rid),
        _ => None,
    }
}

/// The single rel table an expand's colors can track: exactly one
/// candidate, a fixed direction, and a stored COLOR summary.
fn colored_rel(e: &ExpandOp, query: &BoundQuery, schema: &Schema) -> Option<u32> {
    if matches!(e.direction, RelDirection::Undirected) {
        return None;
    }
    let [rid] = query.variables[e.rel].rel_tables[..] else {
        return None;
    };
    schema.color_summary(rid).map(|_| rid)
}

/// A frontier spread evenly over the summarized nodes, the walk's
/// state when nothing better is known.
fn color_uniform(sum: &ColorSummary) -> Vec<f64> {
    let total: u64 = sum.counts.iter().sum();
    sum.counts
        .iter()
        .map(|&c| c as f64 / (total.max(1)) as f64)
        .collect()
}

/// One hop of the color walk: rows reached per frontier row when the
/// frontier's colors are distributed as `dist`, and the reached rows'
/// distribution. A dead walk is the summary saying no edge continues
/// from this frontier, so the fan-out floors at the estimator's epsilon
/// instead of falling back to a mean the summary just refuted.
fn color_hop(sum: &ColorSummary, dist: &[f64], reversed: bool) -> (f64, Vec<f64>) {
    let mut next = vec![0.0; sum.counts.len()];
    for &(f, t, edges, _) in &sum.triples {
        let (src, tgt) = if reversed {
            (t as usize, f as usize)
        } else {
            (f as usize, t as usize)
        };
        let count = sum.counts.get(src).copied().unwrap_or(0);
        let Some(share) = dist.get(src) else {
            continue;
        };
        if count > 0 {
            next[tgt] += share * edges as f64 / count as f64;
        }
    }
    let fan: f64 = next.iter().sum();
    if fan <= 0.0 {
        return (1e-6, color_uniform(sum));
    }
    for v in &mut next {
        *v /= fan;
    }
    (fan, next)
}

/// The color distribution of the rows one unconditioned hop reaches:
/// each edge lands on its endpoint, so endpoints weigh by edge count.
/// None when the summary holds no edges at all.
fn color_seed(sum: &ColorSummary, reversed: bool) -> Option<Vec<f64>> {
    let mut v = vec![0.0; sum.counts.len()];
    for &(f, t, edges, _) in &sum.triples {
        let tgt = if reversed { f as usize } else { t as usize };
        v[tgt] += edges as f64;
    }
    let total: f64 = v.iter().sum();
    if total <= 0.0 {
        return None;
    }
    for x in &mut v {
        *x /= total;
    }
    Some(v)
}

/// Fan-out of one expand step and, when a COLOR summary tracks it, the
/// reached rows' color distribution (docs/07 §6). The first hop from an
/// unknown frontier keeps the histogram's positive-source mean and
/// seeds the distribution with the edge-weighted endpoints; every later
/// hop walks the color matrix, because a frontier is edge-biased toward
/// hubs in a way one global mean cannot see and the tracked
/// distribution is exactly that bias. A distribution recorded by a
/// different rel table lives in a different color space and is ignored.
fn expand_estimate(
    e: &ExpandOp,
    source: usize,
    spread: Spread,
    dist: Option<&(u32, Vec<f64>)>,
    query: &BoundQuery,
    schema: &Schema,
) -> (f64, Option<(u32, Vec<f64>)>) {
    let plain = degree(e, source, spread, query, schema);
    let Some(rid) = colored_rel(e, query, schema) else {
        return (plain, None);
    };
    let sum = schema.color_summary(rid).expect("colored_rel checked");
    let reversed = source == e.to && source != e.from;
    let reversed = match e.direction {
        RelDirection::Out => reversed,
        RelDirection::In => !reversed,
        RelDirection::Undirected => unreachable!("colored_rel rejects undirected"),
    };
    let hops = e.range.map_or(1, |v| v.min.unwrap_or(1).clamp(1, 8));
    let (mut fan, mut d) = match dist {
        Some((from_rel, d)) if *from_rel == rid => color_hop(sum, d, reversed),
        _ => match color_seed(sum, reversed) {
            Some(seed) => (hop_degree(e, source, spread, query, schema).max(1e-6), seed),
            None => return (plain, None),
        },
    };
    for _ in 1..hops {
        let (step, next) = color_hop(sum, &d, reversed);
        fan *= step;
        d = next;
    }
    (fan.max(1e-6), Some((rid, d)))
}

/// Probability an edge exists between two already bound endpoints.
fn into_prob(e: &ExpandOp, query: &BoundQuery, schema: &Schema) -> f64 {
    let mut p = 0.0;
    for rid in &query.variables[e.rel].rel_tables {
        let Some(rd) = schema.rel_by_id(*rid) else {
            continue;
        };
        let pairs = node_count(schema, rd.from) * node_count(schema, rd.to);
        p += rd.edge_count as f64 / pairs.max(1.0);
    }
    p.clamp(1e-9, 1.0)
}

/// The order-preserving byte key of a literal, the form column
/// statistics store their values in. Anything that is not a string or
/// an integer has no key and falls back to the fixed selectivities.
fn literal_key(value: &Literal) -> Option<Vec<u8>> {
    match value {
        Literal::Str(s) => Some(s.as_bytes().to_vec()),
        Literal::Int(i) => Some(zu_common::int_key(*i).to_vec()),
        _ => None,
    }
}

/// The column statistics behind `slot.key`, weighted across the node
/// tables the slot can be. A slot bound to two labels reads as one
/// column: the rows add up and each table's answer counts for as many
/// rows as it has, which is what a scan over both would see.
fn col_selectivity(
    slot: usize,
    key: &str,
    query: &BoundQuery,
    schema: &Schema,
    of: impl Fn(&ColStats) -> f64,
) -> Option<f64> {
    let mut rows = 0.0;
    let mut matched = 0.0;
    let mut found = false;
    for table in &query.variables[slot].node_tables {
        let Some(stats) = schema.col_stats(*table, key) else {
            continue;
        };
        found = true;
        rows += stats.rows as f64;
        matched += of(stats) * stats.rows as f64;
    }
    if !found || rows <= 0.0 {
        return None;
    }
    Some((matched / rows).clamp(0.0, 1.0))
}

/// Selectivity of one filter (perf/12 §2.1). Equality reads the top
/// value frequencies when the query names a literal and the distinct
/// count when it names a parameter, ranges read the equi-depth
/// buckets, and anything a statistic does not cover keeps the fixed
/// guesses this used to be made of. Equality on `id` stays a point
/// lookup off the table count, because the key index is exact and no
/// sample beats it.
fn selectivity(filter: &BoundExpr, query: &BoundQuery, schema: &Schema) -> f64 {
    if let BoundExpr::Binary { op, lhs, rhs } = filter {
        let sides = [(lhs, rhs), (rhs, lhs)];
        match op {
            BinaryOp::Eq => {
                for (side, other) in sides {
                    let BoundExpr::Property { base, key } = side.as_ref() else {
                        continue;
                    };
                    let BoundExpr::Var(slot) = base.as_ref() else {
                        continue;
                    };
                    let known = match other.as_ref() {
                        BoundExpr::Literal(v) => literal_key(v),
                        BoundExpr::Param(_) => None,
                        _ => continue,
                    };
                    if key == "id" && !query.variables[*slot].node_tables.is_empty() {
                        return 1.0 / slot_card(*slot, query, schema);
                    }
                    let got = match &known {
                        Some(k) => {
                            col_selectivity(*slot, key, query, schema, |s| s.eq_selectivity(k))
                        }
                        None => col_selectivity(*slot, key, query, schema, ColStats::eq_average),
                    };
                    return got.unwrap_or(0.1);
                }
                return 0.1;
            }
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                for (side, other) in sides {
                    let BoundExpr::Property { base, key } = side.as_ref() else {
                        continue;
                    };
                    let BoundExpr::Var(slot) = base.as_ref() else {
                        continue;
                    };
                    let BoundExpr::Literal(v) = other.as_ref() else {
                        continue;
                    };
                    let Some(k) = literal_key(v) else { continue };
                    // `side` is the column, so the comparison reads in
                    // the direction the operator was written when the
                    // column is on the left and the other way when the
                    // literal is.
                    let below = col_selectivity(*slot, key, query, schema, |s| {
                        s.below(&k).unwrap_or(f64::NAN)
                    });
                    let Some(below) = below.filter(|b| b.is_finite()) else {
                        continue;
                    };
                    let column_left = std::ptr::eq(side.as_ref(), lhs.as_ref());
                    let lower = matches!(op, BinaryOp::Lt | BinaryOp::Le) == column_left;
                    return if lower { below } else { 1.0 - below };
                }
                return 0.3;
            }
            _ => {}
        }
    }
    if matches!(filter, BoundExpr::IsNull { .. }) {
        return 0.1;
    }
    0.5
}

/// Whether `filter` pins `slot` to one key: equality between the
/// slot's `id` property and a literal or parameter.
fn key_point(filter: &BoundExpr, slot: usize, query: &BoundQuery) -> bool {
    let BoundExpr::Binary {
        op: BinaryOp::Eq,
        lhs,
        rhs,
    } = filter
    else {
        return false;
    };
    [(lhs, rhs), (rhs, lhs)].into_iter().any(|(side, other)| {
        matches!(side.as_ref(), BoundExpr::Property { base, key }
            if key == "id"
                && !query.variables[slot].node_tables.is_empty()
                && matches!(base.as_ref(), BoundExpr::Var(s) if *s == slot))
            && matches!(other.as_ref(), BoundExpr::Param(_) | BoundExpr::Literal(_))
    })
}

/// Ceiling on the largest degree the histogram admits: bucket `i`
/// holds degrees below `2^(i+1)`. Zero when the table has no edges.
fn hist_dmax(hist: &[u64]) -> f64 {
    hist.iter()
        .rposition(|c| *c > 0)
        .map_or(0.0, |top| 2f64.powi(top as i32 + 1) - 1.0)
}

/// Ceiling on the rows one expand step from `source` produces, given
/// `rows` arriving and whether their values in the source slot are
/// known to be all different (perf/12 §2.3). None disables the caps for
/// the whole order: a table with no statistics has no usable ceiling
/// and an unbounded var-length step has none at all.
///
/// `rows` rows over distinct keys can only reach the `rows`
/// highest-degree nodes once each, which is what the lp norms bound
/// and why a full scan expanded over every edge is capped at the edge
/// count exactly. Once the keys can repeat, the only thing left to say
/// is that every row takes the worst node in the table.
fn step_bound(
    e: &ExpandOp,
    source: usize,
    rows: f64,
    distinct: bool,
    query: &BoundQuery,
    schema: &Schema,
) -> Option<f64> {
    let hops = match e.range {
        None => 1,
        Some(v) => v.max?.clamp(1, 8) as i32,
    };
    // Per rel table and direction, the ceiling on one hop's output and
    // the per-row worst case the hops after it multiply by.
    let mut first = 0.0f64;
    let mut per_row = 0.0f64;
    for rid in &query.variables[e.rel].rel_tables {
        let hists = schema.degree_hist(*rid);
        let norms = schema.degree_norm(*rid);
        if hists.is_none() && norms.is_none() {
            return None;
        }
        for d in walk_sides(e, source) {
            let (top, worst) = match norms.map(|n| n.side(*d)) {
                Some(n) => (n.top_sum(rows), n.linf),
                // Histograms only: the top bucket's ceiling per row,
                // and no way to tell the distinct case apart.
                None => {
                    let dmax = hist_dmax(&hists.expect("checked above")[*d]);
                    (rows * dmax, dmax)
                }
            };
            first += if distinct { top } else { rows * worst };
            per_row += worst;
        }
    }
    // Only the first hop starts from keys that may be distinct; a
    // longer walk revisits nodes and every hop after it is worst case.
    Some(first * per_row.powi(hops - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{self, NodeDef, RelDef};
    use crate::parser::parse;
    use crate::plan;

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

    fn optimized(source: &str) -> String {
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let opt = optimize(built, &query, &schema).expect("optimize");
        plan::explain(&opt, &query, &schema)
    }

    fn lines(text: &str) -> Vec<&str> {
        text.lines().map(str::trim_start).collect()
    }

    #[test]
    fn point_lookup_moves_to_the_filtered_side() {
        // Written order scans a and filters b afterwards; the id filter
        // makes b the one-row side, so the plan starts there.
        let text = optimized("MATCH (a:Person)-[:KNOWS]->(b:Person {id: $x}) RETURN a.id AS id");
        assert_eq!(
            lines(&text),
            [
                "Project a.id AS id",
                "Expand (b)<-[#1:KNOWS]-(a)",
                "Filter b.id = $x",
                "ScanNodes b: Person",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn two_hop_expands_the_cheap_side_first() {
        // From the pinned middle node, IS_LOCATED_IN fans out to one
        // place per person while KNOWS fans in twenty wide, so the
        // location expand runs first.
        let text = optimized(
            "MATCH (a:Person)-[:KNOWS]->(m:Person {id: $x})-[:IS_LOCATED_IN]->(c) \
             RETURN a.id AS id, c.id AS place",
        );
        assert_eq!(
            lines(&text),
            [
                "Project a.id AS id, c.id AS place",
                "Expand (m)<-[#1:KNOWS]-(a)",
                "Expand (m)-[#3:IS_LOCATED_IN]->(c)",
                "Filter m.id = $x",
                "ScanNodes m: Person",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn conjunctions_split_to_their_earliest_point() {
        let text = optimized(
            "MATCH (a:Person)-[:KNOWS]->(b) WHERE a.id = $x AND b.id > 5 RETURN b.id AS id",
        );
        assert_eq!(
            lines(&text),
            [
                "Project b.id AS id",
                "Filter b.id > 5",
                "Expand (a)-[#1:KNOWS]->(b)",
                "Filter a.id = $x",
                "ScanNodes a: Person",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn triangles_still_close_with_expand_into() {
        let text = optimized(
            "MATCH (a:Person {id: $x})-[r1:KNOWS]->(b)-[r2:KNOWS]->(c), (a)-[r3:KNOWS]->(c) \
             RETURN a.id AS id",
        );
        assert_eq!(text.matches("ScanNodes").count(), 1, "got:\n{text}");
        assert_eq!(text.matches("ExpandInto").count(), 1, "got:\n{text}");
    }

    /// Collects `(into, wcoj)` per expand, bottom-up.
    fn expand_marks(source: &str) -> Vec<(bool, bool)> {
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let mut plan = &optimize(built, &query, &schema).expect("optimize");
        let mut marks = Vec::new();
        loop {
            match plan {
                LogicalPlan::Empty => break,
                LogicalPlan::Expand {
                    input, into, wcoj, ..
                } => {
                    marks.push((*into, *wcoj));
                    plan = input;
                }
                LogicalPlan::ScanNodes { input, .. }
                | LogicalPlan::Filter { input, .. }
                | LogicalPlan::Unwind { input, .. }
                | LogicalPlan::TableFunction { input, .. }
                | LogicalPlan::Project { input, .. }
                | LogicalPlan::Aggregate { input, .. }
                | LogicalPlan::Distinct { input }
                | LogicalPlan::Sort { input, .. }
                | LogicalPlan::Skip { input, .. }
                | LogicalPlan::Limit { input, .. } => plan = input,
            }
        }
        marks.reverse();
        marks
    }

    #[test]
    fn hist_fanout_averages_over_edge_holding_sources_only() {
        assert_eq!(hist_fanout(600.0, &[0, 0, 300]), Some(2.0));
        assert_eq!(hist_fanout(180_000.0, &[100, 0, 200]), Some(600.0));
        assert_eq!(hist_fanout(0.0, &[]), None, "no stats, fall back");
    }

    #[test]
    fn skewed_histograms_raise_the_estimate_and_flip_the_close_to_asp() {
        // Seeded triangle: the count ratio says fan-out 20, estimate
        // 400 probes, far under the 180 K edge sweep, so the close
        // probes storage. The histogram reveals 300 sources holding
        // all the edges in either direction, fan-out 600 whichever way
        // the DP walks, estimate 360 K, and the same close upgrades to
        // the hash join.
        let source = "MATCH (a:Person {id: $x})-[:KNOWS]->(b)-[:KNOWS]->(c), \
                      (a)-[:KNOWS]->(c) RETURN a.id AS id";
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let text = plan::explain(
            &optimize(built.clone(), &query, &schema).expect("optimize"),
            &query,
            &schema,
        );
        assert_eq!(text.matches("ExpandInto").count(), 1, "got:\n{text}");
        let mut skewed = schema.clone();
        skewed.set_degree_hists(
            [(2u32, {
                let skew = vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 300];
                [skew.clone(), skew]
            })]
            .into_iter()
            .collect(),
        );
        let text = plan::explain(
            &optimize(built, &query, &skewed).expect("optimize"),
            &query,
            &skewed,
        );
        assert_eq!(text.matches("AspJoin").count(), 1, "got:\n{text}");
    }

    #[test]
    fn hist_dmax_reads_the_top_bucket_ceiling() {
        assert_eq!(hist_dmax(&[0, 0, 300]), 7.0, "bucket 2 holds up to 7");
        assert_eq!(hist_dmax(&[5]), 1.0);
        assert_eq!(hist_dmax(&[]), 0.0, "no edges, no fan-out");
    }

    #[test]
    fn mild_skew_keeps_the_estimated_order() {
        // Placeholder hists: 1400 places holding 4 to 7 people each.
        // The backward walk is cheaper on average and its worst case
        // sits within a factor of the estimate, so the plan starts
        // from the smaller place table.
        let source = "MATCH (a:Person)-[:IS_LOCATED_IN]->(c:Place) RETURN a.id AS id";
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let mut mild = schema.clone();
        mild.set_degree_hists(
            [(3u32, [vec![9000], vec![0, 0, 1400]])]
                .into_iter()
                .collect(),
        );
        let text = plan::explain(
            &optimize(built, &query, &mild).expect("optimize"),
            &query,
            &mild,
        );
        assert!(text.contains("ScanNodes c: Place"), "got:\n{text}");
    }

    #[test]
    fn a_catastrophic_ceiling_falls_back_to_the_bound_optimal_order() {
        // Same mean fan-out as the mild case, but one place holds
        // nearly every person: the backward walk still estimates 6.4
        // per row while its ceiling is 8191, past the 100x
        // disagreement, so the DP reruns on the ceilings and starts
        // from the person side whose worst case is one place each.
        let source = "MATCH (a:Person)-[:IS_LOCATED_IN]->(c:Place) RETURN a.id AS id";
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let mut skewed = schema.clone();
        let mut in_hist = vec![0u64; 13];
        in_hist[0] = 1399;
        in_hist[12] = 1;
        skewed.set_degree_hists([(3u32, [vec![9000], in_hist])].into_iter().collect());
        let text = plan::explain(
            &optimize(built, &query, &skewed).expect("optimize"),
            &query,
            &skewed,
        );
        assert!(text.contains("ScanNodes a: Person"), "got:\n{text}");
    }

    #[test]
    fn the_dual_run_threshold_is_a_knob_and_says_when_it_fires() {
        // The one hub place above clears the default 100x, so the
        // order comes off the ceilings and the note says so. Winding
        // the threshold past the ratio the two are actually apart
        // hands the same query back to the estimates.
        let source = "MATCH (a:Person)-[:IS_LOCATED_IN]->(c:Place) RETURN a.id AS id";
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let mut skewed = schema.clone();
        let mut in_hist = vec![0u64; 13];
        in_hist[0] = 1399;
        in_hist[12] = 1;
        skewed.set_degree_hists([(3u32, [vec![9000], in_hist])].into_iter().collect());

        let (plan, notes) = optimize_noted(built.clone(), &query, &skewed).expect("optimize");
        assert_eq!(notes.len(), 1, "got {notes:?}");
        assert!(notes[0].contains("bound disagreement"), "got {notes:?}");
        assert!(notes[0].contains("100x threshold"), "got {notes:?}");
        assert!(
            plan::explain(&plan, &query, &skewed).contains("ScanNodes a: Person"),
            "the default threshold takes the robust order"
        );

        let mut trusting = skewed.clone();
        trusting.set_bound_disagreement(1.0e9);
        let (plan, notes) = optimize_noted(built, &query, &trusting).expect("optimize");
        assert!(notes.is_empty(), "nothing fired, got {notes:?}");
        assert!(
            plan::explain(&plan, &query, &trusting).contains("ScanNodes c: Place"),
            "a trusting threshold leaves the estimated order alone"
        );

        // A threshold at or under one would hand every order to the
        // ceilings, so it is refused and the last good value stands.
        trusting.set_bound_disagreement(0.5);
        trusting.set_bound_disagreement(f64::NAN);
        assert_eq!(trusting.bound_disagreement(), 1.0e9);
    }

    #[test]
    fn direction_specific_histograms_steer_the_scan_side() {
        // All nine thousand location edges land on 15 hub places, so
        // walking backwards from a place multiplies by 600 while
        // walking forwards from a person stays at one. The estimator
        // reads the two directions separately and the DP starts from
        // the person side; a swapped or ignored histogram starts from
        // the smaller place table instead.
        let source = "MATCH (p:Person)-[:IS_LOCATED_IN]->(c:Place) RETURN p.id AS id";
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let mut hubs = schema.clone();
        hubs.set_degree_hists(
            [(3u32, [vec![9000], vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 15]])]
                .into_iter()
                .collect(),
        );
        let text = plan::explain(
            &optimize(built, &query, &hubs).expect("optimize"),
            &query,
            &hubs,
        );
        assert!(text.contains("ScanNodes p: Person"), "got:\n{text}");
    }

    #[test]
    fn color_hops_condition_on_the_frontier_and_dead_walks_floor() {
        // Ten hubs (color 0) hold a thousand edges into the spokes
        // (color 1), which hold none.
        let sum = ColorSummary {
            counts: vec![10, 990],
            triples: vec![(0, 1, 1000, 100)],
        };
        let (fan, dist) = color_hop(&sum, &[1.0, 0.0], false);
        assert_eq!((fan, dist), (100.0, vec![0.0, 1.0]));
        let (dead, uniform) = color_hop(&sum, &[0.0, 1.0], false);
        assert_eq!(dead, 1e-6, "no edge continues from the spokes");
        assert_eq!(uniform, [0.01, 0.99]);
        let (back, _) = color_hop(&sum, &[0.0, 1.0], true);
        assert!((back - 1000.0 / 990.0).abs() < 1e-9, "got {back}");
        assert_eq!(color_seed(&sum, false), Some(vec![0.0, 1.0]));
        assert_eq!(color_seed(&sum, true), Some(vec![1.0, 0.0]));
    }

    #[test]
    fn distributions_from_another_rel_table_do_not_condition_a_hop() {
        // A frontier known to sit on the hundred KNOWS hubs expands by
        // their mean, 1800 wide. The same vector tagged with another
        // rel table indexes a different color space, so the estimate
        // must ignore it and reseed from the global mean instead.
        let source = "MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a.id AS id";
        let mut schema = schema();
        schema.set_color_summaries(
            [(
                2u32,
                ColorSummary {
                    counts: vec![100, 8900],
                    triples: vec![(0, 1, 180_000, 1800)],
                },
            )]
            .into_iter()
            .collect(),
        );
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let e = ExpandOp {
            rel: 1,
            from: 0,
            to: 2,
            direction: RelDirection::Out,
            range: None,
            into: false,
        };
        assert_eq!(query.variables[e.rel].rel_tables, [2], "slot 1 is r");
        let hub = (2u32, vec![1.0, 0.0]);
        let (fan, _) = expand_estimate(&e, 0, Spread::Flat, Some(&hub), &query, &schema);
        assert_eq!(fan, 1800.0);
        let foreign = (3u32, vec![1.0, 0.0]);
        let (fan, _) = expand_estimate(&e, 0, Spread::Flat, Some(&foreign), &query, &schema);
        assert_eq!(fan, 20.0, "foreign space falls back to the mean");
    }

    #[test]
    fn a_dead_color_walk_reorders_the_var_length_chain() {
        // Without colors two KNOWS hops estimate 400 wide, so the plan
        // runs the location hop first. The summary says every KNOWS
        // edge lands on a node holding none, the two-hop walk dies, and
        // the chain becomes the cheap side to run first.
        let source = "MATCH (a:Person)-[:KNOWS*2..2]->(c:Person), \
                      (a)-[:IS_LOCATED_IN]->(p:Place) RETURN a.id AS id";
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let text = plan::explain(
            &optimize(built.clone(), &query, &schema).expect("optimize"),
            &query,
            &schema,
        );
        assert!(
            text.find("KNOWS") < text.find("IS_LOCATED_IN"),
            "location first without colors, got:\n{text}"
        );
        let mut colored = schema.clone();
        colored.set_color_summaries(
            [(
                2u32,
                ColorSummary {
                    counts: vec![100, 8900],
                    triples: vec![(0, 1, 180_000, 1800)],
                },
            )]
            .into_iter()
            .collect(),
        );
        let text = plan::explain(
            &optimize(built, &query, &colored).expect("optimize"),
            &query,
            &colored,
        );
        assert!(
            text.find("IS_LOCATED_IN") < text.find("KNOWS"),
            "dead two-hop walk first with colors, got:\n{text}"
        );
        assert!(!text.contains("ScanNodes p"), "got:\n{text}");
    }

    #[test]
    fn a_collapsed_color_walk_keeps_the_close_an_expand_into() {
        // Unseeded, the means say the probe side carries far more rows
        // than one edge sweep and the close upgrades to the hash join.
        // The summary says every KNOWS edge lands on a node holding
        // none, so almost no two-path survives to probe and the upgrade
        // would pay an accumulate sweep for nothing.
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                      RETURN a.id AS id";
        let schema = schema();
        let query = binder::bind(&parse(source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let text = plan::explain(
            &optimize(built.clone(), &query, &schema).expect("optimize"),
            &query,
            &schema,
        );
        assert_eq!(text.matches("AspJoin").count(), 1, "got:\n{text}");
        let mut colored = schema.clone();
        colored.set_color_summaries(
            [(
                2u32,
                ColorSummary {
                    counts: vec![10, 8990],
                    triples: vec![(0, 1, 180_000, 18_000)],
                },
            )]
            .into_iter()
            .collect(),
        );
        let text = plan::explain(
            &optimize(built, &query, &colored).expect("optimize"),
            &query,
            &colored,
        );
        assert_eq!(text.matches("AspJoin").count(), 0, "got:\n{text}");
        assert_eq!(text.matches("ExpandInto").count(), 1, "got:\n{text}");
    }

    #[test]
    fn cyclic_closes_carry_the_wcoj_mark() {
        // The closing expand is the cycle close, so it alone carries
        // the mark; the two introducing expands stay unmarked.
        let marks = expand_marks(
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
             RETURN a.id AS id",
        );
        assert_eq!(marks, [(false, false), (false, false), (true, true)]);
    }

    #[test]
    fn undirected_closes_stay_unmarked() {
        // Every edge undirected, so whichever edge the DP picks as the
        // close has no single sorted list to gallop and stays unmarked.
        let marks = expand_marks(
            "MATCH (a:Person)-[:KNOWS]-(b)-[:KNOWS]-(c), (a)-[:KNOWS]-(c) \
             RETURN a.id AS id",
        );
        assert!(marks.iter().any(|(into, _)| *into), "got: {marks:?}");
        assert!(marks.iter().all(|(_, wcoj)| !wcoj), "got: {marks:?}");
    }

    #[test]
    fn unseeded_triangles_upgrade_to_asp_join() {
        // No seed, so the probe side is every 2-path in the graph,
        // far more rows than the 180 K edges one accumulate sweep
        // reads: the closing expand becomes the ASP hash join.
        let text = optimized(
            "MATCH (a:Person)-[r1:KNOWS]->(b)-[r2:KNOWS]->(c), (a)-[r3:KNOWS]->(c) \
             RETURN a.id AS id",
        );
        assert_eq!(text.matches("AspJoin").count(), 1, "got:\n{text}");
        assert_eq!(text.matches("ExpandInto").count(), 0, "got:\n{text}");
    }

    #[test]
    fn cross_products_run_the_small_side_first() {
        let text = optimized("MATCH (a:Person), (b:Place) RETURN a, b");
        assert_eq!(
            lines(&text),
            ["Project a, b", "ScanNodes a: Person", "ScanNodes b: Place"],
            "got:\n{text}"
        );
    }

    #[test]
    fn bound_slots_from_with_expand_without_scanning() {
        let text = optimized(
            "MATCH (a:Person {id: $x}) WITH a LIMIT 5 MATCH (a)-[:KNOWS]->(b) RETURN b.id AS id",
        );
        assert_eq!(text.matches("ScanNodes").count(), 1, "got:\n{text}");
        assert!(text.contains("Expand (a)-[#1:KNOWS]->(b)"), "got:\n{text}");
    }

    #[test]
    fn optional_segments_keep_their_written_shape() {
        let text =
            optimized("MATCH (a:Person) OPTIONAL MATCH (a)-[:IS_LOCATED_IN]->(p) RETURN a, p");
        assert_eq!(
            lines(&text),
            [
                "Project a, p",
                "OptionalExpand (a)-[#1:IS_LOCATED_IN]->(p)",
                "ScanNodes a: Person",
            ],
            "got:\n{text}"
        );
    }

    #[test]
    fn oversized_components_keep_written_order() {
        let mut source = String::from("MATCH (n0:Person)");
        for i in 1..=13 {
            source.push_str(&format!("-[:KNOWS]->(n{i})"));
        }
        source.push_str(" RETURN n0.id AS id");
        let schema = schema();
        let query = binder::bind(&parse(&source).expect("parse"), &schema).expect("bind");
        let built = plan::build(&query).expect("build");
        let opt = optimize(built.clone(), &query, &schema).expect("optimize");
        assert_eq!(opt, built);
    }

    #[test]
    fn var_length_orders_from_the_pinned_end() {
        let text =
            optimized("MATCH (a:Person)-[:KNOWS*1..2]->(b:Person {id: $x}) RETURN a.id AS id");
        assert!(
            text.contains("Expand (b)<-[#1:KNOWS*1..2]-(a)"),
            "got:\n{text}"
        );
        assert!(text.contains("ScanNodes b: Person"), "got:\n{text}");
    }
}
