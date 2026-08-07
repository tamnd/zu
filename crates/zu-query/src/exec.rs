//! Factorized vectorized execution over an engine-agnostic `Graph`.
//!
//! The logical plan compiles into stages split at every projection or
//! aggregation. Within a stage, operators form a pull pipeline over
//! `Chunk`s: a scan or expand fills a vector of up to `VECTOR_SIZE`
//! values, and downstream operators either iterate it lazily (the
//! factorized representation) or pin one position at a time after an
//! explicit `Flatten`. An expand consumes a flat source position and
//! produces the whole neighbor list as one unflat vector, so a two-hop
//! pattern holds its intermediate result as nested lists instead of a
//! flattened cross product.
//!
//! The sink at the top of each stage materializes rows. A plain
//! projection walks the cartesian product of the unflat vectors, which
//! is the one place flattening is forced. An aggregation instead uses
//! the factorized shortcut: `count` and `sum` over one unflat vector
//! multiply by the sizes of the others without ever enumerating the
//! product, which is what makes a two-hop count linear in the edges
//! touched. `Options { flat: true }` inserts a `Flatten` after every
//! producer so the same queries run tuple-at-a-time, as a differential
//! oracle for the factorized paths.
//!
//! A `Filter` directly above a scan on an `id` equality becomes an
//! `IndexLookup` that jumps to the offset. This leans on the v0
//! contract that the `id` property of a node equals its offset; keyed
//! tables get their own lookup path when the column catalog lands.
//!
//! Variable-length patterns and OPTIONAL MATCH parse and plan but do
//! not execute yet; both return a clear error here rather than a wrong
//! answer.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use zu_common::{Result, ZuError};

use crate::ast::{BinaryOp, Literal, RelDirection, UnaryOp};
use crate::binder::{BoundExpr, BoundItem, BoundQuery, Func, Schema};
use crate::plan::LogicalPlan;

/// Vector width of one chunk fill.
pub const VECTOR_SIZE: usize = 2048;

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// One runtime value. Nodes and rels carry their table so multi-table
/// slots stay unambiguous.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Node { table: u32, offset: u64 },
    Rel { table: u32, src: u64, dst: u64 },
    List(Vec<Value>),
}

/// The rows a query returns, one column name per RETURN item.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// What the executor needs from a storage engine. Methods take
/// `&mut self` because readers cache decoded state.
pub trait Graph {
    /// Replaces `out` with the neighbor list of `node` in rel table
    /// `rel`: destinations when `reversed` is false, sources when true.
    fn neighbors(&mut self, rel: u32, node: u64, reversed: bool, out: &mut Vec<u64>) -> Result<()>;
    /// Edge probe in storage orientation: does `src` point at `dst`?
    fn has_edge(&mut self, rel: u32, src: u64, dst: u64) -> Result<bool>;
    /// One property of one node. The v0 contract is that `id` equals
    /// the offset; everything else is up to the engine.
    fn property(&mut self, table: u32, offset: u64, key: &str) -> Result<Value>;
}

/// Execution switches. `flat` forces tuple-at-a-time execution by
/// flattening after every producer, the differential oracle for the
/// factorized paths.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub flat: bool,
}

/// Total order over values for grouping, DISTINCT, and ORDER BY:
/// nulls first, then booleans, numbers (int and float compare
/// numerically), strings, nodes, rels, lists.
#[derive(Debug, Clone, PartialEq)]
pub struct OrdValue(pub Value);

impl Eq for OrdValue {}

impl PartialOrd for OrdValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdValue {
    fn cmp(&self, other: &Self) -> Ordering {
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => 0,
                Value::Bool(_) => 1,
                Value::Int(_) | Value::Float(_) => 2,
                Value::Str(_) => 3,
                Value::Node { .. } => 4,
                Value::Rel { .. } => 5,
                Value::List(_) => 6,
            }
        }
        match (&self.0, &other.0) {
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).total_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.total_cmp(&(*b as f64)),
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Str(a), Value::Str(b)) => a.cmp(b),
            (
                Value::Node {
                    table: t1,
                    offset: o1,
                },
                Value::Node {
                    table: t2,
                    offset: o2,
                },
            ) => (t1, o1).cmp(&(t2, o2)),
            (
                Value::Rel {
                    table: t1,
                    src: s1,
                    dst: d1,
                },
                Value::Rel {
                    table: t2,
                    src: s2,
                    dst: d2,
                },
            ) => (t1, s1, d1).cmp(&(t2, s2, d2)),
            (Value::List(a), Value::List(b)) => {
                for (x, y) in a.iter().zip(b) {
                    let ord = OrdValue(x.clone()).cmp(&OrdValue(y.clone()));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                a.len().cmp(&b.len())
            }
            (a, b) => rank(a).cmp(&rank(b)),
        }
    }
}

// ---------------------------------------------------------------------------
// Physical plan
// ---------------------------------------------------------------------------

/// One rel candidate table with its endpoint tables, for orientation
/// checks at expand time.
#[derive(Debug, Clone)]
struct RelStep {
    id: u32,
    from_table: u32,
    to_table: u32,
}

#[derive(Debug, Clone)]
enum OpDesc {
    /// Yields exactly one empty configuration: the seed under the
    /// first scan of a stage.
    Source,
    /// Feeds the previous stage's rows back in as unflat batches.
    RowSource {
        chunk: usize,
    },
    Scan {
        tables: Vec<u32>,
        chunk: usize,
    },
    /// Fused scan plus `id` equality: jumps straight to the offset.
    IndexLookup {
        tables: Vec<u32>,
        key: BoundExpr,
        chunk: usize,
    },
    /// Pins one position of an unflat chunk per pull: the nested loop
    /// that turns a factorized vector into single configurations.
    Flatten {
        chunk: usize,
    },
    Expand {
        from: usize,
        direction: RelDirection,
        rels: Vec<RelStep>,
        chunk: usize,
    },
    /// Both endpoints bound: an edge probe instead of a list read.
    ExpandInto {
        from: usize,
        to: usize,
        direction: RelDirection,
        rels: Vec<RelStep>,
        chunk: usize,
    },
    /// `compact` names the one unflat chunk this filter may shrink in
    /// place; with `None` every referenced chunk is flat and the
    /// filter just gates configurations.
    Filter {
        expr: BoundExpr,
        compact: Option<usize>,
    },
    Unwind {
        expr: BoundExpr,
        chunk: usize,
    },
}

#[derive(Debug, Clone)]
enum PostOp {
    Distinct,
    Filter(BoundExpr),
    Sort(Vec<(BoundExpr, bool)>),
    Skip(BoundExpr),
    Limit(BoundExpr),
}

/// One aggregate item, restricted in v0 to a bare call.
#[derive(Debug, Clone)]
struct AggSpec {
    func: Func,
    distinct: bool,
    star: bool,
    arg: Option<BoundExpr>,
    /// The one unflat chunk the argument reads, if any; the planner
    /// flattens the rest.
    arg_chunk: Option<usize>,
}

#[derive(Debug, Clone)]
struct SinkDef {
    /// Projection items in clause order; for aggregations this
    /// interleaves keys and aggregates exactly as written.
    items: Vec<BoundItem>,
    aggregate: bool,
    aggs: Vec<AggSpec>,
    post: Vec<PostOp>,
    /// Slots snapshotted per row for post-projection filters.
    extra_slots: Vec<usize>,
}

#[derive(Debug, Clone)]
struct StageDef {
    descs: Vec<OpDesc>,
    /// Slots of each chunk, in column order.
    chunk_slots: Vec<Vec<usize>>,
    slot_loc: BTreeMap<usize, (usize, usize)>,
    /// Chunks still unflat when the sink runs, in creation order.
    unflat: Vec<usize>,
    sink: SinkDef,
}

