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

use std::collections::{HashMap, HashSet};

use zu_common::Result;

use crate::ast::{BinaryOp, RelDirection};
use crate::binder::{BoundExpr, BoundQuery, Schema};
use crate::plan::{LogicalPlan, VarLength};

/// Components larger than this keep their written join order.
const MAX_DP_RELS: usize = 12;

/// When the pessimistic ceiling exceeds the estimate by this factor,
/// the join order falls back to the ceiling-optimal order (docs/07
/// §6, robustness first).
const BOUND_DISAGREEMENT: f64 = 100.0;

/// Rewrites a built plan with join ordering and filter placement,
/// then marks closing expands: ASP hash joins where the estimates
/// justify the accumulate sweep, and the WCOJ intersection where the
/// close completes a cycle the fusion can take.
pub fn optimize(plan: LogicalPlan, query: &BoundQuery, schema: &Schema) -> Result<LogicalPlan> {
    let plan = rewrite(plan, query, schema)?;
    Ok(mark_asp(plan, query, schema).0)
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
    match plan {
        LogicalPlan::Empty => (LogicalPlan::Empty, 1.0),
        LogicalPlan::ScanNodes {
            input,
            slot,
            optional,
        } => {
            let (input, est) = mark_asp(*input, query, schema);
            let est = est * slot_card(slot, query, schema);
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
            let (input, est) = mark_asp(*input, query, schema);
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
                (asp, wcoj, est * into_prob(&e, query, schema))
            } else {
                (false, false, est * degree(&e, from, query, schema))
            };
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
            let (input, est) = mark_asp(*input, query, schema);
            let est = (est * selectivity(&expr, query, schema)).max(1e-6);
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
            let (input, est) = mark_asp(*input, query, schema);
            (
                LogicalPlan::Unwind {
                    input: Box::new(input),
                    expr,
                    slot,
                },
                est * 10.0,
            )
        }
        LogicalPlan::Project { input, items } => {
            let (input, est) = mark_asp(*input, query, schema);
            (
                LogicalPlan::Project {
                    input: Box::new(input),
                    items,
                },
                est,
            )
        }
        LogicalPlan::Aggregate { input, keys, aggs } => {
            let (input, _) = mark_asp(*input, query, schema);
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
            let (input, est) = mark_asp(*input, query, schema);
            (
                LogicalPlan::Distinct {
                    input: Box::new(input),
                },
                est,
            )
        }
        LogicalPlan::Sort { input, keys } => {
            let (input, est) = mark_asp(*input, query, schema);
            (
                LogicalPlan::Sort {
                    input: Box::new(input),
                    keys,
                },
                est,
            )
        }
        LogicalPlan::Skip { input, expr } => {
            let (input, est) = mark_asp(*input, query, schema);
            (
                LogicalPlan::Skip {
                    input: Box::new(input),
                    expr,
                },
                est,
            )
        }
        LogicalPlan::Limit { input, expr } => {
            let (input, est) = mark_asp(*input, query, schema);
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

fn rewrite(plan: LogicalPlan, query: &BoundQuery, schema: &Schema) -> Result<LogicalPlan> {
    if matches!(
        &plan,
        LogicalPlan::Filter { optional: None, .. }
            | LogicalPlan::ScanNodes { optional: None, .. }
            | LogicalPlan::Expand { optional: None, .. }
    ) {
        return reorder_run(plan, query, schema);
    }
    match plan {
        LogicalPlan::Empty => Ok(LogicalPlan::Empty),
        LogicalPlan::Filter {
            input,
            expr,
            optional,
        } => Ok(LogicalPlan::Filter {
            input: Box::new(rewrite(*input, query, schema)?),
            expr,
            optional,
        }),
        LogicalPlan::ScanNodes {
            input,
            slot,
            optional,
        } => Ok(LogicalPlan::ScanNodes {
            input: Box::new(rewrite(*input, query, schema)?),
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
            input: Box::new(rewrite(*input, query, schema)?),
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
            input: Box::new(rewrite(*input, query, schema)?),
            expr,
            slot,
        }),
        LogicalPlan::Project { input, items } => Ok(LogicalPlan::Project {
            input: Box::new(rewrite(*input, query, schema)?),
            items,
        }),
        LogicalPlan::Aggregate { input, keys, aggs } => Ok(LogicalPlan::Aggregate {
            input: Box::new(rewrite(*input, query, schema)?),
            keys,
            aggs,
        }),
        LogicalPlan::Distinct { input } => Ok(LogicalPlan::Distinct {
            input: Box::new(rewrite(*input, query, schema)?),
        }),
        LogicalPlan::Sort { input, keys } => Ok(LogicalPlan::Sort {
            input: Box::new(rewrite(*input, query, schema)?),
            keys,
        }),
        LogicalPlan::Skip { input, expr } => Ok(LogicalPlan::Skip {
            input: Box::new(rewrite(*input, query, schema)?),
            expr,
        }),
        LogicalPlan::Limit { input, expr } => Ok(LogicalPlan::Limit {
            input: Box::new(rewrite(*input, query, schema)?),
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

fn reorder_run(top: LogicalPlan, query: &BoundQuery, schema: &Schema) -> Result<LogicalPlan> {
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
    let below = rewrite(rest, query, schema)?;
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
        let (steps, card) = order_component(comp, &expands, &filters, &b0, query, schema);
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
/// step order and its estimated output cardinality.
///
/// Every step also carries a pessimistic row ceiling from the degree
/// histograms (docs/07 §6): the worst degree any single row can
/// multiply by. The ceiling clamps the estimate, and when the summed
/// ceilings exceed the summed estimates by [`BOUND_DISAGREEMENT`] the
/// DP reruns minimizing the ceiling, with the estimate only breaking
/// near ties, so a skew-blind guess cannot pick an order whose worst
/// case is catastrophic. Steps without histograms have no usable
/// ceiling and disable the caps for their order.
fn order_component(
    comp: &Component,
    expands: &[ExpandOp],
    filters: &[BoundExpr],
    b0: &HashSet<usize>,
    query: &BoundQuery,
    schema: &Schema,
) -> (Vec<Step>, f64) {
    #[derive(Clone)]
    struct Entry {
        cost: f64,
        card: f64,
        /// Pessimistic ceiling on `card`, None once any step lacks one.
        bnd: Option<f64>,
        /// Running sum of the ceilings, the bound analog of `cost`.
        bcost: f64,
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
                    bcost: 0.0,
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
                        bcost: bnd,
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
                    let (step, factor, bfactor) = if from_bound && to_bound {
                        (
                            Step::Expand {
                                ix: *rel,
                                from: e.from,
                                to: e.to,
                                into: true,
                            },
                            into_prob(e, query, schema),
                            // A close keeps or drops rows, never adds.
                            Some(1.0),
                        )
                    } else if from_bound {
                        (
                            Step::Expand {
                                ix: *rel,
                                from: e.from,
                                to: e.to,
                                into: false,
                            },
                            degree(e, e.from, query, schema),
                            step_bound(e, e.from, query, schema),
                        )
                    } else {
                        (
                            Step::Expand {
                                ix: *rel,
                                from: e.to,
                                to: e.from,
                                into: false,
                            },
                            degree(e, e.to, query, schema),
                            step_bound(e, e.to, query, schema),
                        )
                    };
                    let next = mask | (1 << i);
                    let grown = base_bound(next);
                    let bnd = entry.bnd.zip(bfactor).map(|(b, f)| (b * f).max(1e-6));
                    let card = (entry.card * factor * newly(&bound, &grown)).max(1e-6);
                    let card = bnd.map_or(card, |b| card.min(b));
                    let cand = Entry {
                        cost: entry.cost + card,
                        card,
                        bnd,
                        bcost: entry.bcost + bnd.unwrap_or(0.0),
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
    let best = if best.bnd.is_some() && best.bcost > BOUND_DISAGREEMENT * best.cost {
        run(true).unwrap_or(best)
    } else {
        best
    };
    (best.steps, best.card)
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
fn degree(e: &ExpandOp, source: usize, query: &BoundQuery, schema: &Schema) -> f64 {
    let reversed = source == e.to && source != e.from;
    let mut deg = 0.0;
    for rid in &query.variables[e.rel].rel_tables {
        let Some(rd) = schema.rel_by_id(*rid) else {
            continue;
        };
        let edges = rd.edge_count as f64;
        let from_cnt = node_count(schema, rd.from);
        let to_cnt = node_count(schema, rd.to);
        let hists = schema.degree_hist(*rid);
        let fwd = hists
            .and_then(|[out, _]| hist_fanout(edges, out))
            .unwrap_or(edges / from_cnt);
        let bwd = hists
            .and_then(|[_, inn]| hist_fanout(edges, inn))
            .unwrap_or(edges / to_cnt);
        deg += match e.direction {
            RelDirection::Out => {
                if reversed {
                    bwd
                } else {
                    fwd
                }
            }
            RelDirection::In => {
                if reversed {
                    fwd
                } else {
                    bwd
                }
            }
            RelDirection::Undirected => fwd + bwd,
        };
    }
    let hops = e.range.map_or(1, |v| v.min.unwrap_or(1).clamp(1, 8)) as i32;
    deg.max(1e-6).powi(hops)
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

/// Fixed selectivities until the column catalog carries statistics.
/// Equality on `id` against a literal or parameter is a point lookup.
fn selectivity(filter: &BoundExpr, query: &BoundQuery, schema: &Schema) -> f64 {
    if let BoundExpr::Binary { op, lhs, rhs } = filter {
        match op {
            BinaryOp::Eq => {
                for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                    let BoundExpr::Property { base, key } = side.as_ref() else {
                        continue;
                    };
                    let BoundExpr::Var(slot) = base.as_ref() else {
                        continue;
                    };
                    if !matches!(other.as_ref(), BoundExpr::Param(_) | BoundExpr::Literal(_)) {
                        continue;
                    }
                    if key == "id" && !query.variables[*slot].node_tables.is_empty() {
                        return 1.0 / slot_card(*slot, query, schema);
                    }
                    return 0.1;
                }
                return 0.1;
            }
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => return 0.3,
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

/// Worst-case fan-out of one expand step from `source`: the largest
/// degree any single row can multiply by, summed over the candidate
/// rel tables. None disables the caps for the order: a table without
/// histograms has no usable ceiling and an unbounded var-length step
/// has none at all.
fn step_bound(e: &ExpandOp, source: usize, query: &BoundQuery, schema: &Schema) -> Option<f64> {
    let hops = match e.range {
        None => 1,
        Some(v) => v.max?.clamp(1, 8) as i32,
    };
    let reversed = source == e.to && source != e.from;
    let mut worst = 0.0;
    for rid in &query.variables[e.rel].rel_tables {
        let [out, inn] = schema.degree_hist(*rid)?;
        let (fwd, bwd) = (hist_dmax(out), hist_dmax(inn));
        worst += match e.direction {
            RelDirection::Out => {
                if reversed {
                    bwd
                } else {
                    fwd
                }
            }
            RelDirection::In => {
                if reversed {
                    fwd
                } else {
                    bwd
                }
            }
            RelDirection::Undirected => fwd + bwd,
        };
    }
    Some(worst.powi(hops))
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