struct StageBuilder {
    descs: Vec<OpDesc>,
    chunk_slots: Vec<Vec<usize>>,
    chunk_flat: Vec<bool>,
    slot_loc: BTreeMap<usize, (usize, usize)>,
    /// The chunk a filter may still compact: the latest producer's,
    /// invalidated by any flatten between it and the filter.
    compactable: Option<usize>,
    flat: bool,
}

impl StageBuilder {
    fn new(flat: bool) -> Self {
        StageBuilder {
            descs: Vec::new(),
            chunk_slots: Vec::new(),
            chunk_flat: Vec::new(),
            slot_loc: BTreeMap::new(),
            compactable: None,
            flat,
        }
    }

    fn new_chunk(&mut self, slots: Vec<usize>, flat: bool) -> usize {
        let c = self.chunk_slots.len();
        for (col, &slot) in slots.iter().enumerate() {
            self.slot_loc.insert(slot, (c, col));
        }
        self.chunk_slots.push(slots);
        self.chunk_flat.push(flat);
        c
    }

    fn ensure_flat(&mut self, chunk: usize) {
        if !self.chunk_flat[chunk] {
            self.descs.push(OpDesc::Flatten { chunk });
            self.chunk_flat[chunk] = true;
            self.compactable = None;
        }
    }

    /// Called after every producer: in flat mode everything flattens
    /// immediately, otherwise the new chunk becomes the compaction
    /// candidate.
    fn produced(&mut self, chunk: usize) {
        self.compactable = Some(chunk);
        if self.flat {
            self.ensure_flat(chunk);
        }
    }

    /// The unflat chunks an expression reads, in creation order.
    fn unflat_of(&self, expr: &BoundExpr) -> Result<Vec<usize>> {
        let mut slots = BTreeSet::new();
        expr_slots(expr, &mut slots);
        let mut chunks = BTreeSet::new();
        for slot in slots {
            let Some(&(c, _)) = self.slot_loc.get(&slot) else {
                return Err(invalid(format!(
                    "expression references slot {slot} before anything binds it"
                )));
            };
            if !self.chunk_flat[c] {
                chunks.insert(c);
            }
        }
        Ok(chunks.into_iter().collect())
    }
}

fn expr_slots(expr: &BoundExpr, out: &mut BTreeSet<usize>) {
    match expr {
        BoundExpr::Literal(_) | BoundExpr::Param(_) => {}
        BoundExpr::Var(slot) => {
            out.insert(*slot);
        }
        BoundExpr::Property { base, .. } => expr_slots(base, out),
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
        BoundExpr::Map(pairs) => {
            for (_, v) in pairs {
                expr_slots(v, out);
            }
        }
    }
}

fn input_of(plan: &LogicalPlan) -> Option<&LogicalPlan> {
    match plan {
        LogicalPlan::Empty => None,
        LogicalPlan::ScanNodes { input, .. }
        | LogicalPlan::Expand { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Skip { input, .. }
        | LogicalPlan::Limit { input, .. } => Some(input),
    }
}

/// Matches `slot.id = key` or `id(slot) = key` with a slot-free key
/// expression, in either operand order.
fn index_key(expr: &BoundExpr, slot: usize) -> Option<BoundExpr> {
    let BoundExpr::Binary {
        op: BinaryOp::Eq,
        lhs,
        rhs,
    } = expr
    else {
        return None;
    };
    for (side, other) in [(lhs, rhs), (rhs, lhs)] {
        let hits = match side.as_ref() {
            BoundExpr::Property { base, key } => {
                key == "id" && matches!(base.as_ref(), BoundExpr::Var(s) if *s == slot)
            }
            BoundExpr::Call {
                func: Func::Id,
                star: false,
                args,
                ..
            } => {
                matches!(args.as_slice(), [BoundExpr::Var(s)] if *s == slot)
            }
            _ => false,
        };
        if hits {
            let mut slots = BTreeSet::new();
            expr_slots(other, &mut slots);
            if slots.is_empty() {
                return Some(other.as_ref().clone());
            }
        }
    }
    None
}

fn rel_steps(rel_slot: usize, query: &BoundQuery, schema: &Schema) -> Result<Vec<RelStep>> {
    let var = &query.variables[rel_slot];
    let mut steps = Vec::with_capacity(var.rel_tables.len());
    for &id in &var.rel_tables {
        let rd = schema
            .rel_by_id(id)
            .ok_or_else(|| invalid(format!("rel table {id} vanished from the schema")))?;
        steps.push(RelStep {
            id,
            from_table: rd.from,
            to_table: rd.to,
        });
    }
    if steps.is_empty() {
        return Err(invalid(format!(
            "relationship '{}' has no candidate tables",
            var.name
        )));
    }
    Ok(steps)
}

fn build_stages(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    options: &Options,
) -> Result<Vec<StageDef>> {
    let mut linear = Vec::new();
    let mut cur = plan;
    while let Some(input) = input_of(cur) {
        linear.push(cur);
        cur = input;
    }
    linear.reverse();

    let projections: Vec<&Vec<BoundItem>> = query
        .clauses
        .iter()
        .filter_map(|c| match c {
            crate::binder::BoundClause::Project { items, .. } => Some(items),
            _ => None,
        })
        .collect();
    let mut proj_ix = 0;

    let mut stages = Vec::new();
    let mut b = StageBuilder::new(options.flat);
    b.descs.push(OpDesc::Source);

    let mut i = 0;
    while i < linear.len() {
        match linear[i] {
            LogicalPlan::Empty => unreachable!("Empty never appears in the linearized ops"),
            LogicalPlan::ScanNodes { slot, optional, .. } => {
                if *optional {
                    return Err(invalid("OPTIONAL MATCH does not execute yet".into()));
                }
                let tables = query.variables[*slot].node_tables.clone();
                if tables.is_empty() {
                    return Err(invalid(format!(
                        "variable '{}' has no candidate node tables",
                        query.variables[*slot].name
                    )));
                }
                let fused = linear.get(i + 1).and_then(|op| match op {
                    LogicalPlan::Filter { expr, .. } => index_key(expr, *slot),
                    _ => None,
                });
                let chunk = b.new_chunk(vec![*slot], false);
                if let Some(key) = fused {
                    b.descs.push(OpDesc::IndexLookup { tables, key, chunk });
                    i += 1;
                } else {
                    b.descs.push(OpDesc::Scan { tables, chunk });
                }
                b.produced(chunk);
            }
            LogicalPlan::Expand {
                rel,
                from,
                to,
                direction,
                range,
                into,
                optional,
                ..
            } => {
                if *optional {
                    return Err(invalid("OPTIONAL MATCH does not execute yet".into()));
                }
                if range.is_some() {
                    return Err(invalid(
                        "variable-length patterns do not execute yet".into(),
                    ));
                }
                let rels = rel_steps(*rel, query, schema)?;
                let from_chunk = b
                    .slot_loc
                    .get(from)
                    .map(|&(c, _)| c)
                    .ok_or_else(|| invalid(format!("expand from unbound slot {from}")))?;
                b.ensure_flat(from_chunk);
                if *into {
                    let to_chunk = b
                        .slot_loc
                        .get(to)
                        .map(|&(c, _)| c)
                        .ok_or_else(|| invalid(format!("expand into unbound slot {to}")))?;
                    b.ensure_flat(to_chunk);
                    // The probe result is a single edge, born flat.
                    let chunk = b.new_chunk(vec![*rel], true);
                    b.descs.push(OpDesc::ExpandInto {
                        from: *from,
                        to: *to,
                        direction: *direction,
                        rels,
                        chunk,
                    });
                } else {
                    let chunk = b.new_chunk(vec![*to, *rel], false);
                    b.descs.push(OpDesc::Expand {
                        from: *from,
                        direction: *direction,
                        rels,
                        chunk,
                    });
                    b.produced(chunk);
                }
            }
            LogicalPlan::Filter { expr, .. } => {
                let unflat = b.unflat_of(expr)?;
                let compact = match unflat.as_slice() {
                    [c] if b.compactable == Some(*c) => Some(*c),
                    _ => {
                        for &c in &unflat {
                            b.ensure_flat(c);
                        }
                        None
                    }
                };
                b.descs.push(OpDesc::Filter {
                    expr: expr.clone(),
                    compact,
                });
            }
            LogicalPlan::Unwind { expr, slot, .. } => {
                for c in b.unflat_of(expr)? {
                    b.ensure_flat(c);
                }
                let chunk = b.new_chunk(vec![*slot], false);
                b.descs.push(OpDesc::Unwind {
                    expr: expr.clone(),
                    chunk,
                });
                b.produced(chunk);
            }
            LogicalPlan::Project { .. } | LogicalPlan::Aggregate { .. } => {
                let items = projections
                    .get(proj_ix)
                    .ok_or_else(|| invalid("more sinks than projection clauses".into()))?
                    .to_vec();
                proj_ix += 1;
                let aggregate = matches!(linear[i], LogicalPlan::Aggregate { .. });

                let mut aggs = Vec::new();
                if aggregate {
                    // Grouping keys must be flat; each aggregate
                    // argument may keep exactly one unflat vector.
                    for item in items.iter().filter(|it| !it.aggregate) {
                        for c in b.unflat_of(&item.expr)? {
                            b.ensure_flat(c);
                        }
                    }
                    for item in items.iter().filter(|it| it.aggregate) {
                        let BoundExpr::Call {
                            func,
                            distinct,
                            star,
                            args,
                        } = &item.expr
                        else {
                            return Err(invalid(format!(
                                "aggregate item '{}' must be a bare call for now",
                                item.name
                            )));
                        };
                        let arg = args.first().cloned();
                        let arg_chunk = match &arg {
                            None => None,
                            Some(expr) => {
                                let mut unflat = b.unflat_of(expr)?;
                                while unflat.len() > 1 {
                                    b.ensure_flat(unflat.remove(0));
                                }
                                unflat.first().copied()
                            }
                        };
                        aggs.push(AggSpec {
                            func: *func,
                            distinct: *distinct,
                            star: *star,
                            arg,
                            arg_chunk,
                        });
                    }
                }

                let mut post = Vec::new();
                let mut extra = BTreeSet::new();
                while let Some(op) = linear.get(i + 1) {
                    match op {
                        LogicalPlan::Distinct { .. } => post.push(PostOp::Distinct),
                        LogicalPlan::Filter { expr, .. } => {
                            expr_slots(expr, &mut extra);
                            post.push(PostOp::Filter(expr.clone()));
                        }
                        LogicalPlan::Sort { keys, .. } => post.push(PostOp::Sort(keys.clone())),
                        LogicalPlan::Skip { expr, .. } => post.push(PostOp::Skip(expr.clone())),
                        LogicalPlan::Limit { expr, .. } => post.push(PostOp::Limit(expr.clone())),
                        _ => break,
                    }
                    i += 1;
                }

                let unflat = (0..b.chunk_flat.len())
                    .filter(|&c| !b.chunk_flat[c])
                    .collect();
                let sink = SinkDef {
                    items,
                    aggregate,
                    aggs,
                    post,
                    extra_slots: extra.into_iter().collect(),
                };
                stages.push(StageDef {
                    descs: std::mem::take(&mut b.descs),
                    chunk_slots: std::mem::take(&mut b.chunk_slots),
                    slot_loc: std::mem::take(&mut b.slot_loc),
                    unflat,
                    sink,
                });

                if i + 1 < linear.len() {
                    let last = &stages.last().expect("just pushed").sink;
                    let mut slots = Vec::with_capacity(last.items.len());
                    for item in &last.items {
                        slots.push(item.slot.ok_or_else(|| {
                            invalid("WITH item lost its slot, this is a bug".into())
                        })?);
                    }
                    b = StageBuilder::new(options.flat);
                    let chunk = b.new_chunk(slots, false);
                    b.descs.push(OpDesc::RowSource { chunk });
                    b.produced(chunk);
                }
            }
            LogicalPlan::Distinct { .. }
            | LogicalPlan::Sort { .. }
            | LogicalPlan::Skip { .. }
            | LogicalPlan::Limit { .. } => {
                return Err(invalid(
                    "row operator without a projection under it, this is a bug".into(),
                ));
            }
        }
        i += 1;
    }

    if stages.is_empty() {
        return Err(invalid("query has no RETURN, this is a bug".into()));
    }
    Ok(stages)
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Chunk {
    cols: Vec<Vec<Value>>,
    size: usize,
    /// `Some(pos)` pins one position: the chunk reads as a single
    /// value per column. `None` is the unflat state.
    cur: Option<usize>,
}

impl Chunk {
    fn retain(&mut self, keep: &[bool]) {
        for col in &mut self.cols {
            let mut it = keep.iter();
            col.retain(|_| *it.next().expect("keep mask matches size"));
        }
        self.size = keep.iter().filter(|k| **k).count();
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct OpState {
    active: bool,
    pos: usize,
    table_ix: usize,
    offset: u64,
}

struct StageCtx<'a> {
    graph: &'a mut dyn Graph,
    params: &'a [Value],
    counts: &'a BTreeMap<u32, u64>,
    slot_loc: &'a BTreeMap<usize, (usize, usize)>,
    chunks: Vec<Chunk>,
    states: Vec<OpState>,
    rows: Vec<Vec<Value>>,
    /// Row-context values that shadow the chunks: projection aliases
    /// during materialization, the row snapshot during post filters.
    overlay: BTreeMap<usize, Value>,
    scratch: Vec<u64>,
}

fn value_of(ctx: &mut StageCtx, slot: usize) -> Result<Value> {
    if let Some(v) = ctx.overlay.get(&slot) {
        return Ok(v.clone());
    }
    let Some(&(c, col)) = ctx.slot_loc.get(&slot) else {
        return Err(invalid(format!("slot {slot} is not bound in this stage")));
    };
    let chunk = &ctx.chunks[c];
    let Some(pos) = chunk.cur else {
        return Err(invalid(
            "read of an unflattened vector, the planner missed a flatten".into(),
        ));
    };
    Ok(chunk.cols[col][pos].clone())
}

fn node_value(v: Value, what: &str) -> Result<(u32, u64)> {
    match v {
        Value::Node { table, offset } => Ok((table, offset)),
        other => Err(invalid(format!("{what} expects a node, got {other:?}"))),
    }
}

fn next(descs: &[OpDesc], ctx: &mut StageCtx, i: usize) -> Result<bool> {
    match &descs[i] {
        OpDesc::Source => {
            if ctx.states[i].active {
                return Ok(false);
            }
            ctx.states[i].active = true;
            Ok(true)
        }
        OpDesc::RowSource { chunk } => {
            let start = ctx.states[i].pos;
            if start >= ctx.rows.len() {
                return Ok(false);
            }
            let end = (start + VECTOR_SIZE).min(ctx.rows.len());
            let ncols = ctx.chunks[*chunk].cols.len();
            let mut cols = vec![Vec::with_capacity(end - start); ncols];
            for row in &ctx.rows[start..end] {
                for (col, v) in cols.iter_mut().zip(row) {
                    col.push(v.clone());
                }
            }
            let c = &mut ctx.chunks[*chunk];
            c.cols = cols;
            c.size = end - start;
            c.cur = None;
            ctx.states[i].pos = end;
            Ok(true)
        }
        OpDesc::Scan { tables, chunk } => loop {
            if !ctx.states[i].active {
                if !next(descs, ctx, i - 1)? {
                    return Ok(false);
                }
                ctx.states[i] = OpState {
                    active: true,
                    ..OpState::default()
                };
            }
            let mut vals = Vec::with_capacity(VECTOR_SIZE);
            {
                let st = &mut ctx.states[i];
                while vals.len() < VECTOR_SIZE && st.table_ix < tables.len() {
                    let table = tables[st.table_ix];
                    let count = *ctx.counts.get(&table).unwrap_or(&0);
                    if st.offset >= count {
                        st.table_ix += 1;
                        st.offset = 0;
                        continue;
                    }
                    vals.push(Value::Node {
                        table,
                        offset: st.offset,
                    });
                    st.offset += 1;
                }
            }
            if vals.is_empty() {
                ctx.states[i].active = false;
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = vals.len();
            c.cols[0] = vals;
            c.cur = None;
            return Ok(true);
        },
        OpDesc::IndexLookup { tables, key, chunk } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let Value::Int(k) = eval(ctx, key)? else {
                continue;
            };
            let Ok(k) = u64::try_from(k) else {
                continue;
            };
            let mut vals = Vec::with_capacity(tables.len());
            for &table in tables {
                if k < *ctx.counts.get(&table).unwrap_or(&0) {
                    vals.push(Value::Node { table, offset: k });
                }
            }
            if vals.is_empty() {
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = vals.len();
            c.cols[0] = vals;
            c.cur = None;
            return Ok(true);
        },
        OpDesc::Flatten { chunk } => {
            let c = *chunk;
            if ctx.states[i].active {
                let pos = ctx.states[i].pos + 1;
                if pos < ctx.chunks[c].size {
                    ctx.states[i].pos = pos;
                    ctx.chunks[c].cur = Some(pos);
                    return Ok(true);
                }
                ctx.states[i].active = false;
            }
            loop {
                if !next(descs, ctx, i - 1)? {
                    return Ok(false);
                }
                if ctx.chunks[c].size == 0 {
                    continue;
                }
                ctx.states[i] = OpState {
                    active: true,
                    ..OpState::default()
                };
                ctx.chunks[c].cur = Some(0);
                return Ok(true);
            }
        }
        OpDesc::Expand {
            from,
            direction,
            rels,
            chunk,
        } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let (table, offset) = node_value(value_of(ctx, *from)?, "expand")?;
            let mut far = Vec::new();
            let mut rel_vals = Vec::new();
            for step in rels {
                if matches!(direction, RelDirection::Out | RelDirection::Undirected)
                    && table == step.from_table
                {
                    ctx.scratch.clear();
                    let mut scratch = std::mem::take(&mut ctx.scratch);
                    ctx.graph.neighbors(step.id, offset, false, &mut scratch)?;
                    for &dst in &scratch {
                        far.push(Value::Node {
                            table: step.to_table,
                            offset: dst,
                        });
                        rel_vals.push(Value::Rel {
                            table: step.id,
                            src: offset,
                            dst,
                        });
                    }
                    ctx.scratch = scratch;
                }
                if matches!(direction, RelDirection::In | RelDirection::Undirected)
                    && table == step.to_table
                {
                    ctx.scratch.clear();
                    let mut scratch = std::mem::take(&mut ctx.scratch);
                    ctx.graph.neighbors(step.id, offset, true, &mut scratch)?;
                    for &src in &scratch {
                        far.push(Value::Node {
                            table: step.from_table,
                            offset: src,
                        });
                        rel_vals.push(Value::Rel {
                            table: step.id,
                            src,
                            dst: offset,
                        });
                    }
                    ctx.scratch = scratch;
                }
            }
            if far.is_empty() {
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = far.len();
            c.cols[0] = far;
            c.cols[1] = rel_vals;
            c.cur = None;
            return Ok(true);
        },
        OpDesc::ExpandInto {
            from,
            to,
            direction,
            rels,
            chunk,
        } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let (ft, fo) = node_value(value_of(ctx, *from)?, "expand into")?;
            let (tt, to_off) = node_value(value_of(ctx, *to)?, "expand into")?;
            let mut hit = None;
            for step in rels {
                if matches!(direction, RelDirection::Out | RelDirection::Undirected)
                    && ft == step.from_table
                    && tt == step.to_table
                    && ctx.graph.has_edge(step.id, fo, to_off)?
                {
                    hit = Some(Value::Rel {
                        table: step.id,
                        src: fo,
                        dst: to_off,
                    });
                    break;
                }
                if matches!(direction, RelDirection::In | RelDirection::Undirected)
                    && ft == step.to_table
                    && tt == step.from_table
                    && ctx.graph.has_edge(step.id, to_off, fo)?
                {
                    hit = Some(Value::Rel {
                        table: step.id,
                        src: to_off,
                        dst: fo,
                    });
                    break;
                }
            }
            let Some(rel_val) = hit else { continue };
            let c = &mut ctx.chunks[*chunk];
            c.size = 1;
            c.cols[0] = vec![rel_val];
            c.cur = Some(0);
            return Ok(true);
        },
        OpDesc::Filter { expr, compact } => match compact {
            None => loop {
                if !next(descs, ctx, i - 1)? {
                    return Ok(false);
                }
                if truthy(&eval(ctx, expr)?) {
                    return Ok(true);
                }
            },
            Some(chunk) => loop {
                if !next(descs, ctx, i - 1)? {
                    return Ok(false);
                }
                let size = ctx.chunks[*chunk].size;
                let mut keep = Vec::with_capacity(size);
                for pos in 0..size {
                    ctx.chunks[*chunk].cur = Some(pos);
                    keep.push(truthy(&eval(ctx, expr)?));
                }
                ctx.chunks[*chunk].cur = None;
                if keep.iter().any(|k| *k) {
                    ctx.chunks[*chunk].retain(&keep);
                    return Ok(true);
                }
            },
        },
        OpDesc::Unwind { expr, chunk } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let items = match eval(ctx, expr)? {
                Value::List(items) => items,
                Value::Null => continue,
                other => {
                    return Err(invalid(format!("UNWIND expects a list, got {other:?}")));
                }
            };
            if items.is_empty() {
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = items.len();
            c.cols[0] = items;
            c.cur = None;
            return Ok(true);
        },
    }
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

fn truth(v: &Value) -> Result<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(invalid(format!("expected a boolean, got {other:?}"))),
    }
}

fn truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Three-valued equality: `None` when null is involved, `Some(false)`
/// across mismatched types.
fn cmp_eq(a: &Value, b: &Value) -> Option<bool> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x == y),
        (Value::Float(x), Value::Float(y)) => Some(x == y),
        (Value::Int(x), Value::Float(y)) => Some((*x as f64) == *y),
        (Value::Float(x), Value::Int(y)) => Some(*x == (*y as f64)),
        (Value::Bool(x), Value::Bool(y)) => Some(x == y),
        (Value::Str(x), Value::Str(y)) => Some(x == y),
        (
            Value::Node {
                table: t1,
                offset: o1,
            },
            Value::Node {
                table: t2,
                offset: o2,
            },
        ) => Some(t1 == t2 && o1 == o2),
        (
            Value::Rel {
                table: t1,
                src: s1,
                dst: d1,
            },
            Value::Rel {
                table: t2,
                src: s2,
                dst: d2,
            },
        ) => Some(t1 == t2 && s1 == s2 && d1 == d2),
        (Value::List(x), Value::List(y)) => {
            if x.len() != y.len() {
                return Some(false);
            }
            let mut saw_null = false;
            for (a, b) in x.iter().zip(y) {
                match cmp_eq(a, b) {
                    Some(false) => return Some(false),
                    Some(true) => {}
                    None => saw_null = true,
                }
            }
            if saw_null { None } else { Some(true) }
        }
        _ => Some(false),
    }
}

/// Ordering for comparisons; `None` when null or incomparable types
/// are involved, which makes the comparison null.
fn cmp_ord(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => match (as_f64(a), as_f64(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y),
            _ => None,
        },
    }
}

fn arith(op: BinaryOp, a: Value, b: Value) -> Result<Value> {
    use BinaryOp::*;
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    if op == Add {
        match (&a, &b) {
            (Value::Str(x), Value::Str(y)) => return Ok(Value::Str(format!("{x}{y}"))),
            (Value::List(x), Value::List(y)) => {
                let mut out = x.clone();
                out.extend(y.iter().cloned());
                return Ok(Value::List(out));
            }
            _ => {}
        }
    }
    let overflow = || invalid("integer overflow".into());
    match (&a, &b) {
        (Value::Int(x), Value::Int(y)) => {
            let (x, y) = (*x, *y);
            let r = match op {
                Add => x.checked_add(y).ok_or_else(overflow)?,
                Sub => x.checked_sub(y).ok_or_else(overflow)?,
                Mul => x.checked_mul(y).ok_or_else(overflow)?,
                Div => {
                    if y == 0 {
                        return Err(invalid("division by zero".into()));
                    }
                    x.checked_div(y).ok_or_else(overflow)?
                }
                Mod => {
                    if y == 0 {
                        return Err(invalid("division by zero".into()));
                    }
                    x.checked_rem(y).ok_or_else(overflow)?
                }
                _ => unreachable!("arith only sees arithmetic operators"),
            };
            Ok(Value::Int(r))
        }
        _ => match (as_f64(&a), as_f64(&b)) {
            (Some(x), Some(y)) => {
                let r = match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    Div => x / y,
                    Mod => x % y,
                    _ => unreachable!("arith only sees arithmetic operators"),
                };
                Ok(Value::Float(r))
            }
            _ => Err(invalid(format!("cannot apply {op:?} to {a:?} and {b:?}"))),
        },
    }
}

fn eval(ctx: &mut StageCtx, expr: &BoundExpr) -> Result<Value> {
    match expr {
        BoundExpr::Literal(lit) => Ok(match lit {
            Literal::Null => Value::Null,
            Literal::Bool(b) => Value::Bool(*b),
            Literal::Int(i) => Value::Int(*i),
            Literal::Float(f) => Value::Float(*f),
            Literal::Str(s) => Value::Str(s.clone()),
        }),
        BoundExpr::Param(ix) => Ok(ctx.params[*ix].clone()),
        BoundExpr::Var(slot) => value_of(ctx, *slot),
        BoundExpr::Property { base, key } => match eval(ctx, base)? {
            Value::Node { table, offset } => ctx.graph.property(table, offset, key),
            Value::Null | Value::Rel { .. } => Ok(Value::Null),
            other => Err(invalid(format!(
                "property access on {other:?}, expected a node"
            ))),
        },
        BoundExpr::Unary { op, expr } => {
            let v = eval(ctx, expr)?;
            match op {
                UnaryOp::Not => Ok(match truth(&v)? {
                    Some(b) => Value::Bool(!b),
                    None => Value::Null,
                }),
                UnaryOp::Neg => match v {
                    Value::Int(i) => Ok(Value::Int(
                        i.checked_neg()
                            .ok_or_else(|| invalid("integer overflow".into()))?,
                    )),
                    Value::Float(f) => Ok(Value::Float(-f)),
                    Value::Null => Ok(Value::Null),
                    other => Err(invalid(format!("cannot negate {other:?}"))),
                },
            }
        }
        BoundExpr::Binary { op, lhs, rhs } => {
            use BinaryOp::*;
            match op {
                And => {
                    let l = truth(&eval(ctx, lhs)?)?;
                    if l == Some(false) {
                        return Ok(Value::Bool(false));
                    }
                    let r = truth(&eval(ctx, rhs)?)?;
                    Ok(match (l, r) {
                        (_, Some(false)) => Value::Bool(false),
                        (Some(true), Some(true)) => Value::Bool(true),
                        _ => Value::Null,
                    })
                }
                Or => {
                    let l = truth(&eval(ctx, lhs)?)?;
                    if l == Some(true) {
                        return Ok(Value::Bool(true));
                    }
                    let r = truth(&eval(ctx, rhs)?)?;
                    Ok(match (l, r) {
                        (_, Some(true)) => Value::Bool(true),
                        (Some(false), Some(false)) => Value::Bool(false),
                        _ => Value::Null,
                    })
                }
                Xor => {
                    let l = truth(&eval(ctx, lhs)?)?;
                    let r = truth(&eval(ctx, rhs)?)?;
                    Ok(match (l, r) {
                        (Some(a), Some(b)) => Value::Bool(a ^ b),
                        _ => Value::Null,
                    })
                }
                Eq | Ne => {
                    let l = eval(ctx, lhs)?;
                    let r = eval(ctx, rhs)?;
                    Ok(match cmp_eq(&l, &r) {
                        Some(b) => Value::Bool(if *op == Eq { b } else { !b }),
                        None => Value::Null,
                    })
                }
                Lt | Le | Gt | Ge => {
                    let l = eval(ctx, lhs)?;
                    let r = eval(ctx, rhs)?;
                    if matches!(l, Value::Null) || matches!(r, Value::Null) {
                        return Ok(Value::Null);
                    }
                    Ok(match cmp_ord(&l, &r) {
                        Some(ord) => Value::Bool(match op {
                            Lt => ord == Ordering::Less,
                            Le => ord != Ordering::Greater,
                            Gt => ord == Ordering::Greater,
                            Ge => ord != Ordering::Less,
                            _ => unreachable!(),
                        }),
                        None => Value::Null,
                    })
                }
                Add | Sub | Mul | Div | Mod => {
                    let l = eval(ctx, lhs)?;
                    let r = eval(ctx, rhs)?;
                    arith(*op, l, r)
                }
                In => {
                    let l = eval(ctx, lhs)?;
                    match eval(ctx, rhs)? {
                        Value::Null => Ok(Value::Null),
                        Value::List(items) => {
                            let mut saw_null = false;
                            for item in &items {
                                match cmp_eq(&l, item) {
                                    Some(true) => return Ok(Value::Bool(true)),
                                    None => saw_null = true,
                                    Some(false) => {}
                                }
                            }
                            Ok(if saw_null {
                                Value::Null
                            } else {
                                Value::Bool(false)
                            })
                        }
                        other => Err(invalid(format!("IN expects a list, got {other:?}"))),
                    }
                }
                StartsWith | EndsWith | Contains => {
                    let l = eval(ctx, lhs)?;
                    let r = eval(ctx, rhs)?;
                    match (&l, &r) {
                        (Value::Str(a), Value::Str(b)) => Ok(Value::Bool(match op {
                            StartsWith => a.starts_with(b.as_str()),
                            EndsWith => a.ends_with(b.as_str()),
                            Contains => a.contains(b.as_str()),
                            _ => unreachable!(),
                        })),
                        _ => Ok(Value::Null),
                    }
                }
            }
        }
        BoundExpr::IsNull { expr, negated } => {
            let is_null = matches!(eval(ctx, expr)?, Value::Null);
            Ok(Value::Bool(is_null != *negated))
        }
        BoundExpr::Call {
            func, star, args, ..
        } => match func {
            Func::Id => {
                if *star || args.len() != 1 {
                    return Err(invalid("id() takes exactly one argument".into()));
                }
                match eval(ctx, &args[0])? {
                    Value::Node { offset, .. } => Ok(Value::Int(offset as i64)),
                    Value::Null | Value::Rel { .. } => Ok(Value::Null),
                    other => Err(invalid(format!("id() expects a node, got {other:?}"))),
                }
            }
            Func::Size => {
                if *star || args.len() != 1 {
                    return Err(invalid("size() takes exactly one argument".into()));
                }
                match eval(ctx, &args[0])? {
                    Value::List(items) => Ok(Value::Int(items.len() as i64)),
                    Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                    Value::Null => Ok(Value::Null),
                    other => Err(invalid(format!(
                        "size() expects a list or string, got {other:?}"
                    ))),
                }
            }
            _ => Err(invalid(
                "aggregate call outside a projection, this is a bug".into(),
            )),
        },
        BoundExpr::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(eval(ctx, item)?);
            }
            Ok(Value::List(out))
        }
        BoundExpr::Map(_) => Err(invalid("map values are not supported yet".into())),
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Acc {
    Count(i64),
    Sum(Option<Value>),
    Avg { sum: f64, n: i64 },
    Min(Option<Value>),
    Max(Option<Value>),
    Collect(Vec<Value>),
}

#[derive(Debug, Clone)]
struct AggState {
    acc: Acc,
    /// DISTINCT arguments collect into a set first; multiplicities do
    /// not apply under set semantics.
    distinct: Option<BTreeSet<OrdValue>>,
}

impl AggState {
    fn new(spec: &AggSpec) -> AggState {
        let acc = match spec.func {
            Func::Count => Acc::Count(0),
            Func::Sum => Acc::Sum(None),
            Func::Avg => Acc::Avg { sum: 0.0, n: 0 },
            Func::Min => Acc::Min(None),
            Func::Max => Acc::Max(None),
            Func::Collect => Acc::Collect(Vec::new()),
            Func::Id | Func::Size => unreachable!("scalar function as an aggregate"),
        };
        let distinct = (spec.distinct && !spec.star).then(BTreeSet::new);
        AggState { acc, distinct }
    }

    fn add_star(&mut self, mult: i64) {
        if let Acc::Count(n) = &mut self.acc {
            *n += mult;
        }
    }

    fn add(&mut self, v: Value, mult: i64) -> Result<()> {
        if matches!(v, Value::Null) {
            return Ok(());
        }
        if let Some(set) = &mut self.distinct {
            set.insert(OrdValue(v));
            return Ok(());
        }
        self.apply(v, mult)
    }

    fn apply(&mut self, v: Value, mult: i64) -> Result<()> {
        match &mut self.acc {
            Acc::Count(n) => *n += mult,
            Acc::Sum(acc) => {
                let scaled = match &v {
                    Value::Int(i) => Value::Int(
                        i.checked_mul(mult)
                            .ok_or_else(|| invalid("integer overflow in sum()".into()))?,
                    ),
                    Value::Float(f) => Value::Float(f * mult as f64),
                    other => {
                        return Err(invalid(format!("sum() needs numbers, got {other:?}")));
                    }
                };
                *acc = Some(match acc.take() {
                    None => scaled,
                    Some(prev) => arith(BinaryOp::Add, prev, scaled)?,
                });
            }
            Acc::Avg { sum, n } => {
                let x =
                    as_f64(&v).ok_or_else(|| invalid(format!("avg() needs numbers, got {v:?}")))?;
                *sum += x * mult as f64;
                *n += mult;
            }
            Acc::Min(cur) => {
                if cur
                    .as_ref()
                    .is_none_or(|c| OrdValue(v.clone()) < OrdValue(c.clone()))
                {
                    *cur = Some(v);
                }
            }
            Acc::Max(cur) => {
                if cur
                    .as_ref()
                    .is_none_or(|c| OrdValue(v.clone()) > OrdValue(c.clone()))
                {
                    *cur = Some(v);
                }
            }
            Acc::Collect(items) => {
                for _ in 0..mult {
                    items.push(v.clone());
                }
            }
        }
        Ok(())
    }

    fn finalize(mut self) -> Result<Value> {
        if let Some(set) = self.distinct.take() {
            for v in set {
                self.apply(v.0, 1)?;
            }
        }
        Ok(match self.acc {
            Acc::Count(n) => Value::Int(n),
            Acc::Sum(acc) => acc.unwrap_or(Value::Int(0)),
            Acc::Avg { n: 0, .. } => Value::Null,
            Acc::Avg { sum, n } => Value::Float(sum / n as f64),
            Acc::Min(cur) | Acc::Max(cur) => cur.unwrap_or(Value::Null),
            Acc::Collect(items) => Value::List(items),
        })
    }
}

// ---------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------

struct Row {
    values: Vec<Value>,
    keys: Vec<OrdValue>,
    extra: BTreeMap<usize, Value>,
}

/// The slot a projection item answers to in ORDER BY and WHERE after
/// the projection. WITH items carry it; RETURN items lose theirs in
/// the binder, so it comes back from the variable table by name.
fn item_slot(item: &BoundItem, query: &BoundQuery) -> Option<usize> {
    if item.slot.is_some() {
        return item.slot;
    }
    if let BoundExpr::Var(slot) = item.expr {
        return Some(slot);
    }
    query.variables.iter().rposition(|v| v.name == item.name)
}

fn sort_exprs(sink: &SinkDef) -> &[(BoundExpr, bool)] {
    for op in &sink.post {
        if let PostOp::Sort(keys) = op {
            return keys;
        }
    }
    &[]
}

fn each_config<F>(ctx: &mut StageCtx, unflat: &[usize], f: &mut F) -> Result<()>
where
    F: FnMut(&mut StageCtx) -> Result<()>,
{
    let Some((&c, rest)) = unflat.split_first() else {
        return f(ctx);
    };
    for pos in 0..ctx.chunks[c].size {
        ctx.chunks[c].cur = Some(pos);
        each_config(ctx, rest, f)?;
    }
    ctx.chunks[c].cur = None;
    Ok(())
}

fn materialize(sink: &SinkDef, query: &BoundQuery, ctx: &mut StageCtx) -> Result<Row> {
    ctx.overlay.clear();
    let mut values = Vec::with_capacity(sink.items.len());
    for item in &sink.items {
        values.push(eval(ctx, &item.expr)?);
    }
    for (item, v) in sink.items.iter().zip(&values) {
        if let Some(slot) = item_slot(item, query) {
            ctx.overlay.insert(slot, v.clone());
        }
    }
    let mut keys = Vec::new();
    for (expr, _) in sort_exprs(sink) {
        keys.push(OrdValue(eval(ctx, expr)?));
    }
    let mut extra = BTreeMap::new();
    for &slot in &sink.extra_slots {
        extra.insert(slot, value_of(ctx, slot)?);
    }
    ctx.overlay.clear();
    Ok(Row {
        values,
        keys,
        extra,
    })
}

fn update_groups(
    sink: &SinkDef,
    unflat: &[usize],
    ctx: &mut StageCtx,
    groups: &mut BTreeMap<Vec<OrdValue>, Vec<AggState>>,
) -> Result<()> {
    let mut keyvals = Vec::new();
    for item in sink.items.iter().filter(|it| !it.aggregate) {
        keyvals.push(OrdValue(eval(ctx, &item.expr)?));
    }
    let mut mult: i64 = 1;
    for &c in unflat {
        mult = mult
            .checked_mul(ctx.chunks[c].size as i64)
            .ok_or_else(|| invalid("multiplicity overflow in aggregation".into()))?;
    }
    let states = groups
        .entry(keyvals)
        .or_insert_with(|| sink.aggs.iter().map(AggState::new).collect());
    for (spec, state) in sink.aggs.iter().zip(states) {
        if spec.star {
            state.add_star(mult);
            continue;
        }
        let arg = spec
            .arg
            .as_ref()
            .ok_or_else(|| invalid(format!("{:?}() needs an argument", spec.func)))?;
        match spec.arg_chunk {
            None => {
                let v = eval(ctx, arg)?;
                state.add(v, mult)?;
            }
            Some(c) => {
                let size = ctx.chunks[c].size;
                let others = mult / size as i64;
                for pos in 0..size {
                    ctx.chunks[c].cur = Some(pos);
                    let v = eval(ctx, arg)?;
                    state.add(v, others)?;
                }
                ctx.chunks[c].cur = None;
            }
        }
    }
    Ok(())
}

fn finalize_group(
    sink: &SinkDef,
    query: &BoundQuery,
    ctx: &mut StageCtx,
    keyvals: Vec<OrdValue>,
    states: Vec<AggState>,
) -> Result<Row> {
    let mut kit = keyvals.into_iter();
    let mut sit = states.into_iter();
    let mut values = Vec::with_capacity(sink.items.len());
    for item in &sink.items {
        let v = if item.aggregate {
            sit.next()
                .expect("one state per aggregate item")
                .finalize()?
        } else {
            kit.next().expect("one key value per key item").0
        };
        values.push(v);
    }
    ctx.overlay.clear();
    for (item, v) in sink.items.iter().zip(&values) {
        if let Some(slot) = item_slot(item, query) {
            ctx.overlay.insert(slot, v.clone());
        }
    }
    let mut keys = Vec::new();
    for (expr, _) in sort_exprs(sink) {
        keys.push(OrdValue(eval(ctx, expr)?));
    }
    let extra = ctx.overlay.clone();
    ctx.overlay.clear();
    Ok(Row {
        values,
        keys,
        extra,
    })
}

fn count_expr(ctx: &mut StageCtx, expr: &BoundExpr, what: &str) -> Result<usize> {
    ctx.overlay.clear();
    match eval(ctx, expr)? {
        Value::Int(n) if n >= 0 => Ok(n as usize),
        other => Err(invalid(format!(
            "{what} needs a non-negative integer, got {other:?}"
        ))),
    }
}

fn apply_post(sink: &SinkDef, ctx: &mut StageCtx, mut rows: Vec<Row>) -> Result<Vec<Row>> {
    for op in &sink.post {
        match op {
            PostOp::Distinct => {
                let mut seen = BTreeSet::new();
                rows.retain(|row| {
                    seen.insert(row.values.iter().cloned().map(OrdValue).collect::<Vec<_>>())
                });
            }
            PostOp::Filter(expr) => {
                let mut kept = Vec::with_capacity(rows.len());
                for mut row in rows {
                    std::mem::swap(&mut ctx.overlay, &mut row.extra);
                    let pass = truthy(&eval(ctx, expr)?);
                    std::mem::swap(&mut ctx.overlay, &mut row.extra);
                    if pass {
                        kept.push(row);
                    }
                }
                rows = kept;
            }
            PostOp::Sort(keys) => {
                let dirs: Vec<bool> = keys.iter().map(|(_, asc)| *asc).collect();
                rows.sort_by(|a, b| {
                    for (ix, asc) in dirs.iter().enumerate() {
                        let ord = a.keys[ix].cmp(&b.keys[ix]);
                        let ord = if *asc { ord } else { ord.reverse() };
                        if ord != Ordering::Equal {
                            return ord;
                        }
                    }
                    Ordering::Equal
                });
            }
            PostOp::Skip(expr) => {
                let n = count_expr(ctx, expr, "SKIP")?.min(rows.len());
                rows.drain(..n);
            }
            PostOp::Limit(expr) => {
                let n = count_expr(ctx, expr, "LIMIT")?;
                rows.truncate(n);
            }
        }
    }
    Ok(rows)
}

fn run_stage(stage: &StageDef, query: &BoundQuery, ctx: &mut StageCtx) -> Result<Vec<Vec<Value>>> {
    let top = stage.descs.len() - 1;
    let sink = &stage.sink;
    let mut rows = Vec::new();
    if sink.aggregate {
        let mut groups: BTreeMap<Vec<OrdValue>, Vec<AggState>> = BTreeMap::new();
        while next(&stage.descs, ctx, top)? {
            update_groups(sink, &stage.unflat, ctx, &mut groups)?;
        }
        if groups.is_empty() && sink.items.iter().all(|it| it.aggregate) {
            groups.insert(Vec::new(), sink.aggs.iter().map(AggState::new).collect());
        }
        for (keyvals, states) in groups {
            rows.push(finalize_group(sink, query, ctx, keyvals, states)?);
        }
    } else {
        while next(&stage.descs, ctx, top)? {
            each_config(ctx, &stage.unflat, &mut |ctx| {
                rows.push(materialize(sink, query, ctx)?);
                Ok(())
            })?;
        }
    }
    let rows = apply_post(sink, ctx, rows)?;
    Ok(rows.into_iter().map(|r| r.values).collect())
}

/// Runs an optimized plan against a graph and returns the result rows.
pub fn execute(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
) -> Result<QueryResult> {
    let stages = build_stages(plan, query, schema, options)?;
    let counts: BTreeMap<u32, u64> = schema
        .nodes()
        .iter()
        .map(|n| (n.id, n.node_count))
        .collect();
    let mut rows = Vec::new();
    for stage in &stages {
        let mut ctx = StageCtx {
            graph: &mut *graph,
            params,
            counts: &counts,
            slot_loc: &stage.slot_loc,
            chunks: stage
                .chunk_slots
                .iter()
                .map(|slots| Chunk {
                    cols: vec![Vec::new(); slots.len()],
                    ..Chunk::default()
                })
                .collect(),
            states: vec![OpState::default(); stage.descs.len()],
            rows,
            overlay: BTreeMap::new(),
            scratch: Vec::new(),
        };
        rows = run_stage(stage, query, &mut ctx)?;
    }
    Ok(QueryResult {
        columns: query.columns.clone(),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{NodeDef, RelDef};

    /// Six people, three places, eight KNOWS edges with exactly one
    /// directed triangle (0, 1, 2), and one place per person.
    fn schema() -> Schema {
        Schema::new(
            vec![
                NodeDef {
                    id: 0,
                    name: "Person".into(),
                    node_count: 6,
                },
                NodeDef {
                    id: 1,
                    name: "Place".into(),
                    node_count: 3,
                },
            ],
            vec![
                RelDef {
                    id: 2,
                    name: "KNOWS".into(),
                    from: 0,
                    to: 0,
                    edge_count: 8,
                },
                RelDef {
                    id: 3,
                    name: "IS_LOCATED_IN".into(),
                    from: 0,
                    to: 1,
                    edge_count: 6,
                },
            ],
        )
        .expect("schema")
    }

    struct MockGraph {
        edges: BTreeMap<u32, Vec<(u64, u64)>>,
    }

    fn mock() -> MockGraph {
        let mut edges = BTreeMap::new();
        edges.insert(
            2,
            vec![
                (0, 1),
                (0, 2),
                (1, 2),
                (1, 3),
                (2, 4),
                (3, 4),
                (4, 5),
                (5, 0),
            ],
        );
        edges.insert(3, vec![(0, 0), (1, 1), (2, 2), (3, 0), (4, 1), (5, 2)]);
        MockGraph { edges }
    }

    impl Graph for MockGraph {
        fn neighbors(
            &mut self,
            rel: u32,
            node: u64,
            reversed: bool,
            out: &mut Vec<u64>,
        ) -> Result<()> {
            out.clear();
            for &(src, dst) in &self.edges[&rel] {
                if !reversed && src == node {
                    out.push(dst);
                }
                if reversed && dst == node {
                    out.push(src);
                }
            }
            Ok(())
        }

        fn has_edge(&mut self, rel: u32, src: u64, dst: u64) -> Result<bool> {
            Ok(self.edges[&rel].contains(&(src, dst)))
        }

        fn property(&mut self, _table: u32, offset: u64, key: &str) -> Result<Value> {
            match key {
                "id" => Ok(Value::Int(offset as i64)),
                other => Err(invalid(format!("unknown property '{other}'"))),
            }
        }
    }

    fn run_with(source: &str, params: &[(&str, Value)], flat: bool) -> QueryResult {
        let schema = schema();
        let parsed = crate::parser::parse(source).expect("parse");
        let query = crate::binder::bind(&parsed, &schema).expect("bind");
        let built = crate::plan::build(&query).expect("plan");
        let optimized = crate::optimizer::optimize(built, &query, &schema).expect("optimize");
        let mut args = Vec::new();
        for name in &query.params {
            let (_, v) = params
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("missing parameter ${name}"));
            args.push(v.clone());
        }
        let mut graph = mock();
        execute(
            &optimized,
            &query,
            &schema,
            &mut graph,
            &args,
            &Options { flat },
        )
        .expect("execute")
    }

    fn run(source: &str, params: &[(&str, Value)]) -> QueryResult {
        run_with(source, params, false)
    }

    fn int_rows(result: &QueryResult) -> Vec<Vec<i64>> {
        result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|v| match v {
                        Value::Int(i) => *i,
                        other => panic!("expected an int, got {other:?}"),
                    })
                    .collect()
            })
            .collect()
    }

    fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        rows.sort_by(|a, b| {
            for (x, y) in a.iter().zip(b) {
                let ord = OrdValue(x.clone()).cmp(&OrdValue(y.clone()));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        rows
    }

    #[test]
    fn returns_literals_without_a_match() {
        let r = run("RETURN 1 AS one, 'x' AS s, 1 + 2 * 3 AS n", &[]);
        assert_eq!(r.columns, ["one", "s", "n"]);
        assert_eq!(
            r.rows,
            [[Value::Int(1), Value::Str("x".into()), Value::Int(7)]]
        );
    }

    #[test]
    fn point_lookup_expands_one_hop() {
        let r = run(
            "MATCH (a:Person {id: $src})-[:KNOWS]->(b) \
             RETURN b.id AS friend ORDER BY friend",
            &[("src", Value::Int(0))],
        );
        assert_eq!(int_rows(&r), [[1], [2]]);
    }

    #[test]
    fn reversed_patterns_read_in_neighbors() {
        let r = run(
            "MATCH (a:Person)<-[:KNOWS]-(b) WHERE a.id = 0 RETURN b.id AS src ORDER BY src",
            &[],
        );
        assert_eq!(int_rows(&r), [[5]]);
    }

    #[test]
    fn two_hop_count_stays_factorized() {
        let r = run(
            "MATCH (a:Person {id: $src})-[:KNOWS]->(b)-[:KNOWS]->(c) \
             RETURN count(c) AS paths",
            &[("src", Value::Int(0))],
        );
        assert_eq!(int_rows(&r), [[3]]);
    }

    #[test]
    fn grouped_counts_flatten_the_keys() {
        let r = run(
            "MATCH (a:Person)-[:KNOWS]->(b) \
             RETURN a.id AS id, count(b) AS deg ORDER BY id",
            &[],
        );
        assert_eq!(
            int_rows(&r),
            [[0, 2], [1, 2], [2, 1], [3, 1], [4, 1], [5, 1]]
        );
    }

    #[test]
    fn undirected_expands_union_both_directions() {
        let r = run(
            "MATCH (a:Person {id: 2})-[:KNOWS]-(b) RETURN b.id AS other ORDER BY other",
            &[],
        );
        assert_eq!(int_rows(&r), [[0], [1], [4]]);
    }

    #[test]
    fn cross_table_expands_reach_places() {
        let r = run(
            "MATCH (p:Person)-[:IS_LOCATED_IN]->(pl:Place) WHERE pl.id = 1 \
             RETURN p.id AS person ORDER BY person",
            &[],
        );
        assert_eq!(int_rows(&r), [[1], [4]]);
    }

    #[test]
    fn triangles_close_with_an_edge_probe() {
        let r = run(
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
             RETURN a.id AS a, b.id AS b, c.id AS c",
            &[],
        );
        assert_eq!(int_rows(&r), [[0, 1, 2]]);
    }

    #[test]
    fn with_pipelines_filter_grouped_rows() {
        let r = run(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg WHERE deg > 1 \
             MATCH (a)-[:IS_LOCATED_IN]->(pl) \
             RETURN a.id AS person, pl.id AS place ORDER BY person",
            &[],
        );
        assert_eq!(int_rows(&r), [[0, 0], [1, 1]]);
    }

    #[test]
    fn unwind_distinct_sort_skip_limit() {
        let r = run(
            "UNWIND [3, 1, 2, 3, 2] AS x \
             RETURN DISTINCT x AS v ORDER BY v DESC SKIP 1 LIMIT 2",
            &[],
        );
        assert_eq!(int_rows(&r), [[2], [1]]);
    }

    #[test]
    fn aggregate_functions_cover_the_numeric_kit() {
        let r = run(
            "MATCH (a:Person)-[:KNOWS]->(b) \
             RETURN sum(b.id) AS s, min(b.id) AS mn, max(b.id) AS mx, \
                    avg(b.id) AS av, count(DISTINCT b) AS d",
            &[],
        );
        assert_eq!(
            r.rows,
            [[
                Value::Int(21),
                Value::Int(0),
                Value::Int(5),
                Value::Float(2.625),
                Value::Int(6),
            ]]
        );
    }

    #[test]
    fn nulls_are_skipped_but_star_counts_rows() {
        let r = run(
            "UNWIND [1, null, 2] AS x RETURN count(x) AS c, count(*) AS star",
            &[],
        );
        assert_eq!(int_rows(&r), [[2, 3]]);
    }

    #[test]
    fn lookups_outside_the_table_return_nothing() {
        let r = run(
            "MATCH (a:Person {id: 99})-[:KNOWS]->(b) RETURN b.id AS friend",
            &[],
        );
        assert!(r.rows.is_empty(), "got {:?}", r.rows);
    }

    #[test]
    fn aggregates_over_no_rows_still_answer() {
        let r = run(
            "MATCH (a:Person {id: 99})-[:KNOWS]->(b) RETURN count(b) AS n",
            &[],
        );
        assert_eq!(int_rows(&r), [[0]]);
    }

    #[test]
    fn flat_mode_matches_factorized_execution() {
        let cases: &[(&str, &[(&str, Value)])] = &[
            (
                "MATCH (a:Person {id: $src})-[:KNOWS]->(b) RETURN b.id AS friend",
                &[("src", Value::Int(0))],
            ),
            (
                "MATCH (a:Person {id: $src})-[:KNOWS]->(b)-[:KNOWS]->(c) \
                 RETURN count(c) AS paths",
                &[("src", Value::Int(0))],
            ),
            (
                "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.id AS id, count(b) AS deg",
                &[],
            ),
            (
                "MATCH (a:Person {id: 2})-[:KNOWS]-(b) RETURN b.id AS other",
                &[],
            ),
            (
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                 RETURN a.id AS a, b.id AS b, c.id AS c",
                &[],
            ),
            (
                "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg WHERE deg > 1 \
                 MATCH (a)-[:IS_LOCATED_IN]->(pl) RETURN a.id AS person, pl.id AS place",
                &[],
            ),
        ];
        for (source, params) in cases {
            let fac = run_with(source, params, false);
            let flat = run_with(source, params, true);
            assert_eq!(
                sorted(fac.rows.clone()),
                sorted(flat.rows.clone()),
                "flat and factorized disagree on: {source}"
            );
        }
    }
}
