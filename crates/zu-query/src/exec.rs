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
//! `execute_profiled` runs the same pipeline with per-operator
//! counters and returns a `Profile` next to the result: pulls, rows,
//! and self time per operator, per stage. Rows over pulls is the
//! average vector length, the factorization stat: an expand averaging
//! forty is producing real vectors, one averaging one has degenerated
//! to tuple-at-a-time. This is the EXPLAIN ANALYZE backend; the
//! grammar has no EXPLAIN keyword yet, so the facade exposes it as an
//! API entry point.
//!
//! A `Filter` directly above a scan on an `id` equality becomes an
//! `IndexLookup` that jumps to the offset. This leans on the v0
//! contract that the `id` property of a node equals its offset; keyed
//! tables get their own lookup path when the column catalog lands.
//!
//! Variable-length patterns execute as `VarExpand`: a depth-first
//! enumeration of paths, one output value per path, with the rel
//! variable bound to the edge list. The path mode picks the repeat
//! rule (WALK none, TRAIL no repeated edge, ACYCLIC no repeated node)
//! and a SHORTEST selector first runs a breadth-first hop-level pass
//! from the start so only minimum-hop paths enumerate. That is the
//! correctness-first baseline; wiring the RecursiveBFS frontier engine
//! underneath is the rest of milestone 4.
//!
//! OPTIONAL MATCH executes as a left-outer group. Every flatten the
//! group needs on outer chunks sits below an `OptionalBegin` that
//! yields each outer configuration exactly once per activation, the
//! group's operators run above it, and `OptionalEnd` passes matches
//! through or, when an outer configuration produced nothing, binds the
//! group's chunks to a single null row. Filters born inside the
//! optional clause compile into the group, so a WHERE there gates
//! matches within the group instead of dropping the null row.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use zu_common::gqlstatus::{DiagnosticRecord, GqlStatus, codes};
use zu_common::{DurationKind, Result, Temporal, ZuError, temporal};

use crate::ast::{BinaryOp, Literal, PathMode, RelDirection, Selector, UnaryOp};
use crate::binder::{BoundExpr, BoundItem, BoundQuery, Func, PathPart, Schema, TableFunc};
use crate::plan::{LogicalPlan, expr_text};

/// Vector width of one chunk fill.
pub const VECTOR_SIZE: usize = 2048;

/// An engine-internal failure: a slot that is not bound, a multiplicity
/// that overflowed, a shape the planner should never have produced.
/// These are bugs in zu, not conditions the standard has a code for, so
/// they stay uncoded rather than borrowing one that nearly fits.
fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// A GQL condition raised while evaluating.
fn gql(status: zu_common::GqlStatus, detail: String) -> ZuError {
    ZuError::gql(status, detail)
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
    Node {
        table: u32,
        offset: u64,
    },
    Rel {
        table: u32,
        src: u64,
        dst: u64,
    },
    List(Vec<Value>),
    /// GV45. A record: named fields, each holding a value. The fields
    /// are kept sorted by name and a name appears once, which is what
    /// makes two records with the same fields written in different
    /// orders one value rather than two. Build one with
    /// [`Value::record`] rather than by hand, so that holds.
    Record(Vec<(String, Value)>),
    /// A date, a time, a datetime or a duration. One arm rather than
    /// six, because a temporal value is a count and a meaning and the
    /// meaning is what [`Temporal`] carries; the executor treats them
    /// alike everywhere except where the calendar is involved.
    Temporal(Temporal),
    /// GV55. A path: the nodes and edges of a walk, alternating, a node
    /// at each end. A one node path has no edges and is the shortest
    /// there is; there is no empty path, because a path is a walk and a
    /// walk starts somewhere. Build one with [`Value::path`], which
    /// checks that shape.
    Path(Vec<Value>),
    /// A PMR chain (docs/07 §5): the executor-internal form of a
    /// variable-length path. [`settle`] turns it into the edge list
    /// before any value leaves the pipeline, so results never hold one.
    Chain(Arc<PathLink>),
}

impl Value {
    /// A record out of the fields as they were written.
    ///
    /// The fields are sorted by name here rather than compared by name
    /// later, because every reader of a record then gets the cheap
    /// version of the question: two records have the same fields when
    /// their name lists are equal, and equality is a walk down the two
    /// in step. A field written twice is the caller's error and the
    /// binder refuses it before this is reached, so the first one
    /// stands rather than this having an error path nothing can reach.
    pub fn record(mut fields: Vec<(String, Value)>) -> Value {
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        fields.dedup_by(|a, b| a.0 == b.0);
        Value::Record(fields)
    }

    /// A path out of an alternating element list, or the condition the
    /// list breaks.
    ///
    /// GE06 builds a path from elements the query names, which means
    /// the two things a matched path gets for free have to be checked
    /// here instead. The shape is the first: nodes and edges alternate
    /// and a node is at each end, so an even length or an edge in a
    /// node's place is not a path at all. The joining is the second and
    /// is the one worth having: an edge between two nodes it does not
    /// touch is a sequence of the right shape that describes a walk
    /// nobody can take, and 22G0Z is the condition for exactly that. An
    /// edge is allowed to be traversed against its direction, because a
    /// walk may go either way along one and ISO's path values are not
    /// directed.
    pub fn path(elements: Vec<Value>) -> Result<Value> {
        if elements.is_empty() || elements.len().is_multiple_of(2) {
            return Err(gql(
                codes::C22G0Z,
                format!(
                    "a path is a node, then an edge and a node for each hop, so it has an odd \
                     number of elements and not {}",
                    elements.len()
                ),
            ));
        }
        for (ix, element) in elements.iter().enumerate() {
            let wanted_node = ix.is_multiple_of(2);
            let ok = match element {
                Value::Node { .. } => wanted_node,
                Value::Rel { .. } => !wanted_node,
                _ => false,
            };
            if !ok {
                let wanted = if wanted_node { "a node" } else { "an edge" };
                return Err(gql(
                    codes::C22G0Z,
                    format!("element {} of the path is not {wanted}", ix + 1),
                ));
            }
        }
        for hop in elements.windows(3).step_by(2) {
            let [
                Value::Node { offset: from, .. },
                Value::Rel { src, dst, .. },
                Value::Node { offset: to, .. },
            ] = hop
            else {
                unreachable!("the loop above accepted the element kinds")
            };
            if !((from == src && to == dst) || (from == dst && to == src)) {
                return Err(gql(
                    codes::C22G0Z,
                    "an edge of the path does not join the two nodes it sits between".to_owned(),
                ));
            }
        }
        Ok(Value::Path(elements))
    }

    /// The value of the field named `name`, or `None` when the record
    /// has no such field.
    pub fn field(&self, name: &str) -> Option<&Value> {
        let Value::Record(fields) = self else {
            return None;
        };
        fields
            .binary_search_by(|(n, _)| n.as_str().cmp(name))
            .ok()
            .map(|ix| &fields[ix].1)
    }
}

/// One link of a PMR chain: a persistent predecessor list. Every DFS
/// branch shares its parent's links, so holding every path from one
/// start node costs the tree of links, not one edge list per path.
/// `prev` and `rel` are `None` only on the root link at the start.
#[derive(Debug, Clone, PartialEq)]
pub struct PathLink {
    pub prev: Option<Arc<PathLink>>,
    pub rel: Option<Value>,
    pub node: Value,
    pub hops: u64,
}

/// The settled form of a chain: the rel values root first, which is
/// what a variable-length rel variable equals outside the executor.
fn path_rels(link: &PathLink) -> Value {
    let mut rels = Vec::with_capacity(link.hops as usize);
    let mut cur = Some(link);
    while let Some(l) = cur {
        if let Some(rel) = &l.rel {
            rels.push(rel.clone());
        }
        cur = l.prev.as_deref();
    }
    rels.reverse();
    Value::List(rels)
}

/// Replaces every PMR chain in a value with its settled edge list.
/// Runs where values leave the pipeline: projections, grouping keys,
/// sort keys, and aggregate arguments.
fn settle(v: Value) -> Value {
    match v {
        Value::Chain(link) => path_rels(&link),
        Value::List(items) => Value::List(items.into_iter().map(settle).collect()),
        // A field can hold a chain the same way a list element can,
        // and for the same reason: `{p: p}` names a path variable.
        Value::Record(fields) => {
            Value::Record(fields.into_iter().map(|(n, v)| (n, settle(v))).collect())
        }
        other => other,
    }
}

fn chain_has_rel(link: &PathLink, rel: &Value) -> bool {
    let mut cur = Some(link);
    while let Some(l) = cur {
        if l.rel.as_ref() == Some(rel) {
            return true;
        }
        cur = l.prev.as_deref();
    }
    false
}

fn chain_has_node(link: &PathLink, table: u32, offset: u64) -> bool {
    let mut cur = Some(link);
    while let Some(l) = cur {
        if l.node == (Value::Node { table, offset }) {
            return true;
        }
        cur = l.prev.as_deref();
    }
    false
}

/// The rows a query returns, one column name per RETURN item, plus any
/// conditions raised along the way that did not stop it.
///
/// This and [`QueryResult::status`] are the other half of the GQLSTATUS
/// envelope (Spec/2064g/gql/plan/07). An exception replaces the result
/// and comes back as `Err`; a warning rides with the answer and lands in
/// `notices`, because a statement that dropped a null out of an
/// aggregate still has rows to give you and the standard still wants you
/// told. Almost every query leaves `notices` empty, so it costs one empty
/// `Vec` and no allocation on the path that raises nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub notices: Vec<DiagnosticRecord>,
}

impl QueryResult {
    /// The result of a statement that ran to completion.
    pub fn new(columns: Vec<String>, rows: Vec<Vec<Value>>) -> Self {
        QueryResult {
            columns,
            rows,
            notices: Vec::new(),
        }
    }

    /// The completion condition for the statement, which is the single
    /// GQLSTATUS value a caller gets back when nothing went wrong.
    ///
    /// `00000 successful completion` unless the statement had no
    /// projection to give back, in which case the standard has a
    /// condition for exactly that: `00001 successful completion, omitted
    /// result`. It is derived here rather than stored at each call site
    /// so no executor can forget it, and because "did this statement
    /// produce columns" is the whole test.
    ///
    /// This is deliberately not a notice. A statement reports one
    /// outcome and any number of warnings alongside it, and folding the
    /// outcome into the warning list makes the two indistinguishable to
    /// anything reading the envelope.
    pub fn status(&self) -> GqlStatus {
        if self.columns.is_empty() {
            codes::C00001
        } else {
            codes::C00000
        }
    }

    /// Attaches a condition to a result that is still an answer. Nothing
    /// here may be an exception: an exception is not something a
    /// statement returns alongside rows, it is what it returns instead.
    pub fn notice(&mut self, record: DiagnosticRecord) {
        debug_assert!(
            record.severity().is_success(),
            "an exception belongs in Err, not in notices: {record}"
        );
        // Aggregate warnings fire per group; the caller wants to know it
        // happened, not how many times.
        if !self.notices.iter().any(|n| n.status == record.status) {
            self.notices.push(record);
        }
    }
}

/// What the executor needs from a storage engine. Methods take
/// `&mut self` because readers cache decoded state.
pub trait Graph {
    /// Replaces `out` with the neighbor list of `node` in rel table
    /// `rel`: destinations when `reversed` is false, sources when true.
    /// Lists are ascending, the CSR order every engine stores; the
    /// galloping intersection and every binary probe over a list
    /// depend on it.
    fn neighbors(&mut self, rel: u32, node: u64, reversed: bool, out: &mut Vec<u64>) -> Result<()>;
    /// Edge probe in storage orientation: does `src` point at `dst`?
    fn has_edge(&mut self, rel: u32, src: u64, dst: u64) -> Result<bool>;
    /// Neighbor count without the list. The default reads the list and
    /// counts it; engines whose adjacency stores offsets override this
    /// so a counting expand never touches neighbor values.
    fn degree(&mut self, rel: u32, node: u64, reversed: bool) -> Result<u64> {
        let mut out = Vec::new();
        self.neighbors(rel, node, reversed, &mut out)?;
        Ok(out.len() as u64)
    }
    /// Sum of degrees over a node list, the counting expand's bulk
    /// read: one call per source vector instead of one per node. The
    /// default loops over [`Graph::degree`]; engines override to keep
    /// the whole sum inside one reader.
    fn degree_sum(&mut self, rel: u32, nodes: &[u64], reversed: bool) -> Result<u64> {
        let mut total = 0;
        for &node in nodes {
            total += self.degree(rel, node, reversed)?;
        }
        Ok(total)
    }
    /// Resolves a user-facing node id to its row offset, `None` when
    /// the id names no row. The default is the dense-id contract where
    /// the id is the offset; engines whose loads relabeled rows
    /// override it to consult the primary-key index, so `{id: ...}`
    /// lookups and the `id` property stay in the caller's key space.
    fn lookup_key(&mut self, table: u32, key: u64) -> Result<Option<u64>> {
        let _ = table;
        Ok(Some(key))
    }
    /// One property of one node. The v0 contract is that `id` equals
    /// the offset; everything else is up to the engine.
    fn property(&mut self, table: u32, offset: u64, key: &str) -> Result<Value>;
    /// An independent reader over the same storage for a morsel
    /// worker, with its own decoded-state caches. The default `None`
    /// keeps every query on one thread; engines that can open a second
    /// handle override it. A fork only ever reads.
    fn fork(&self) -> Option<Box<dyn Graph + Send>> {
        None
    }
    /// Runs a whole-graph table function kernel over one rel table
    /// (docs/07 §4, `CALL`): one row of YIELD values per node offset of
    /// the rel's node domain, in offset order, without the node column
    /// itself, which the executor synthesizes. The sssp source arrives
    /// as a dense offset, already resolved through [`Graph::lookup_key`].
    /// Engines without kernels keep the default error.
    fn table_function(&mut self, name: &str, rel: u32, args: &[Value]) -> Result<Vec<Vec<Value>>> {
        let _ = (rel, args);
        Err(invalid(format!(
            "table function {name} is not supported by this engine"
        )))
    }
}

/// Execution switches. `flat` forces tuple-at-a-time execution by
/// flattening after every producer, the differential oracle for the
/// factorized paths; flat runs also stay single-threaded so the
/// oracle is the fully sequential baseline.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub flat: bool,
    /// Worker threads for morsel-parallel stages: 0 picks
    /// `min(cores, 8)` per docs/02, 1 forces sequential execution.
    pub threads: usize,
    /// Rows per morsel, 0 picks the 2048-tuple target from docs/02.
    /// Smaller values force many morsels on small graphs, which is how
    /// the tests drive the parallel path over the mock fixtures.
    pub morsel_rows: usize,
    /// How closing-edge probes reach the `MultiwayIntersect`, the
    /// WCOJ closing step of docs/07 §4.
    pub wcoj: Wcoj,
    /// Whether a join hands what it knows about its keys to the
    /// operators producing its probe side, the sideways pass of
    /// perf/13 §1.
    pub sip: Sip,
}

/// The WCOJ fusion switch. The optimizer marks cyclic closes on the
/// plan; `Auto` honors those marks, which is the default path. `Force`
/// fuses every close the fusion can structurally take, marked or not,
/// and `Off` pins the binary ExpandInto/AspJoin pair, the baseline for
/// differential runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wcoj {
    #[default]
    Auto,
    Force,
    Off,
}

/// The sideways information passing switch. `On` lets a join publish a
/// filter over its build keys to the level its probe key is read from,
/// which is the default. `Off` pins the plain probe, so a run with it
/// is the baseline the filter is measured against and the rows either
/// way have to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sip {
    #[default]
    On,
    Off,
}

// ---------------------------------------------------------------------------
// Profiling
// ---------------------------------------------------------------------------

/// One operator line of an EXPLAIN ANALYZE profile.
#[derive(Debug, Clone)]
pub struct OpProfile {
    pub name: String,
    /// Successful pulls: how many chunks this operator produced.
    pub pulls: u64,
    /// Values produced across all pulls. Rows over pulls is the
    /// average vector length, the factorization stat.
    pub rows: u64,
    /// The rows those values stand for with the factorization
    /// multiplied out: on a chain it equals `rows`, on a star it is
    /// the product over every vector still unflat beside this one.
    pub flat: u64,
    /// What the optimizer expected this operator to produce, when the
    /// operator is one that produces rows at all. `None` for the
    /// source, the flattens, and the optional brackets, which pass
    /// their input through and have no cardinality of their own.
    pub est: Option<f64>,
    /// The most rows the optimizer's ceiling allowed this operator,
    /// when the statistics were there to set one. `flat` above this is
    /// a bound violation and perf/12 §6 makes that a hard fail.
    pub bnd: Option<f64>,
    /// Self time in nanoseconds, child time excluded.
    pub nanos: u64,
}

impl OpProfile {
    /// The q-error of this operator's estimate, `max(est/act, act/est)`
    /// (perf/12 §4). Both sides are floored at one row: a zero actual
    /// against an estimate of 3 is an error of 3, not of infinity, and
    /// an operator nobody pulled has no error at all.
    /// Whether this operator produced more rows than the optimizer's
    /// ceiling promised it could. Compared on the flattened count,
    /// which is what the estimate stands for.
    ///
    /// The slack is there because the ceiling comes out of roots and
    /// fractional powers: a table whose every node holds one edge has
    /// an exact bound of `sqrt(n) * l2`, and in f64 that lands a
    /// couple of ULPs under the edge count it is supposed to equal. A
    /// real violation is a factor out, never a rounding.
    pub fn bound_violation(&self) -> bool {
        self.bnd
            .is_some_and(|b| self.flat as f64 > b * (1.0 + 1e-9))
    }

    pub fn qerror(&self) -> Option<f64> {
        let est = self.est?.max(1.0);
        if self.pulls == 0 {
            return None;
        }
        let act = (self.flat as f64).max(1.0);
        Some((est / act).max(act / est))
    }
}

/// One stage of a profiled run: the operator pipeline bottom-up plus
/// the sink that materialized the rows.
#[derive(Debug, Clone)]
pub struct StageProfile {
    pub ops: Vec<OpProfile>,
    pub sink: String,
    pub out_rows: u64,
    /// Wall time of the whole stage in nanoseconds, sink included.
    pub nanos: u64,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub stages: Vec<StageProfile>,
}

fn fmt_time(nanos: u64) -> String {
    if nanos >= 1_000_000_000 {
        format!("{:.2} s", nanos as f64 / 1e9)
    } else if nanos >= 1_000_000 {
        format!("{:.2} ms", nanos as f64 / 1e6)
    } else if nanos >= 1_000 {
        format!("{:.1} us", nanos as f64 / 1e3)
    } else {
        format!("{nanos} ns")
    }
}

impl Profile {
    /// Every operator's q-error across every stage, unordered. This is
    /// what `zu bench cardinality` accumulates into percentiles.
    pub fn qerrors(&self) -> Vec<f64> {
        self.stages
            .iter()
            .flat_map(|s| s.ops.iter().filter_map(OpProfile::qerror))
            .collect()
    }

    /// Renders one block per stage: the sink with its row count and
    /// wall time, then each operator top-down with pulls, rows, the
    /// estimate and its q-error where there is one, the average vector
    /// length per pull, and self time.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (ix, stage) in self.stages.iter().enumerate() {
            out.push_str(&format!(
                "stage {}: {} [{} rows, {}]\n",
                ix + 1,
                stage.sink,
                stage.out_rows,
                fmt_time(stage.nanos)
            ));
            let width = stage.ops.iter().map(|o| o.name.len()).max().unwrap_or(0);
            for op in stage.ops.iter().rev() {
                let avg = if op.pulls == 0 {
                    0.0
                } else {
                    op.rows as f64 / op.pulls as f64
                };
                let est = match (op.est, op.qerror()) {
                    (Some(e), Some(q)) => format!("  est {:>9}  q {q:>6.1}", e.round() as i64),
                    (Some(e), None) => format!("  est {:>9}  q      -", e.round() as i64),
                    (None, _) => "  est         -  q      -".to_string(),
                };
                // A violated ceiling is the one thing here that is a
                // bug rather than a number, so it says so in words.
                let over = match op.bound_violation() {
                    true => format!("  BOUND {:>9}", op.bnd.unwrap_or_default().round() as i64),
                    false => String::new(),
                };
                out.push_str(&format!(
                    "  {:width$}  pulls {:>6}  rows {:>8}  flat {:>9}{est}  avg {:>7.1}  self {}{over}\n",
                    op.name,
                    op.pulls,
                    op.rows,
                    op.flat,
                    avg,
                    fmt_time(op.nanos),
                ));
            }
        }
        out
    }
}

fn slot_names(slots: &[usize], query: &BoundQuery) -> String {
    slots
        .iter()
        .map(|&s| query.variables[s].name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn node_tables_text(tables: &[u32], schema: &Schema) -> String {
    tables
        .iter()
        .map(|&t| schema.node_by_id(t).map_or("?", |d| d.name.as_str()))
        .collect::<Vec<_>>()
        .join("|")
}

fn rel_text(
    from: &str,
    to: &str,
    direction: RelDirection,
    rels: &[RelStep],
    schema: &Schema,
) -> String {
    let names = rels
        .iter()
        .map(|r| schema.rel_by_id(r.id).map_or("?", |d| d.name.as_str()))
        .collect::<Vec<_>>()
        .join("|");
    match direction {
        RelDirection::Out => format!("({from})-[:{names}]->({to})"),
        RelDirection::In => format!("({from})<-[:{names}]-({to})"),
        RelDirection::Undirected => format!("({from})-[:{names}]-({to})"),
    }
}

fn op_name(desc: &OpDesc, stage: &StageDef, query: &BoundQuery, schema: &Schema) -> String {
    let var = |slot: usize| query.variables[slot].name.as_str();
    match desc {
        OpDesc::Source => "Source".into(),
        OpDesc::RowSource { chunk } => {
            format!(
                "RowSource {}",
                slot_names(&stage.chunk_slots[*chunk], query)
            )
        }
        OpDesc::Scan { tables, chunk } => format!(
            "Scan {}: {}",
            var(stage.chunk_slots[*chunk][0]),
            node_tables_text(tables, schema)
        ),
        OpDesc::IndexLookup { tables, key, chunk } => format!(
            "IndexLookup {}: {} [id = {}]",
            var(stage.chunk_slots[*chunk][0]),
            node_tables_text(tables, schema),
            expr_text(key, query)
        ),
        OpDesc::Flatten { chunk } => {
            format!("Flatten {}", slot_names(&stage.chunk_slots[*chunk], query))
        }
        OpDesc::Expand {
            from,
            direction,
            rels,
            chunk,
            degrees,
            ..
        } => format!(
            "{} {}",
            if *degrees { "ExpandCount" } else { "Expand" },
            rel_text(
                var(*from),
                var(stage.chunk_slots[*chunk][0]),
                *direction,
                rels,
                schema
            )
        ),
        OpDesc::VarExpand {
            from,
            direction,
            rels,
            min,
            max,
            mode,
            selector,
            chunk,
            ..
        } => {
            let max = max.map_or(String::new(), |v| v.to_string());
            let mode = match mode {
                PathMode::Walk => " walk",
                PathMode::Trail => "",
                PathMode::Acyclic => " acyclic",
            };
            let sel = match selector {
                Some(Selector::AnyShortest) => " any shortest",
                Some(Selector::AllShortest) => " all shortest",
                None => "",
            };
            format!(
                "VarExpand *{min}..{max}{mode}{sel} {}",
                rel_text(
                    var(*from),
                    var(stage.chunk_slots[*chunk][0]),
                    *direction,
                    rels,
                    schema
                )
            )
        }
        OpDesc::ExpandInto {
            from,
            to,
            direction,
            rels,
            ..
        } => format!(
            "ExpandInto {}",
            rel_text(var(*from), var(*to), *direction, rels, schema)
        ),
        OpDesc::AspJoin {
            from,
            to,
            direction,
            rels,
            retain,
            ..
        } => format!(
            "AspJoin{} {}",
            if retain.is_some() { " (retain)" } else { "" },
            rel_text(var(*from), var(*to), *direction, rels, schema)
        ),
        OpDesc::MultiwayIntersect {
            seed,
            seed_dir,
            seed_step,
            probe,
            probe_dir,
            probe_step,
            chunk,
            ..
        } => {
            let close = var(stage.chunk_slots[*chunk][0]);
            format!(
                "MultiwayIntersect {} & {}",
                rel_text(
                    var(*seed),
                    close,
                    *seed_dir,
                    std::slice::from_ref(seed_step),
                    schema
                ),
                rel_text(
                    var(*probe),
                    close,
                    *probe_dir,
                    std::slice::from_ref(probe_step),
                    schema
                )
            )
        }
        OpDesc::Filter { expr, .. } => format!("Filter {}", expr_text(expr, query)),
        OpDesc::Unwind { expr, chunk } => format!(
            "Unwind {} AS {}",
            expr_text(expr, query),
            var(stage.chunk_slots[*chunk][0])
        ),
        OpDesc::TableFunction {
            func, rel, chunk, ..
        } => {
            let table = schema
                .rel_by_id(*rel)
                .map(|r| r.name.as_str())
                .unwrap_or("?");
            let cols: Vec<&str> = stage.chunk_slots[*chunk].iter().map(|&s| var(s)).collect();
            format!("Call {}({table}) YIELD {}", func.name(), cols.join(", "))
        }
        OpDesc::OptionalBegin => "OptionalBegin".into(),
        OpDesc::OptionalEnd { chunks, .. } => {
            let slots: Vec<usize> = chunks
                .iter()
                .flat_map(|&c| stage.chunk_slots[c].iter().copied())
                .collect();
            format!("Optional {}", slot_names(&slots, query))
        }
    }
}

fn sink_name(sink: &SinkDef) -> String {
    let mut name = if sink.aggregate {
        "Aggregate".to_string()
    } else {
        "Project".to_string()
    };
    for op in &sink.post {
        name.push_str(match op {
            PostOp::Distinct => " + Distinct",
            PostOp::Filter(_) => " + Filter",
            PostOp::Sort(_) => " + Sort",
            PostOp::Skip(_) => " + Skip",
            PostOp::Limit(_) => " + Limit",
        });
    }
    name
}

/// Total order over values for grouping, DISTINCT, and ORDER BY:
/// nulls first, then booleans, numbers (int and float compare
/// numerically), strings, nodes, rels, lists, records.
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
                // A temporal value sorts after the strings and before
                // the references, and two of different kinds sort by
                // kind, because a date and a duration have no order
                // between them and the total order still owes an
                // answer.
                Value::Temporal(_) => 4,
                Value::Node { .. } => 5,
                Value::Rel { .. } => 6,
                Value::List(_) | Value::Chain(_) => 7,
                // A record sorts after every list, and two records
                // sort by their fields, name first and then value.
                // The order between two records is not a question ISO
                // answers, but DISTINCT and ORDER BY have to answer
                // it, so it is answered here and stated rather than
                // left to whatever the enum's declaration order was.
                Value::Record(_) => 8,
                // A path sorts after every record, and two paths sort
                // by their elements, which is the list order over the
                // same sequence. ISO orders two paths no more than it
                // orders two records, and the same reason applies.
                Value::Path(_) => 9,
            }
        }
        if matches!(self.0, Value::Chain(_)) || matches!(other.0, Value::Chain(_)) {
            return OrdValue(settle(self.0.clone())).cmp(&OrdValue(settle(other.0.clone())));
        }
        match (&self.0, &other.0) {
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).total_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.total_cmp(&(*b as f64)),
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Str(a), Value::Str(b)) => a.cmp(b),
            (Value::Temporal(a), Value::Temporal(b)) => temporal_key(a).cmp(&temporal_key(b)),
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
            (Value::Path(a), Value::Path(b)) => {
                for (x, y) in a.iter().zip(b) {
                    let ord = OrdValue(x.clone()).cmp(&OrdValue(y.clone()));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                a.len().cmp(&b.len())
            }
            (Value::Record(a), Value::Record(b)) => {
                for ((na, va), (nb, vb)) in a.iter().zip(b) {
                    let ord = na
                        .cmp(nb)
                        .then_with(|| OrdValue(va.clone()).cmp(&OrdValue(vb.clone())));
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
#[derive(Debug, Clone, Copy)]
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
        /// Degree mode, the count-to-degree rewrite: the sink counts
        /// this chunk and reads no value from it, so the expand sums
        /// neighbor counts into the chunk size and materializes
        /// nothing.
        degrees: bool,
        /// In degree mode, the unflattened source chunk the expand
        /// walks directly, one pull per upstream configuration; the
        /// chunk's multiplicity lives inside the degree sum from then
        /// on. `None` reads the flattened source position as usual.
        absorb: Option<usize>,
        /// False when nothing in the query reads the rel slot, the
        /// usual case for anonymous rels: the rel column stays empty
        /// and only the far column materializes. Compaction is safe
        /// because `Chunk::retain` walks each column by its own
        /// length.
        emit_rels: bool,
    },
    /// Variable-length expand: enumerates every path of `min..=max`
    /// hops under the path mode's repeat rule, one output value per
    /// path. The rel column holds the path as a list of rels. The
    /// default TRAIL is GQL's DIFFERENT EDGES semantics. This is the
    /// correctness-first DFS baseline; the RecursiveBFS frontier
    /// engine is milestone 4.
    VarExpand {
        from: usize,
        direction: RelDirection,
        rels: Vec<RelStep>,
        min: u64,
        max: Option<u64>,
        /// WALK allows any repeat, TRAIL forbids a repeated edge,
        /// ACYCLIC forbids a repeated node including the start.
        mode: PathMode,
        /// A SHORTEST selector restricts enumeration to minimum-hop
        /// paths per reached endpoint via a hop-level prepass.
        selector: Option<Selector>,
        /// Candidate tables of the endpoint variable; paths ending
        /// elsewhere are not emitted.
        to_tables: Vec<u32>,
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
    /// The ASP hash join (docs/07): a closing expand marked by the
    /// optimizer because its probe count exceeds the rel's edge count.
    /// The first pull accumulates each rel step's edge set into a hash
    /// table with one neighbors sweep, and every probe after that is a
    /// hash lookup instead of a storage read.
    AspJoin {
        from: usize,
        to: usize,
        direction: RelDirection,
        rels: Vec<RelStep>,
        chunk: usize,
        /// `Some(c)` is the fused semijoin: nothing reads the rel slot
        /// and nothing reads chunk `c` flat, so the probe retains the
        /// unflat neighbor vector of `c` in place per configuration
        /// and the flatten below it is gone. `None` probes one flat
        /// configuration at a time like `ExpandInto`.
        retain: Option<usize>,
    },
    /// The WCOJ closing step (docs/07 §4): an expand followed by an
    /// edge probe into its endpoint, fused into one galloping
    /// intersection of two sorted neighbor lists. Per flat
    /// configuration the intersection is the whole closing-node
    /// vector, so a wedge closes in one leapfrog walk instead of a
    /// list read plus a probe per candidate. The probe side's list is
    /// cached per node because it comes from an outer loop.
    MultiwayIntersect {
        /// The replaced expand's source, the middle of the wedge.
        seed: usize,
        seed_dir: RelDirection,
        seed_step: RelStep,
        /// The replaced probe's bound source, the wedge's other end.
        probe: usize,
        probe_dir: RelDirection,
        probe_step: RelStep,
        /// Columns `[to, seed rel, probe rel]`, born unflat.
        chunk: usize,
        /// False when nothing reads either rel slot, the anonymous-rel
        /// case: only the closing-node column materializes.
        emit_rels: bool,
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
    /// A table function source: one engine kernel call fills the chunk
    /// with every row at once, node column first. The kernel sweeps
    /// whole CSR tables, so the stage stays sequential and downstream
    /// operators iterate the one chunk.
    TableFunction {
        func: TableFunc,
        rel: u32,
        /// The rel's node domain, the table of the synthesized nodes.
        table: u32,
        args: Vec<BoundExpr>,
        chunk: usize,
    },
    /// Bottom of an OPTIONAL MATCH group: yields the current outer
    /// configuration exactly once per activation, so the group's
    /// operators exhaust per outer row and a miss is detectable.
    OptionalBegin,
    /// Top of an OPTIONAL MATCH group: passes group matches through,
    /// and when an outer configuration produced nothing, binds every
    /// chunk the group introduced to a single null row.
    OptionalEnd {
        /// Index of the matching `OptionalBegin` in the stage.
        begin: usize,
        /// Chunks introduced inside the group, nulled on a miss.
        chunks: Vec<usize>,
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
    /// The optimizer's row estimate for each operator, `None` where no
    /// logical operator owns it (the source, the optional brackets,
    /// the flattens the builder inserts on its own).
    est: Vec<Option<crate::optimizer::Estimate>>,
    /// Slots of each chunk, in column order.
    chunk_slots: Vec<Vec<usize>>,
    slot_loc: BTreeMap<usize, (usize, usize)>,
    /// Chunks still unflat when the sink runs, in creation order.
    unflat: Vec<usize>,
    sink: SinkDef,
}

struct StageBuilder {
    descs: Vec<OpDesc>,
    /// Estimates in lockstep with `descs`. Every push and every removal
    /// goes through [`StageBuilder::push`] and
    /// [`StageBuilder::remove`] so the two vectors cannot drift.
    est: Vec<Option<crate::optimizer::Estimate>>,
    /// The estimate operators pushed right now inherit: the logical
    /// operator being compiled owns everything the compiler emits for
    /// it, flattens included.
    cur_est: Option<crate::optimizer::Estimate>,
    chunk_slots: Vec<Vec<usize>>,
    chunk_flat: Vec<bool>,
    slot_loc: BTreeMap<usize, (usize, usize)>,
    /// The chunk a filter may still compact: the latest producer's,
    /// invalidated by any flatten between it and the filter.
    compactable: Option<usize>,
    flat: bool,
    /// The WCOJ fusion switch from [`Options`].
    wcoj: Wcoj,
    /// Path variable slot to the slots its shape reads: evaluating a
    /// path assembles from the pattern's slots, a read no expression
    /// walk can see.
    shapes: BTreeMap<usize, Vec<usize>>,
}

impl StageBuilder {
    fn new(flat: bool, wcoj: Wcoj, shapes: BTreeMap<usize, Vec<usize>>) -> Self {
        StageBuilder {
            descs: Vec::new(),
            est: Vec::new(),
            cur_est: None,
            chunk_slots: Vec::new(),
            chunk_flat: Vec::new(),
            slot_loc: BTreeMap::new(),
            compactable: None,
            flat,
            wcoj,
            shapes,
        }
    }

    /// Appends an operator, tagging it with the estimate of whichever
    /// logical operator is being compiled.
    fn push(&mut self, desc: OpDesc) {
        self.descs.push(desc);
        self.est.push(self.cur_est);
    }

    fn remove(&mut self, ix: usize) {
        self.descs.remove(ix);
        self.est.remove(ix);
    }

    /// Closes a slot set over path shapes: a path variable this stage
    /// assembles (its slot bound by no operator here) reads every slot
    /// of its shape. A path bound by an earlier stage is an ordinary
    /// column and expands nothing.
    fn expand_shapes(&self, out: &mut BTreeSet<usize>) {
        let paths: Vec<usize> = out
            .iter()
            .copied()
            .filter(|s| !self.slot_loc.contains_key(s) && self.shapes.contains_key(s))
            .collect();
        for path in paths {
            out.extend(self.shapes[&path].iter().copied());
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
            self.push(OpDesc::Flatten { chunk });
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
        self.expand_shapes(&mut slots);
        let mut chunks = BTreeSet::new();
        for slot in slots {
            let Some(&(c, _)) = self.slot_loc.get(&slot) else {
                if self.shapes.contains_key(&slot) {
                    continue;
                }
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
        BoundExpr::IsTyped { expr, .. } => expr_slots(expr, out),
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
        BoundExpr::Path(elements) => {
            for element in elements {
                expr_slots(element, out);
            }
        }
        BoundExpr::Cast { expr, .. } => expr_slots(expr, out),
    }
}

fn input_of(plan: &LogicalPlan) -> Option<&LogicalPlan> {
    match plan {
        LogicalPlan::Empty => None,
        LogicalPlan::ScanNodes { input, .. }
        | LogicalPlan::Expand { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::TableFunction { input, .. }
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

/// The OPTIONAL MATCH group of a plan operator, `None` for required
/// operators and for operators that never carry a group.
fn optional_group(op: &LogicalPlan) -> Option<usize> {
    match op {
        LogicalPlan::ScanNodes { optional, .. }
        | LogicalPlan::Expand { optional, .. }
        | LogicalPlan::Filter { optional, .. } => *optional,
        _ => None,
    }
}

/// Whether the operator after an expand is an edge probe into that
/// expand's endpoint that the WCOJ fusion can absorb: a single-step
/// fixed-direction closing expand in the same optional group whose
/// other end is already bound, carrying the optimizer's mark unless
/// the mode forces the fusion. Returns the probe's rel slot, source
/// slot, direction, and rel step, and flattens the source's chunk so
/// the fused operator reads one configuration per pull.
fn wcoj_fusion(
    b: &mut StageBuilder,
    lookahead: Option<&LogicalPlan>,
    to: usize,
    optional: Option<usize>,
    query: &BoundQuery,
    schema: &Schema,
) -> Result<Option<(usize, usize, RelDirection, RelStep)>> {
    let Some(LogicalPlan::Expand {
        rel,
        from,
        to: to2,
        direction,
        range: None,
        into: true,
        wcoj,
        optional: opt2,
        ..
    }) = lookahead
    else {
        return Ok(None);
    };
    if *opt2 != optional || !(*wcoj || b.wcoj == Wcoj::Force) {
        return Ok(None);
    }
    let steps = rel_steps(*rel, query, schema)?;
    let [step] = steps[..] else {
        return Ok(None);
    };
    if !usable_side(&step, *direction) {
        return Ok(None);
    }
    // The close either points into the expand's endpoint from a bound
    // source, or the DP oriented it outward from the endpoint. The
    // mirrored form probes the bound end's lists in the flipped
    // direction, the same sorted reverse index a reversed expand
    // reads, so both shapes fuse into the one intersection.
    let (probe, probe_dir) = if *to2 == to && b.slot_loc.contains_key(from) {
        (*from, *direction)
    } else if *from == to && b.slot_loc.contains_key(to2) {
        let flipped = match direction {
            RelDirection::Out => RelDirection::In,
            RelDirection::In => RelDirection::Out,
            RelDirection::Undirected => RelDirection::Undirected,
        };
        (*to2, flipped)
    } else {
        return Ok(None);
    };
    let probe_chunk = b.slot_loc[&probe].0;
    b.ensure_flat(probe_chunk);
    Ok(Some((*rel, probe, probe_dir, step)))
}

/// Compiles one ScanNodes, Expand, or Filter into the builder.
/// `lookahead` is the next linear operator, offered for IndexLookup
/// fusion; returns true when it was fused and the caller must skip it.
/// Fusion requires the filter to share the scan's group, otherwise an
/// optional `{id: k}` filter would fuse into a required scan and turn
/// the left-outer group inner.
fn compile_match_op(
    b: &mut StageBuilder,
    op: &LogicalPlan,
    lookahead: Option<&LogicalPlan>,
    query: &BoundQuery,
    schema: &Schema,
) -> Result<bool> {
    match op {
        LogicalPlan::ScanNodes { slot, optional, .. } => {
            let tables = query.variables[*slot].node_tables.clone();
            if tables.is_empty() {
                return Err(invalid(format!(
                    "variable '{}' has no candidate node tables",
                    query.variables[*slot].name
                )));
            }
            let fused = lookahead.and_then(|next| match next {
                LogicalPlan::Filter {
                    expr,
                    optional: fopt,
                    ..
                } if fopt == optional => index_key(expr, *slot),
                _ => None,
            });
            let consumed = fused.is_some();
            let chunk = b.new_chunk(vec![*slot], false);
            if let Some(key) = fused {
                b.push(OpDesc::IndexLookup { tables, key, chunk });
            } else {
                b.push(OpDesc::Scan { tables, chunk });
            }
            b.produced(chunk);
            Ok(consumed)
        }
        LogicalPlan::Expand {
            rel,
            from,
            to,
            direction,
            range,
            into,
            asp,
            optional,
            ..
        } => {
            let rels = rel_steps(*rel, query, schema)?;
            let from_chunk = b
                .slot_loc
                .get(from)
                .map(|&(c, _)| c)
                .ok_or_else(|| invalid(format!("expand from unbound slot {from}")))?;
            b.ensure_flat(from_chunk);
            // The WCOJ fusion: this expand's endpoint is probed by the
            // very next operator, so the pair collapses into one
            // galloping intersection. Restricted to single-step rels
            // with a fixed direction; anything else falls through to
            // the binary pair, which stays the correctness baseline.
            if b.wcoj != Wcoj::Off
                && range.is_none()
                && !*into
                && rels.len() == 1
                && usable_side(&rels[0], *direction)
                && let Some(fused) = wcoj_fusion(b, lookahead, *to, *optional, query, schema)?
            {
                let (rel2, probe, probe_dir, probe_step) = fused;
                let chunk = b.new_chunk(vec![*to, *rel, rel2], false);
                b.push(OpDesc::MultiwayIntersect {
                    seed: *from,
                    seed_dir: *direction,
                    seed_step: rels[0],
                    probe,
                    probe_dir,
                    probe_step,
                    chunk,
                    emit_rels: true,
                });
                b.produced(chunk);
                return Ok(true);
            }
            if let Some(v) = range {
                if *into {
                    return Err(invalid(
                        "variable-length patterns into a bound endpoint do not execute yet".into(),
                    ));
                }
                let to_tables = query.variables[*to].node_tables.clone();
                if to_tables.is_empty() {
                    return Err(invalid(format!(
                        "variable '{}' has no candidate node tables",
                        query.variables[*to].name
                    )));
                }
                let chunk = b.new_chunk(vec![*to, *rel], false);
                b.push(OpDesc::VarExpand {
                    from: *from,
                    direction: *direction,
                    rels,
                    min: v.min.unwrap_or(1),
                    max: v.max,
                    mode: v.mode,
                    selector: v.selector,
                    to_tables,
                    chunk,
                });
                b.produced(chunk);
            } else if *into {
                let to_chunk = b
                    .slot_loc
                    .get(to)
                    .map(|&(c, _)| c)
                    .ok_or_else(|| invalid(format!("expand into unbound slot {to}")))?;
                b.ensure_flat(to_chunk);
                // The probe result is a single edge, born flat.
                let chunk = b.new_chunk(vec![*rel], true);
                if *asp {
                    // The retain fusion is decided by the stage-level
                    // rewrite once every downstream reference is known.
                    b.push(OpDesc::AspJoin {
                        from: *from,
                        to: *to,
                        direction: *direction,
                        rels,
                        chunk,
                        retain: None,
                    });
                } else {
                    b.push(OpDesc::ExpandInto {
                        from: *from,
                        to: *to,
                        direction: *direction,
                        rels,
                        chunk,
                    });
                }
            } else {
                let chunk = b.new_chunk(vec![*to, *rel], false);
                b.push(OpDesc::Expand {
                    from: *from,
                    direction: *direction,
                    rels,
                    chunk,
                    degrees: false,
                    absorb: None,
                    emit_rels: true,
                });
                b.produced(chunk);
            }
            Ok(false)
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
            b.push(OpDesc::Filter {
                expr: expr.clone(),
                compact,
            });
            Ok(false)
        }
        _ => unreachable!("compile_match_op only sees pattern operators"),
    }
}

/// Compiles one OPTIONAL MATCH group: flattens for the outer chunks
/// the group reads, then `OptionalBegin`, the group's operators, and
/// the `OptionalEnd` that binds nulls on a miss. Returns the linear
/// index just past the group.
fn compile_optional_group(
    b: &mut StageBuilder,
    linear: &[&LogicalPlan],
    start: usize,
    query: &BoundQuery,
    schema: &Schema,
    est: &[crate::optimizer::Estimate],
) -> Result<usize> {
    let group = optional_group(linear[start]);
    let mut end = start + 1;
    while end < linear.len() && optional_group(linear[end]) == group {
        end += 1;
    }
    // Every outer chunk the group reads must flatten below the
    // boundary, so one `OptionalBegin` activation is exactly one outer
    // configuration and a miss is detectable per outer row. Slots the
    // group introduces itself are not bound yet and fall through.
    let mut read = BTreeSet::new();
    for op in &linear[start..end] {
        match op {
            LogicalPlan::Expand { from, to, into, .. } => {
                read.insert(*from);
                if *into {
                    read.insert(*to);
                }
            }
            LogicalPlan::Filter { expr, .. } => expr_slots(expr, &mut read),
            _ => {}
        }
    }
    b.expand_shapes(&mut read);
    for slot in read {
        if let Some(&(c, _)) = b.slot_loc.get(&slot) {
            b.ensure_flat(c);
        }
    }
    let begin = b.descs.len();
    // The brackets belong to no logical operator, so they carry no
    // estimate and the profile leaves their column blank.
    b.cur_est = None;
    b.push(OpDesc::OptionalBegin);
    // Nothing below the boundary may compact through the group, and
    // nothing above may compact through the `OptionalEnd`.
    b.compactable = None;
    let first_chunk = b.chunk_slots.len();
    let mut i = start;
    while i < end {
        let lookahead = if i + 1 < end {
            Some(linear[i + 1])
        } else {
            None
        };
        b.cur_est = est.get(i).copied();
        let before = b.descs.len();
        if compile_match_op(b, linear[i], lookahead, query, schema)? {
            let fused = est.get(i + 1).copied();
            for slot in &mut b.est[before..] {
                *slot = fused;
            }
            i += 1;
        }
        i += 1;
    }
    b.cur_est = None;
    b.push(OpDesc::OptionalEnd {
        begin,
        chunks: (first_chunk..b.chunk_slots.len()).collect(),
    });
    b.compactable = None;
    Ok(end)
}

/// Slots one operator reads.
fn desc_refs(desc: &OpDesc, out: &mut BTreeSet<usize>) {
    match desc {
        OpDesc::IndexLookup { key, .. } => expr_slots(key, out),
        OpDesc::Expand { from, .. } | OpDesc::VarExpand { from, .. } => {
            out.insert(*from);
        }
        OpDesc::ExpandInto { from, to, .. } | OpDesc::AspJoin { from, to, .. } => {
            out.insert(*from);
            out.insert(*to);
        }
        OpDesc::MultiwayIntersect { seed, probe, .. } => {
            out.insert(*seed);
            out.insert(*probe);
        }
        OpDesc::Filter { expr, .. } | OpDesc::Unwind { expr, .. } => expr_slots(expr, out),
        OpDesc::TableFunction { args, .. } => args.iter().for_each(|e| expr_slots(e, out)),
        OpDesc::Source
        | OpDesc::RowSource { .. }
        | OpDesc::Scan { .. }
        | OpDesc::Flatten { .. }
        | OpDesc::OptionalBegin
        | OpDesc::OptionalEnd { .. } => {}
    }
}

/// Slots the sink's post operators read.
fn post_refs(post: &[PostOp], out: &mut BTreeSet<usize>) {
    for op in post {
        match op {
            PostOp::Filter(e) | PostOp::Skip(e) | PostOp::Limit(e) => expr_slots(e, out),
            PostOp::Sort(keys) => keys.iter().for_each(|(e, _)| expr_slots(e, out)),
            PostOp::Distinct => {}
        }
    }
}

/// Dead-column and count-to-degree rewrites over a finished stage.
///
/// First, any fixed-length `Expand` whose rel slot nothing in the
/// query reads stops materializing its rel column, the usual case for
/// anonymous rels in a pattern.
///
/// Then the count-to-degree rewrite (docs/11 B4): when the sink
/// aggregates and the stage's last producer is a fixed-length `Expand`
/// whose output chunk nothing reads except at most one bare
/// non-distinct count argument, the expand switches to degree mode and
/// that count becomes a star count over the preserved multiplicity.
/// When the expand's flattened source chunk is also read by nothing
/// else, its `Flatten` drops too and the expand walks the unflattened
/// source directly; the absorbed chunk keeps its flat marking so the
/// sink's multiplicity product skips it, its contribution now living
/// inside the degree sum. Runs after the sink's own flattens, so a key
/// or filter that reads either chunk has already shown up in the
/// reference walk and blocks the rewrite.
fn rewrite_count_expand(
    b: &mut StageBuilder,
    items: &[BoundItem],
    aggs: &mut [AggSpec],
    post: &[PostOp],
    extra: &BTreeSet<usize>,
    aggregate: bool,
) {
    // Optional groups hold absolute operator indices and their own
    // null semantics; leave their stages alone.
    if b.descs
        .iter()
        .any(|d| matches!(d, OpDesc::OptionalBegin | OpDesc::OptionalEnd { .. }))
    {
        return;
    }
    // Every slot anything reads, aggregate arguments included: a rel
    // slot outside this set never gets evaluated, so its column can
    // stay empty.
    let mut full_refs = BTreeSet::new();
    for desc in &b.descs {
        desc_refs(desc, &mut full_refs);
    }
    for item in items {
        expr_slots(&item.expr, &mut full_refs);
    }
    post_refs(post, &mut full_refs);
    full_refs.extend(extra.iter().copied());
    b.expand_shapes(&mut full_refs);
    for desc in &mut b.descs {
        if let OpDesc::Expand {
            chunk, emit_rels, ..
        } = desc
            && !full_refs.contains(&b.chunk_slots[*chunk][1])
        {
            *emit_rels = false;
        }
        // The intersect carries two rel slots; both must be dead for
        // the columns to stay empty, and the usual case is exactly
        // that, two anonymous rels closing a wedge.
        if let OpDesc::MultiwayIntersect {
            chunk, emit_rels, ..
        } = desc
            && !full_refs.contains(&b.chunk_slots[*chunk][1])
            && !full_refs.contains(&b.chunk_slots[*chunk][2])
        {
            *emit_rels = false;
        }
    }
    // The AspJoin retain fusion, the semijoin half of the ASP triple:
    // when nothing reads the join's rel slot and nothing but the join
    // reads the probed chunk, the flatten directly below the join
    // drops and the probe retains the whole neighbor vector in place,
    // so the closing edge check never enumerates configurations. The
    // chunk goes back to unflat and the sink's multiplicity product
    // counts the survivors. Skipped in flat mode, whose whole point is
    // forcing tuple-at-a-time execution as the differential oracle.
    while !b.flat {
        let mut fused = false;
        for t in 0..b.descs.len() {
            let OpDesc::AspJoin {
                from,
                to,
                chunk,
                retain: None,
                ..
            } = b.descs[t]
            else {
                continue;
            };
            let f = match (t > 0).then(|| &b.descs[t - 1]) {
                Some(&OpDesc::Flatten { chunk: f }) => f,
                _ => continue,
            };
            if b.slot_loc.get(&to).map(|&(c, _)| c) != Some(f)
                || b.slot_loc.get(&from).map(|&(c, _)| c) == Some(f)
                || full_refs.contains(&b.chunk_slots[chunk][0])
            {
                continue;
            }
            // The retain filters chunk `f`'s vector in place, so every
            // probe pull must refill it: the producer has to sit
            // directly below the removed flatten. With another flatten
            // in between, several probe rows share one fill and one
            // row's survivors would narrow the next row's probe.
            let refills = matches!(
                (t > 1).then(|| &b.descs[t - 2]),
                Some(
                    OpDesc::Expand { chunk: p, .. } | OpDesc::VarExpand { chunk: p, .. }
                ) if *p == f
            );
            if !refills {
                continue;
            }
            // References excluding the join itself: its own flat read
            // of `to` is exactly what the fusion removes.
            let mut others = BTreeSet::new();
            for (ix, desc) in b.descs.iter().enumerate() {
                if ix != t {
                    desc_refs(desc, &mut others);
                }
            }
            for item in items {
                expr_slots(&item.expr, &mut others);
            }
            post_refs(post, &mut others);
            others.extend(extra.iter().copied());
            b.expand_shapes(&mut others);
            if b.chunk_slots[f].iter().any(|s| others.contains(s)) {
                continue;
            }
            if let OpDesc::AspJoin { retain, .. } = &mut b.descs[t] {
                *retain = Some(f);
            }
            b.remove(t - 1);
            b.chunk_flat[f] = false;
            fused = true;
            break;
        }
        if !fused {
            break;
        }
    }
    if !aggregate {
        return;
    }
    let Some(target) = b
        .descs
        .iter()
        .rposition(|d| !matches!(d, OpDesc::Flatten { .. }))
    else {
        return;
    };
    let OpDesc::Expand { from, chunk: c, .. } = b.descs[target] else {
        return;
    };
    // Flat mode, or something read the chunk flat: no factorized count
    // to serve.
    if b.chunk_flat[c] {
        return;
    }
    // Every slot something other than this expand reads.
    let mut refs = BTreeSet::new();
    for (ix, desc) in b.descs.iter().enumerate() {
        if ix != target {
            desc_refs(desc, &mut refs);
        }
    }
    for item in items.iter().filter(|it| !it.aggregate) {
        expr_slots(&item.expr, &mut refs);
    }
    post_refs(post, &mut refs);
    refs.extend(extra.iter().copied());
    b.expand_shapes(&mut refs);
    // Aggregates whose argument touches the expand's chunk: at most
    // one, and it must be a bare non-distinct count of one of the
    // chunk's slots. Every other argument counts as a reference.
    let mut counting = Vec::new();
    for (ix, spec) in aggs.iter().enumerate() {
        let Some(arg) = &spec.arg else { continue };
        let mut arg_refs = BTreeSet::new();
        expr_slots(arg, &mut arg_refs);
        b.expand_shapes(&mut arg_refs);
        if arg_refs.iter().any(|s| b.chunk_slots[c].contains(s)) {
            counting.push(ix);
        } else {
            refs.extend(arg_refs);
        }
    }
    if b.chunk_slots[c].iter().any(|s| refs.contains(s)) {
        return;
    }
    match counting.as_slice() {
        [] => {}
        &[ix] => {
            let spec = &mut aggs[ix];
            let bare = matches!(&spec.arg, Some(BoundExpr::Var(s)) if b.chunk_slots[c].contains(s));
            if spec.func != Func::Count || spec.distinct || !bare {
                return;
            }
            spec.star = true;
            spec.arg = None;
            spec.arg_chunk = None;
        }
        _ => return,
    }
    let mut absorb = None;
    if target > 0
        && let OpDesc::Flatten { chunk: f } = b.descs[target - 1]
        && b.slot_loc.get(&from).map(|&(fc, _)| fc) == Some(f)
        && !b.chunk_slots[f].iter().any(|s| refs.contains(s))
    {
        absorb = Some(f);
    }
    if let OpDesc::Expand {
        degrees, absorb: a, ..
    } = &mut b.descs[target]
    {
        *degrees = true;
        *a = absorb;
    }
    if absorb.is_some() {
        b.remove(target - 1);
    }
}

/// The estimator's reader, over the same storage the query is about to
/// run on and the same parameter values it was called with. Every
/// answer is a lookup or an offsets subtraction, so asking costs about
/// what one point lookup costs and there is at most one of those per
/// pinned slot per plan.
struct GraphProbe<'a> {
    graph: &'a mut dyn Graph,
    params: &'a [Value],
}

impl crate::optimizer::Probe for GraphProbe<'_> {
    fn seed(&mut self, table: u32, key: &BoundExpr) -> Option<u64> {
        let key = match key {
            BoundExpr::Literal(Literal::Int(i)) => *i,
            BoundExpr::Param(ix) => match self.params.get(*ix)? {
                Value::Int(i) => *i,
                _ => return None,
            },
            _ => return None,
        };
        self.graph
            .lookup_key(table, u64::try_from(key).ok()?)
            .ok()
            .flatten()
    }

    fn degree(&mut self, rel: u32, node: u64, reversed: bool) -> Option<u64> {
        self.graph.degree(rel, node, reversed).ok()
    }
}

fn build_stages(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
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

    let shapes: BTreeMap<usize, Vec<usize>> = query
        .path_shapes
        .iter()
        .map(|(&slot, parts)| {
            let read = parts
                .iter()
                .map(|p| match p {
                    PathPart::Node(s) | PathPart::Rel(s) | PathPart::VarRel(s) => *s,
                })
                .collect();
            (slot, read)
        })
        .collect();
    // One estimate per linearized operator, in the same bottom-up
    // order, so `est[i]` belongs to `linear[i]`. The optimizer is the
    // only thing that knows these numbers and it does not write them
    // onto the plan, so ask it again here. This time it gets a reader,
    // which the join ordering never does: a plan is cached across
    // parameter values and a seed's real degree is not the same for two
    // of them, but by here the values are in hand and the numbers
    // EXPLAIN ANALYZE prints are about this run.
    let mut probe = GraphProbe { graph, params };
    let est = crate::optimizer::probed_estimates(plan, query, schema, Some(&mut probe));

    let mut stages = Vec::new();
    let mut b = StageBuilder::new(options.flat, options.wcoj, shapes.clone());
    b.push(OpDesc::Source);

    let mut i = 0;
    while i < linear.len() {
        b.cur_est = est.get(i).copied();
        if optional_group(linear[i]).is_some() {
            i = compile_optional_group(&mut b, &linear, i, query, schema, &est)?;
            continue;
        }
        match linear[i] {
            LogicalPlan::Empty => unreachable!("Empty never appears in the linearized ops"),
            LogicalPlan::ScanNodes { .. }
            | LogicalPlan::Expand { .. }
            | LogicalPlan::Filter { .. } => {
                let before = b.descs.len();
                if compile_match_op(&mut b, linear[i], linear.get(i + 1).copied(), query, schema)? {
                    // The lookahead filter fused into the scan, so the
                    // one operator left standing produces the filtered
                    // count, not the scan's.
                    let fused = est.get(i + 1).copied();
                    for slot in &mut b.est[before..] {
                        *slot = fused;
                    }
                    i += 1;
                }
            }
            LogicalPlan::Unwind { expr, slot, .. } => {
                for c in b.unflat_of(expr)? {
                    b.ensure_flat(c);
                }
                let chunk = b.new_chunk(vec![*slot], false);
                b.push(OpDesc::Unwind {
                    expr: expr.clone(),
                    chunk,
                });
                b.produced(chunk);
            }
            LogicalPlan::TableFunction {
                func,
                rel,
                table,
                args,
                slots,
                ..
            } => {
                let chunk = b.new_chunk(slots.clone(), false);
                b.push(OpDesc::TableFunction {
                    func: *func,
                    rel: *rel,
                    table: *table,
                    args: args.clone(),
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

                rewrite_count_expand(&mut b, &items, &mut aggs, &post, &extra, aggregate);

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
                    est: std::mem::take(&mut b.est),
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
                    b = StageBuilder::new(options.flat, options.wcoj, shapes.clone());
                    let chunk = b.new_chunk(slots, false);
                    b.push(OpDesc::RowSource { chunk });
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

/// Per-operator counters, cumulative over the run. `nanos` includes
/// child time; the profile subtracts the child to report self time,
/// which is exact because every stage is one linear chain.
#[derive(Debug, Clone, Copy, Default)]
struct OpStats {
    pulls: u64,
    rows: u64,
    /// The rows those values stand for once the factorization is
    /// multiplied out, from [`flat_rows`]. This is the number an
    /// estimate is judged against.
    flat: u64,
    nanos: u64,
}

/// Hasher for the accumulated edge sets of an ASP join. SipHash costs
/// more than the whole probe on `(u64, u64)` keys, so edges hash with
/// one multiply-xor round per word; row offsets are not adversarial
/// input.
#[derive(Default)]
struct EdgeHasher(u64);

impl std::hash::Hasher for EdgeHasher {
    fn write(&mut self, _: &[u8]) {
        unreachable!("edge keys hash as u64 words");
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = (self.0 ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn finish(&self) -> u64 {
        self.0 ^ (self.0 >> 32)
    }
}

type EdgeSet = std::collections::HashSet<(u64, u64), std::hash::BuildHasherDefault<EdgeHasher>>;

/// One unit of parallel work: a row range of one node table, handed
/// to the stage's driving scan. Ranges are multiples of the 2048-tuple
/// target, which divides every power-of-two node-group size, so a
/// morsel never spans groups and each worker's reader decodes a group
/// exactly once.
#[derive(Debug, Clone, Copy)]
struct Morsel {
    table: u32,
    start: u64,
    end: u64,
}

struct StageCtx<'a> {
    graph: &'a mut dyn Graph,
    params: &'a [Value],
    counts: &'a BTreeMap<u32, u64>,
    slot_loc: &'a BTreeMap<usize, (usize, usize)>,
    /// Path variable shapes from the binder, the assembly recipe
    /// behind [`assemble_path`]. No operator produces a path slot.
    path_shapes: &'a BTreeMap<usize, Vec<PathPart>>,
    chunks: Vec<Chunk>,
    states: Vec<OpState>,
    rows: Vec<Vec<Value>>,
    /// Row-context values that shadow the chunks: projection aliases
    /// during materialization, the row snapshot during post filters.
    overlay: BTreeMap<usize, Value>,
    scratch: Vec<u64>,
    /// The accumulated edge sets of each ASP join, keyed by operator
    /// index and built on the join's first pull, one set per rel step
    /// in storage orientation. Deliberately outside `states` so an
    /// optional group's rearm never throws the accumulate away, and
    /// kept across morsels so a worker accumulates once per query.
    edge_sets: BTreeMap<usize, Vec<EdgeSet>>,
    /// Each intersect's cached probe-side adjacency, keyed by operator
    /// index: the probe node comes from an outer loop, so consecutive
    /// configurations reread the same sorted list, and caching it
    /// makes the probe side one storage read per node instead of one
    /// per pair. Outside `states` for the same rearm reason as the
    /// edge sets.
    isect: BTreeMap<usize, ProbeSide>,
    /// The row range a parallel worker's driving scan is bounded to,
    /// `None` on sequential runs. Only the scan at operator index 1
    /// consults it; any later scan in the pipeline still iterates its
    /// whole domain per pull.
    morsel: Option<Morsel>,
    /// One entry per operator when profiling, empty otherwise.
    stats: Vec<OpStats>,
    /// The chunks still unflat at each operator, from [`live_unflat`].
    /// Filled only when profiling, and only so the flat row count can
    /// be accumulated.
    live: Vec<Vec<usize>>,
    /// Conditions raised while running this stage that did not stop it.
    /// Drained into the result by whoever owns the stage. Empty on
    /// every query that raises nothing, which is nearly all of them.
    notices: Vec<DiagnosticRecord>,
}

fn value_of(ctx: &mut StageCtx, slot: usize) -> Result<Value> {
    if let Some(v) = ctx.overlay.get(&slot) {
        return Ok(v.clone());
    }
    if let Some(&(c, col)) = ctx.slot_loc.get(&slot) {
        let chunk = &ctx.chunks[c];
        let Some(pos) = chunk.cur else {
            return Err(invalid(
                "read of an unflattened vector, the planner missed a flatten".into(),
            ));
        };
        return Ok(chunk.cols[col][pos].clone());
    }
    let shapes = ctx.path_shapes;
    if let Some(parts) = shapes.get(&slot) {
        return assemble_path(ctx, parts);
    }
    Err(invalid(format!("slot {slot} is not bound in this stage")))
}

/// Materializes a path variable from its recorded shape: the
/// alternating node and rel list of docs/07 §5, read straight from the
/// pattern's slots at eval time. A var-length slot holds a PMR chain;
/// its hops splice in as rel, node pairs with the final node dropped
/// because the pattern's next node part supplies it. Any null part
/// nulls the whole path, the OPTIONAL MATCH contract.
fn assemble_path(ctx: &mut StageCtx, parts: &[PathPart]) -> Result<Value> {
    let mut out = Vec::new();
    for part in parts {
        match part {
            PathPart::Node(slot) | PathPart::Rel(slot) => match value_of(ctx, *slot)? {
                Value::Null => return Ok(Value::Null),
                v => out.push(v),
            },
            PathPart::VarRel(slot) => match value_of(ctx, *slot)? {
                Value::Null => return Ok(Value::Null),
                Value::Chain(link) => {
                    let mut hops = Vec::new();
                    let mut cur = Some(&link);
                    while let Some(l) = cur {
                        if let Some(rel) = &l.rel {
                            hops.push((rel.clone(), l.node.clone()));
                        }
                        cur = l.prev.as_ref();
                    }
                    hops.reverse();
                    let last = hops.len().saturating_sub(1);
                    for (ix, (rel, node)) in hops.into_iter().enumerate() {
                        out.push(rel);
                        if ix < last {
                            out.push(node);
                        }
                    }
                }
                other => {
                    return Err(invalid(format!(
                        "path assembly expects a PMR chain, got {other:?}"
                    )));
                }
            },
        }
    }
    // Not `Value::path`: a matched path is joined by construction, the
    // pattern having walked the edges it names, so re-deriving that on
    // every row would pay for a check that cannot fail. GE06 is where
    // the elements come from the query instead and the check earns its
    // cost.
    debug_assert!(
        !out.len().is_multiple_of(2),
        "a matched path alternates: {out:?}"
    );
    Ok(Value::Path(out))
}

fn node_value(v: Value, what: &str) -> Result<(u32, u64)> {
    match v {
        Value::Node { table, offset } => Ok((table, offset)),
        other => Err(invalid(format!("{what} expects a node, got {other:?}"))),
    }
}

/// Total neighbor count of one node across the applicable rel steps:
/// the degree-mode expand's replacement for list materialization,
/// mirroring the direction and table matching of the value path.
fn degree_sum(
    graph: &mut dyn Graph,
    rels: &[RelStep],
    direction: RelDirection,
    table: u32,
    offset: u64,
) -> Result<u64> {
    let mut total = 0;
    for step in rels {
        if matches!(direction, RelDirection::Out | RelDirection::Undirected)
            && table == step.from_table
        {
            total += graph.degree(step.id, offset, false)?;
        }
        if matches!(direction, RelDirection::In | RelDirection::Undirected)
            && table == step.to_table
        {
            total += graph.degree(step.id, offset, true)?;
        }
    }
    Ok(total)
}

/// The chunks still unflat at each operator, replayed off the final
/// operator list so no bookkeeping can drift out of sync with the
/// fusions that rewrite it.
///
/// This is what turns a per-pull vector width into a row count.
/// `rows` counts the values one operator emitted, but under
/// factorization the rows those values stand for is the product over
/// every vector still unflat beside them, which is exactly the
/// cartesian product the sink walks. On a plain chain the set is the
/// producer's own chunk and the product is the vector width again, so
/// nothing changes; on a star, where one hop's vector stays unflat
/// while the next hop runs, it is the difference between counting the
/// second hop's neighbors and counting the paths.
fn live_unflat(descs: &[OpDesc]) -> Vec<Vec<usize>> {
    let mut unflat: BTreeSet<usize> = BTreeSet::new();
    let mut out = Vec::with_capacity(descs.len());
    for desc in descs {
        match desc {
            // A flatten pins one position, so the chunk stops
            // multiplying from here up.
            OpDesc::Flatten { chunk } => {
                unflat.remove(chunk);
            }
            // The retained probe hands its chunk back unflat: the
            // survivors are a vector again.
            OpDesc::AspJoin {
                retain: Some(f), ..
            } => {
                unflat.insert(*f);
            }
            // An absorbing expand reads the whole source vector in one
            // pull and puts the sum over all of it in its own chunk, so
            // the source stops multiplying here even though no flatten
            // stands in front of it. The fusion removed that flatten.
            OpDesc::Expand {
                chunk,
                absorb: Some(f),
                ..
            } => {
                unflat.remove(f);
                unflat.insert(*chunk);
            }
            OpDesc::RowSource { chunk }
            | OpDesc::Scan { chunk, .. }
            | OpDesc::IndexLookup { chunk, .. }
            | OpDesc::Expand { chunk, .. }
            | OpDesc::VarExpand { chunk, .. }
            | OpDesc::ExpandInto { chunk, .. }
            | OpDesc::AspJoin { chunk, .. }
            | OpDesc::MultiwayIntersect { chunk, .. }
            | OpDesc::Unwind { chunk, .. }
            | OpDesc::TableFunction { chunk, .. } => {
                unflat.insert(*chunk);
            }
            OpDesc::Source | OpDesc::Filter { .. } | OpDesc::OptionalBegin => {}
            OpDesc::OptionalEnd { .. } => {}
        }
        out.push(unflat.iter().copied().collect());
    }
    out
}

/// The rows one successful pull stands for: the product of every
/// vector still unflat at this operator. An empty set is one row,
/// which is what a flatten or the source produces.
fn flat_rows(ctx: &StageCtx, i: usize) -> u64 {
    ctx.live[i]
        .iter()
        .map(|&c| ctx.chunks[c].size as u64)
        .product()
}

/// Whether this operator's flat row count is an output cardinality an
/// estimate can be judged against. The source, the flattens, and the
/// optional brackets pass rows through without producing any, so
/// their count belongs to whatever is under them.
fn counts_rows(desc: &OpDesc) -> bool {
    !matches!(
        desc,
        OpDesc::Source
            | OpDesc::Flatten { .. }
            | OpDesc::OptionalBegin
            | OpDesc::OptionalEnd { .. }
    )
}

/// How many values one successful pull produced: chunk producers
/// report their chunk size, a compacting filter the surviving size,
/// and pass-through operators one configuration.
fn produced_rows(descs: &[OpDesc], ctx: &StageCtx, i: usize) -> u64 {
    match &descs[i] {
        OpDesc::Source | OpDesc::Flatten { .. } => 1,
        OpDesc::Filter { compact: None, .. } => 1,
        OpDesc::OptionalBegin | OpDesc::OptionalEnd { .. } => 1,
        OpDesc::Filter {
            compact: Some(c), ..
        } => ctx.chunks[*c].size as u64,
        OpDesc::AspJoin {
            retain: Some(f), ..
        } => ctx.chunks[*f].size as u64,
        OpDesc::RowSource { chunk }
        | OpDesc::Scan { chunk, .. }
        | OpDesc::IndexLookup { chunk, .. }
        | OpDesc::Expand { chunk, .. }
        | OpDesc::VarExpand { chunk, .. }
        | OpDesc::ExpandInto { chunk, .. }
        | OpDesc::AspJoin { chunk, .. }
        | OpDesc::MultiwayIntersect { chunk, .. }
        | OpDesc::Unwind { chunk, .. }
        | OpDesc::TableFunction { chunk, .. } => ctx.chunks[*chunk].size as u64,
    }
}

/// The static shape of one variable-length expansion, shared by every
/// recursive call of [`enumerate_paths`]. `levels` is set when a
/// SHORTEST selector is active: the minimum hop count of every node
/// reachable from the start, and the DFS then only takes hops that
/// advance exactly one level, which restricts it to the shortest-path
/// DAG. Minimum-hop paths never repeat a node (a repeat would shortcut
/// to a shorter path), so the mode's repeat rule is vacuous under a
/// selector and is skipped.
struct VarSpec<'a> {
    rels: &'a [RelStep],
    direction: RelDirection,
    to_tables: &'a [u32],
    min: u64,
    max: Option<u64>,
    mode: PathMode,
    levels: Option<&'a BTreeMap<(u32, u64), u64>>,
}

/// Every hop leaving `(table, offset)` across the pattern's rel steps
/// and direction: the rel value, the far table, and the far offset, in
/// storage order, which keeps enumeration deterministic.
fn hop_edges(
    ctx: &mut StageCtx,
    rels: &[RelStep],
    direction: RelDirection,
    table: u32,
    offset: u64,
) -> Result<Vec<(Value, u32, u64)>> {
    let mut hops: Vec<(Value, u32, u64)> = Vec::new();
    let mut nbrs = Vec::new();
    for step in rels {
        if matches!(direction, RelDirection::Out | RelDirection::Undirected)
            && table == step.from_table
        {
            ctx.graph.neighbors(step.id, offset, false, &mut nbrs)?;
            for &dst in &nbrs {
                hops.push((
                    Value::Rel {
                        table: step.id,
                        src: offset,
                        dst,
                    },
                    step.to_table,
                    dst,
                ));
            }
        }
        if matches!(direction, RelDirection::In | RelDirection::Undirected)
            && table == step.to_table
        {
            ctx.graph.neighbors(step.id, offset, true, &mut nbrs)?;
            for &src in &nbrs {
                hops.push((
                    Value::Rel {
                        table: step.id,
                        src,
                        dst: offset,
                    },
                    step.from_table,
                    src,
                ));
            }
        }
    }
    Ok(hops)
}

/// Depth-first path enumeration for `VarExpand`: every path of
/// `min..=max` hops from the start node under the mode's repeat rule,
/// WALK unrestricted (the binder guarantees a bound), TRAIL with no
/// repeated edge, ACYCLIC with no repeated node. A path whose endpoint
/// sits in `to_tables` emits one node value and its PMR chain, which
/// is one `Arc` clone of the current link; sibling branches share every
/// ancestor link, so emitting all paths costs one link per path, not
/// one edge list per path. The chain doubles as the visited set for
/// the repeat rules; paths stay short enough that the linear chain
/// scan beats a hash set.
fn enumerate_paths(
    ctx: &mut StageCtx,
    spec: &VarSpec,
    table: u32,
    offset: u64,
    link: &Arc<PathLink>,
    far: &mut Vec<Value>,
    trails: &mut Vec<Value>,
) -> Result<()> {
    let depth = link.hops;
    if depth >= spec.min && spec.to_tables.contains(&table) {
        far.push(Value::Node { table, offset });
        trails.push(Value::Chain(link.clone()));
    }
    if spec.max.is_some_and(|m| depth >= m) {
        return Ok(());
    }
    for (rel_val, next_table, next_offset) in
        hop_edges(ctx, spec.rels, spec.direction, table, offset)?
    {
        if let Some(levels) = spec.levels {
            if levels.get(&(next_table, next_offset)) != Some(&(depth + 1)) {
                continue;
            }
        } else {
            let repeats = match spec.mode {
                PathMode::Walk => false,
                PathMode::Trail => chain_has_rel(link, &rel_val),
                PathMode::Acyclic => chain_has_node(link, next_table, next_offset),
            };
            if repeats {
                continue;
            }
        }
        let child = Arc::new(PathLink {
            prev: Some(link.clone()),
            rel: Some(rel_val),
            node: Value::Node {
                table: next_table,
                offset: next_offset,
            },
            hops: depth + 1,
        });
        enumerate_paths(ctx, spec, next_table, next_offset, &child, far, trails)?;
    }
    Ok(())
}

/// The root of a PMR chain: zero hops at the start node.
fn chain_root(table: u32, offset: u64) -> Arc<PathLink> {
    Arc::new(PathLink {
        prev: None,
        rel: None,
        node: Value::Node { table, offset },
        hops: 0,
    })
}

/// The breadth-first prepass behind the SHORTEST selectors: minimum
/// hop counts from the start node within the hop window, the first
/// discovered parent hop per node, and nodes in discovery order.
/// Frontiers expand in discovery order and neighbors in storage order,
/// so levels, parents, and order are all deterministic. ANY SHORTEST
/// walks the parent chain for one canonical path per endpoint; ALL
/// SHORTEST hands the level map to [`enumerate_paths`].
struct HopLevels {
    levels: BTreeMap<(u32, u64), u64>,
    parents: BTreeMap<(u32, u64), (Value, u32, u64)>,
    order: Vec<(u32, u64)>,
}

fn hop_levels(
    ctx: &mut StageCtx,
    rels: &[RelStep],
    direction: RelDirection,
    max: Option<u64>,
    table: u32,
    offset: u64,
) -> Result<HopLevels> {
    let mut bfs = HopLevels {
        levels: BTreeMap::new(),
        parents: BTreeMap::new(),
        order: vec![(table, offset)],
    };
    bfs.levels.insert((table, offset), 0);
    let mut frontier = vec![(table, offset)];
    let mut depth = 0u64;
    while !frontier.is_empty() && max.is_none_or(|m| depth < m) {
        depth += 1;
        let mut next = Vec::new();
        for (t, o) in frontier {
            for (rel_val, nt, no) in hop_edges(ctx, rels, direction, t, o)? {
                if bfs.levels.contains_key(&(nt, no)) {
                    continue;
                }
                bfs.levels.insert((nt, no), depth);
                bfs.parents.insert((nt, no), (rel_val, t, o));
                bfs.order.push((nt, no));
                next.push((nt, no));
            }
        }
        frontier = next;
    }
    Ok(bfs)
}

/// One probe against the accumulated edge sets of an ASP join,
/// mirroring the direction and endpoint table checks of the storage
/// probe in `ExpandInto`.
fn asp_hit(
    sets: &[EdgeSet],
    rels: &[RelStep],
    direction: RelDirection,
    ft: u32,
    fo: u64,
    tt: u32,
    to_off: u64,
) -> Option<Value> {
    for (step, set) in rels.iter().zip(sets) {
        if matches!(direction, RelDirection::Out | RelDirection::Undirected)
            && ft == step.from_table
            && tt == step.to_table
            && set.contains(&(fo, to_off))
        {
            return Some(Value::Rel {
                table: step.id,
                src: fo,
                dst: to_off,
            });
        }
        if matches!(direction, RelDirection::In | RelDirection::Undirected)
            && ft == step.to_table
            && tt == step.from_table
            && set.contains(&(to_off, fo))
        {
            return Some(Value::Rel {
                table: step.id,
                src: to_off,
                dst: fo,
            });
        }
    }
    None
}

/// Whether one end of the intersection has stored lists to gallop for
/// this direction. A fixed direction reads one list and always does; an
/// undirected end reads both, which only lines up when the rel is self
/// referencing and the two lists hold the same node table.
fn usable_side(step: &RelStep, direction: RelDirection) -> bool {
    !matches!(direction, RelDirection::Undirected) || step.from_table == step.to_table
}

/// The stored lists one end of the intersection reads, and the table
/// its far end lands in. A fixed direction is one list; an undirected
/// end is both, forward first, which is the order the plain undirected
/// expand emits in. Undirected only answers for a self-referencing rel,
/// where both stored lists hold the same node table.
fn near_sides(
    step: &RelStep,
    direction: RelDirection,
    table: u32,
) -> Option<(&'static [bool], u32)> {
    match direction {
        RelDirection::Out if table == step.from_table => Some((&[false], step.to_table)),
        RelDirection::In if table == step.to_table => Some((&[true], step.from_table)),
        RelDirection::Undirected
            if step.from_table == step.to_table && table == step.from_table =>
        {
            Some((&[false, true], step.to_table))
        }
        _ => None,
    }
}

/// The probe side of one intersection, cached per probe node.
///
/// `all` is what the leapfrog walks: the stored list for a fixed
/// direction, the union of both for an undirected end. `out` is the
/// forward list on its own, kept only in the undirected case so an
/// emitted rel can be oriented the way the binary probe would have
/// oriented it.
struct ProbeSide {
    key: (u32, u64),
    all: Vec<u64>,
    out: Vec<u64>,
}

impl ProbeSide {
    /// A cache no probe node can match, so the first pull reads.
    fn empty() -> Self {
        ProbeSide {
            key: (u32::MAX, u64::MAX),
            all: Vec::new(),
            out: Vec::new(),
        }
    }
}

/// Union of two ascending lists into `out`, ascending and deduplicated.
/// The intersection's probe side is an existence check, so a value both
/// stored lists hold is one entry here and one hit there.
fn merge_sorted(a: &[u64], b: &[u64], out: &mut Vec<u64>) {
    out.clear();
    out.reserve(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let v = a[i].min(b[j]);
        while i < a.len() && a[i] == v {
            i += 1;
        }
        while j < b.len() && b[j] == v {
            j += 1;
        }
        out.push(v);
    }
    for &v in &a[i..] {
        if out.last() != Some(&v) {
            out.push(v);
        }
    }
    for &v in &b[j..] {
        if out.last() != Some(&v) {
            out.push(v);
        }
    }
}

/// First index at or after `from` whose value is at least `target`, in
/// a sorted list: an exponential probe brackets the answer, a binary
/// search pins it. This is the galloping step of the intersection,
/// sublinear in the gap it skips.
fn gallop(list: &[u64], target: u64, from: usize) -> usize {
    let mut span = 1;
    while from + span < list.len() && list[from + span] < target {
        span <<= 1;
    }
    let lo = from + (span >> 1);
    let hi = (from + span).min(list.len());
    lo + list[lo..hi].partition_point(|&v| v < target)
}

/// Leapfrog intersection of two sorted lists, galloping both sides.
/// Emits one hit per occurrence in `seed`: the seed list is the
/// replaced expand's output, where a duplicate is a real multi-edge
/// row, while the probe side is an edge-existence check exactly like
/// the binary probe it replaces.
fn leapfrog(seed: &[u64], probe: &[u64], mut emit: impl FnMut(u64)) {
    let (mut si, mut pi) = (0, 0);
    while si < seed.len() && pi < probe.len() {
        let (sv, pv) = (seed[si], probe[pi]);
        if sv < pv {
            si = gallop(seed, pv, si);
        } else if pv < sv {
            pi = gallop(probe, sv, pi);
        } else {
            while si < seed.len() && seed[si] == sv {
                emit(sv);
                si += 1;
            }
            pi += 1;
        }
    }
}

/// The pull entry point: a plain `step` normally, a timed and counted
/// one when the context carries stats. Recursive pulls inside `step`
/// come back through here, so every operator gets counted.
fn next(descs: &[OpDesc], ctx: &mut StageCtx, i: usize) -> Result<bool> {
    if ctx.stats.is_empty() {
        return step(descs, ctx, i);
    }
    let start = Instant::now();
    let got = step(descs, ctx, i)?;
    let nanos = start.elapsed().as_nanos() as u64;
    let rows = if got { produced_rows(descs, ctx, i) } else { 0 };
    let flat = if got { flat_rows(ctx, i) } else { 0 };
    let s = &mut ctx.stats[i];
    s.nanos += nanos;
    if got {
        s.pulls += 1;
        s.rows += rows;
        s.flat = s.flat.saturating_add(flat);
    }
    Ok(got)
}

fn step(descs: &[OpDesc], ctx: &mut StageCtx, i: usize) -> Result<bool> {
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
            let morsel = if i == 1 { ctx.morsel } else { None };
            if !ctx.states[i].active {
                if !next(descs, ctx, i - 1)? {
                    return Ok(false);
                }
                ctx.states[i] = OpState {
                    active: true,
                    offset: morsel.map_or(0, |m| m.start),
                    ..OpState::default()
                };
            }
            let mut vals = Vec::with_capacity(VECTOR_SIZE);
            if let Some(m) = morsel {
                let st = &mut ctx.states[i];
                while vals.len() < VECTOR_SIZE && st.offset < m.end {
                    vals.push(Value::Node {
                        table: m.table,
                        offset: st.offset,
                    });
                    st.offset += 1;
                }
            } else {
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
                let Some(offset) = ctx.graph.lookup_key(table, k)? else {
                    continue;
                };
                if offset < *ctx.counts.get(&table).unwrap_or(&0) {
                    vals.push(Value::Node { table, offset });
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
            degrees: false,
            emit_rels,
            ..
        } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            // A null source, from an optional miss upstream, matches
            // nothing.
            let v = value_of(ctx, *from)?;
            if matches!(v, Value::Null) {
                continue;
            }
            let (table, offset) = node_value(v, "expand")?;
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
                        if *emit_rels {
                            rel_vals.push(Value::Rel {
                                table: step.id,
                                src: offset,
                                dst,
                            });
                        }
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
                        if *emit_rels {
                            rel_vals.push(Value::Rel {
                                table: step.id,
                                src,
                                dst: offset,
                            });
                        }
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
        // Degree mode: nothing reads this chunk's values, so the
        // expand sums neighbor counts into the chunk size and
        // materializes no lists. With `absorb` the source chunk never
        // flattened and the expand walks it whole, one pull per
        // upstream configuration, batching the whole vector into one
        // storage call per step and direction.
        OpDesc::Expand {
            from,
            direction,
            rels,
            chunk,
            degrees: true,
            absorb,
            ..
        } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let mut total = 0u64;
            match absorb {
                None => {
                    let v = value_of(ctx, *from)?;
                    if matches!(v, Value::Null) {
                        continue;
                    }
                    let (table, offset) = node_value(v, "expand")?;
                    total = degree_sum(&mut *ctx.graph, rels, *direction, table, offset)?;
                }
                Some(f) => {
                    let col = ctx
                        .slot_loc
                        .get(from)
                        .map(|&(_, col)| col)
                        .expect("expand from a bound slot");
                    let StageCtx {
                        graph,
                        chunks,
                        scratch,
                        ..
                    } = ctx;
                    let source = &chunks[*f];
                    for step in rels {
                        let sides = [
                            (
                                matches!(direction, RelDirection::Out | RelDirection::Undirected),
                                step.from_table,
                                false,
                            ),
                            (
                                matches!(direction, RelDirection::In | RelDirection::Undirected),
                                step.to_table,
                                true,
                            ),
                        ];
                        for (applies, step_table, reversed) in sides {
                            if !applies {
                                continue;
                            }
                            scratch.clear();
                            for pos in 0..source.size {
                                match &source.cols[col][pos] {
                                    Value::Null => {}
                                    &Value::Node { table, offset } => {
                                        if table == step_table {
                                            scratch.push(offset);
                                        }
                                    }
                                    other => {
                                        return Err(invalid(format!(
                                            "expand expects a node, got {other:?}"
                                        )));
                                    }
                                }
                            }
                            total += graph.degree_sum(step.id, scratch, reversed)?;
                        }
                    }
                }
            }
            if total == 0 {
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = total as usize;
            for col in &mut c.cols {
                col.clear();
            }
            c.cur = None;
            return Ok(true);
        },
        OpDesc::VarExpand {
            from,
            direction,
            rels,
            min,
            max,
            mode,
            selector,
            to_tables,
            chunk,
        } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let v = value_of(ctx, *from)?;
            if matches!(v, Value::Null) {
                continue;
            }
            let (table, offset) = node_value(v, "var expand")?;
            let mut far = Vec::new();
            let mut trails = Vec::new();
            match selector {
                Some(Selector::AnyShortest) => {
                    // Chains build in discovery order, so a node's
                    // parent chain always exists before its own and
                    // every endpoint's path is one `Arc` clone.
                    let bfs = hop_levels(ctx, rels, *direction, *max, table, offset)?;
                    let mut chains: BTreeMap<(u32, u64), Arc<PathLink>> = BTreeMap::new();
                    chains.insert((table, offset), chain_root(table, offset));
                    for &(t, o) in &bfs.order {
                        if let Some((rel_val, pt, po)) = bfs.parents.get(&(t, o)) {
                            let parent = chains[&(*pt, *po)].clone();
                            let hops = parent.hops + 1;
                            chains.insert(
                                (t, o),
                                Arc::new(PathLink {
                                    prev: Some(parent),
                                    rel: Some(rel_val.clone()),
                                    node: Value::Node {
                                        table: t,
                                        offset: o,
                                    },
                                    hops,
                                }),
                            );
                        }
                        if bfs.levels[&(t, o)] < *min || !to_tables.contains(&t) {
                            continue;
                        }
                        far.push(Value::Node {
                            table: t,
                            offset: o,
                        });
                        trails.push(Value::Chain(chains[&(t, o)].clone()));
                    }
                }
                Some(Selector::AllShortest) => {
                    let bfs = hop_levels(ctx, rels, *direction, *max, table, offset)?;
                    let spec = VarSpec {
                        rels,
                        direction: *direction,
                        to_tables,
                        min: *min,
                        max: *max,
                        mode: *mode,
                        levels: Some(&bfs.levels),
                    };
                    let root = chain_root(table, offset);
                    enumerate_paths(ctx, &spec, table, offset, &root, &mut far, &mut trails)?;
                }
                None => {
                    let spec = VarSpec {
                        rels,
                        direction: *direction,
                        to_tables,
                        min: *min,
                        max: *max,
                        mode: *mode,
                        levels: None,
                    };
                    let root = chain_root(table, offset);
                    enumerate_paths(ctx, &spec, table, offset, &root, &mut far, &mut trails)?;
                }
            }
            if far.is_empty() {
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = far.len();
            c.cols[0] = far;
            c.cols[1] = trails;
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
            let fv = value_of(ctx, *from)?;
            let tv = value_of(ctx, *to)?;
            if matches!(fv, Value::Null) || matches!(tv, Value::Null) {
                continue;
            }
            let (ft, fo) = node_value(fv, "expand into")?;
            let (tt, to_off) = node_value(tv, "expand into")?;
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
        OpDesc::AspJoin {
            from,
            to,
            direction,
            rels,
            chunk,
            retain,
        } => {
            // Accumulate on the first pull: one neighbors sweep per
            // rel step in storage orientation, so every probe after
            // this is a hash lookup instead of a storage read.
            if !ctx.edge_sets.contains_key(&i) {
                let mut sets = Vec::with_capacity(rels.len());
                for step in rels {
                    let mut set = EdgeSet::default();
                    let count = *ctx.counts.get(&step.from_table).unwrap_or(&0);
                    let StageCtx { graph, scratch, .. } = ctx;
                    for src in 0..count {
                        graph.neighbors(step.id, src, false, scratch)?;
                        for &dst in scratch.iter() {
                            set.insert((src, dst));
                        }
                    }
                    sets.push(set);
                }
                ctx.edge_sets.insert(i, sets);
            }
            match retain {
                // Flat probe, one configuration at a time like
                // `ExpandInto`.
                None => loop {
                    if !next(descs, ctx, i - 1)? {
                        return Ok(false);
                    }
                    let fv = value_of(ctx, *from)?;
                    let tv = value_of(ctx, *to)?;
                    if matches!(fv, Value::Null) || matches!(tv, Value::Null) {
                        continue;
                    }
                    let (ft, fo) = node_value(fv, "asp join")?;
                    let (tt, to_off) = node_value(tv, "asp join")?;
                    let sets = ctx.edge_sets.get(&i).expect("accumulated above");
                    let Some(rel_val) = asp_hit(sets, rels, *direction, ft, fo, tt, to_off) else {
                        continue;
                    };
                    let c = &mut ctx.chunks[*chunk];
                    c.size = 1;
                    c.cols[0] = vec![rel_val];
                    c.cur = Some(0);
                    return Ok(true);
                },
                // The fused semijoin: probe the whole unflat neighbor
                // vector and retain the survivors in place.
                Some(f) => loop {
                    if !next(descs, ctx, i - 1)? {
                        return Ok(false);
                    }
                    let fv = value_of(ctx, *from)?;
                    if matches!(fv, Value::Null) {
                        continue;
                    }
                    let (ft, fo) = node_value(fv, "asp join")?;
                    let col = ctx
                        .slot_loc
                        .get(to)
                        .map(|&(_, col)| col)
                        .expect("join into a bound slot");
                    let sets = ctx.edge_sets.get(&i).expect("accumulated above");
                    let source = &ctx.chunks[*f];
                    let mut keep = Vec::with_capacity(source.size);
                    for pos in 0..source.size {
                        keep.push(match &source.cols[col][pos] {
                            Value::Null => false,
                            &Value::Node { table, offset } => {
                                asp_hit(sets, rels, *direction, ft, fo, table, offset).is_some()
                            }
                            other => {
                                return Err(invalid(format!(
                                    "asp join expects a node, got {other:?}"
                                )));
                            }
                        });
                    }
                    if !keep.iter().any(|k| *k) {
                        continue;
                    }
                    ctx.chunks[*f].retain(&keep);
                    // The rel column is dead by the fusion's own
                    // precondition; the chunk still pins one null so
                    // downstream sees a nonempty flat result.
                    let c = &mut ctx.chunks[*chunk];
                    if c.cols[0].is_empty() {
                        c.cols[0].push(Value::Null);
                    }
                    c.size = 1;
                    c.cur = Some(0);
                    return Ok(true);
                },
            }
        }
        OpDesc::MultiwayIntersect {
            seed,
            seed_dir,
            seed_step,
            probe,
            probe_dir,
            probe_step,
            chunk,
            emit_rels,
        } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let sv = value_of(ctx, *seed)?;
            let pv = value_of(ctx, *probe)?;
            if matches!(sv, Value::Null) || matches!(pv, Value::Null) {
                continue;
            }
            let (st, so) = node_value(sv, "multiway intersect")?;
            let (pt, po) = node_value(pv, "multiway intersect")?;
            let (Some((sdirs, sfar)), Some((pdirs, pfar))) = (
                near_sides(seed_step, *seed_dir, st),
                near_sides(probe_step, *probe_dir, pt),
            ) else {
                continue;
            };
            if sfar != pfar {
                continue;
            }
            let mut far = Vec::new();
            let mut seed_rels = Vec::new();
            let mut probe_rels = Vec::new();
            {
                let StageCtx {
                    graph,
                    scratch,
                    isect,
                    ..
                } = ctx;
                let cache = isect.entry(i).or_insert_with(ProbeSide::empty);
                if cache.key != (pt, po) {
                    match pdirs {
                        [prev] => {
                            graph.neighbors(probe_step.id, po, *prev, &mut cache.all)?;
                            cache.out.clear();
                        }
                        _ => {
                            graph.neighbors(probe_step.id, po, false, &mut cache.out)?;
                            graph.neighbors(probe_step.id, po, true, scratch)?;
                            merge_sorted(&cache.out, scratch, &mut cache.all);
                        }
                    }
                    cache.key = (pt, po);
                }
                let cache = &*cache;
                // An undirected end reads both stored lists. Walking
                // them one after the other, forward first, is the same
                // rows in the same order the expand this replaces
                // emitted them in, so nothing above the close sees the
                // fusion in its input.
                for &srev in sdirs {
                    graph.neighbors(seed_step.id, so, srev, scratch)?;
                    leapfrog(scratch, &cache.all, |v| {
                        far.push(Value::Node {
                            table: sfar,
                            offset: v,
                        });
                        if *emit_rels {
                            let (ss, sd) = if srev { (v, so) } else { (so, v) };
                            seed_rels.push(Value::Rel {
                                table: seed_step.id,
                                src: ss,
                                dst: sd,
                            });
                            // The binary probe an undirected close
                            // replaces reports the forward edge when
                            // there is one, so the merged list asks
                            // which side the hit came from.
                            let forward = match pdirs {
                                [prev] => !prev,
                                _ => cache.out.binary_search(&v).is_ok(),
                            };
                            let (ps, pd) = if forward { (po, v) } else { (v, po) };
                            probe_rels.push(Value::Rel {
                                table: probe_step.id,
                                src: ps,
                                dst: pd,
                            });
                        }
                    });
                }
            }
            if far.is_empty() {
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = far.len();
            c.cols[0] = far;
            c.cols[1] = seed_rels;
            c.cols[2] = probe_rels;
            c.cur = None;
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
            let items = match settle(eval(ctx, expr)?) {
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
        OpDesc::TableFunction {
            func,
            rel,
            table,
            args,
            chunk,
        } => {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let mut vals = Vec::with_capacity(args.len());
            for arg in args {
                vals.push(eval(ctx, arg)?);
            }
            // The sssp source arrives as a user-facing id; resolve it
            // to the dense offset the kernel walks.
            if *func == TableFunc::Sssp {
                let Some(Value::Int(key)) = vals.first() else {
                    return Err(invalid(format!(
                        "sssp's source must be a node id, got {:?}",
                        vals.first()
                    )));
                };
                let offset = u64::try_from(*key)
                    .ok()
                    .and_then(|k| ctx.graph.lookup_key(*table, k).transpose())
                    .transpose()?
                    .ok_or_else(|| invalid(format!("sssp source {key} names no node")))?;
                vals[0] = Value::Int(offset as i64);
            }
            let rows = ctx.graph.table_function(func.name(), *rel, &vals)?;
            if rows.is_empty() {
                return Ok(false);
            }
            let c = &mut ctx.chunks[*chunk];
            if rows.iter().any(|r| r.len() + 1 != c.cols.len()) {
                return Err(invalid(format!(
                    "{} returned a row with the wrong arity",
                    func.name()
                )));
            }
            c.size = rows.len();
            c.cols[0] = (0..rows.len())
                .map(|offset| Value::Node {
                    table: *table,
                    offset: offset as u64,
                })
                .collect();
            for (j, col) in c.cols.iter_mut().enumerate().skip(1) {
                *col = rows.iter().map(|r| r[j - 1].clone()).collect();
            }
            c.cur = None;
            Ok(true)
        }
        OpDesc::OptionalBegin => {
            if ctx.states[i].active {
                return Ok(false);
            }
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            ctx.states[i].active = true;
            Ok(true)
        }
        OpDesc::OptionalEnd { begin, chunks } => loop {
            if next(descs, ctx, i - 1)? {
                // `pos` doubles as the emitted flag for the current
                // outer configuration.
                ctx.states[i].pos = 1;
                return Ok(true);
            }
            if !ctx.states[*begin].active {
                // The begin never yielded: the outer input is done.
                return Ok(false);
            }
            let missed = ctx.states[i].pos == 0;
            // Rearm the group for the next outer configuration.
            for s in &mut ctx.states[*begin..i] {
                *s = OpState::default();
            }
            ctx.states[i].pos = 0;
            if missed {
                for &c in chunks {
                    let chunk = &mut ctx.chunks[c];
                    for col in &mut chunk.cols {
                        *col = vec![Value::Null];
                    }
                    chunk.size = 1;
                    chunk.cur = Some(0);
                }
                return Ok(true);
            }
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

/// Three-valued equality: `Ok(None)` when null is involved,
/// `Ok(Some(false))` across mismatched types.
///
/// The error arm is for the one comparison ISO refuses to answer
/// rather than answer falsely: two records whose fields differ. Every
/// other mismatch is a false, because a query that compares a number
/// with a string has asked a question with an answer.
fn cmp_eq(a: &Value, b: &Value) -> Result<Option<bool>> {
    Ok(match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x == y),
        (Value::Float(x), Value::Float(y)) => Some(x == y),
        (Value::Int(x), Value::Float(y)) => Some((*x as f64) == *y),
        (Value::Float(x), Value::Int(y)) => Some(*x == (*y as f64)),
        (Value::Bool(x), Value::Bool(y)) => Some(x == y),
        (Value::Str(x), Value::Str(y)) => Some(x == y),
        // Two temporal values are equal when they are the same kind
        // and the same count. A zoned value carries UTC, so two
        // spellings of one instant are equal and the offset each was
        // written with is not part of the comparison.
        (Value::Temporal(x), Value::Temporal(y)) => Some(same_instant(x, y)),
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
                return Ok(Some(false));
            }
            let mut saw_null = false;
            for (a, b) in x.iter().zip(y) {
                match cmp_eq(a, b)? {
                    Some(false) => return Ok(Some(false)),
                    Some(true) => {}
                    None => saw_null = true,
                }
            }
            if saw_null { None } else { Some(true) }
        }
        // 22G0U. Two records are comparable when they name the same
        // fields, and these do not, so there is no field by field
        // comparison to make. False would be the wrong answer here in
        // the way that matters: it is the answer a query gets when the
        // records differ in a value, and a query that misspelled a
        // field name would read it as data rather than as the mistake
        // it is.
        (Value::Record(x), Value::Record(y)) => {
            if !same_fields(x, y) {
                return Err(gql(
                    codes::C22G0U,
                    format!(
                        "a record with fields {} cannot be compared with one with fields {}",
                        field_names(x),
                        field_names(y)
                    ),
                ));
            }
            let mut saw_null = false;
            for ((_, a), (_, b)) in x.iter().zip(y) {
                match cmp_eq(a, b)? {
                    Some(false) => return Ok(Some(false)),
                    Some(true) => {}
                    None => saw_null = true,
                }
            }
            if saw_null { None } else { Some(true) }
        }
        // GA09. Two paths are the same path when they are the same walk,
        // so this is the element comparison a list gets and not an
        // identity: the path a pattern matched and the one GE06 built
        // out of the elements it matched are equal, which is what makes
        // a constructed path a value rather than a copy of one.
        (Value::Path(x), Value::Path(y)) => {
            if x.len() != y.len() {
                return Ok(Some(false));
            }
            for (a, b) in x.iter().zip(y) {
                if cmp_eq(a, b)? != Some(true) {
                    return Ok(Some(false));
                }
            }
            Some(true)
        }
        _ => Some(false),
    })
}

/// Whether two records name the same fields. Both lists are sorted by
/// name, so this is a walk rather than a set.
fn same_fields(x: &[(String, Value)], y: &[(String, Value)]) -> bool {
    x.len() == y.len() && x.iter().zip(y).all(|((a, _), (b, _))| a == b)
}

/// A record's field names for a message, in the order they are held.
fn field_names(fields: &[(String, Value)]) -> String {
    let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
    format!("({})", names.join(", "))
}

/// Whether two temporal values are the same value.
///
/// A zoned value is stored as the instant, so the offset is not
/// compared: `2024-01-15T10:00+07:00` and `2024-01-15T03:00Z` are one
/// instant written twice. Two of different kinds are not equal, and
/// that includes the two duration kinds, because no number of days is
/// a month.
fn same_instant(a: &Temporal, b: &Temporal) -> bool {
    match (a, b) {
        (Temporal::ZonedDatetime { nanos: x, .. }, Temporal::ZonedDatetime { nanos: y, .. }) => {
            x == y
        }
        (
            Temporal::ZonedTime {
                nanos: x,
                offset: ox,
            },
            Temporal::ZonedTime {
                nanos: y,
                offset: oy,
            },
        ) => zoned_time_key(*x, *ox) == zoned_time_key(*y, *oy),
        _ => a == b,
    }
}

/// A zoned time as an instant in the day, so two spellings of one
/// moment compare equal the way two zoned datetimes do.
fn zoned_time_key(nanos: i64, offset: i16) -> i64 {
    nanos - i64::from(offset) * 60 * 1_000_000_000
}

/// A temporal value as a sortable pair: which kind it is, then its
/// count. Two kinds have no order between them and the total order
/// still owes an answer, so the kind decides and the answer is stable.
fn temporal_key(t: &Temporal) -> (u8, i64) {
    match t {
        Temporal::Date(days) => (0, i64::from(*days)),
        Temporal::LocalTime(nanos) => (1, *nanos),
        Temporal::ZonedTime { nanos, offset } => (2, zoned_time_key(*nanos, *offset)),
        Temporal::LocalDatetime(nanos) => (3, *nanos),
        Temporal::ZonedDatetime { nanos, .. } => (4, *nanos),
        Temporal::Duration(DurationKind::YearMonth, count) => (5, *count),
        Temporal::Duration(DurationKind::DayTime, count) => (6, *count),
    }
}

/// Ordering for comparisons; `None` when null or incomparable types
/// are involved, which makes the comparison null.
fn cmp_ord(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        // Same kind orders by count, and two kinds do not order at
        // all: a date is not earlier or later than a duration, so the
        // comparison is null rather than an arbitrary answer.
        (Value::Temporal(x), Value::Temporal(y)) => {
            let (kx, cx) = temporal_key(x);
            let (ky, cy) = temporal_key(y);
            (kx == ky).then(|| cx.cmp(&cy))
        }
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
    if matches!(a, Value::Temporal(_)) || matches!(b, Value::Temporal(_)) {
        return temporal_arith(op, &a, &b);
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
                        return Err(divide_by_zero(op));
                    }
                    x.checked_div(y).ok_or_else(overflow)?
                }
                Mod => {
                    if y == 0 {
                        return Err(divide_by_zero(op));
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
                    // Approximate numerics divide by zero too. IEEE
                    // would answer infinity here, but GQL asks for
                    // 22012 whatever the numeric type is, and an engine
                    // that quietly answers inf has given a wrong answer
                    // rather than raised a condition.
                    Div | Mod if y == 0.0 => return Err(divide_by_zero(op)),
                    Div => x / y,
                    Mod => x % y,
                    _ => unreachable!("arith only sees arithmetic operators"),
                };
                Ok(Value::Float(r))
            }
            // Neither side is a number, so this is not arithmetic at
            // all. `22G03 invalid value type` is the condition for an
            // operand whose type the operator does not accept.
            _ => Err(gql(
                codes::C22G03,
                format!("cannot apply {op:?} to {a:?} and {b:?}"),
            )),
        },
    }
}

/// `22008 datetime field overflow`, for a result outside the calendar.
fn datetime_overflow(detail: String) -> ZuError {
    gql(codes::C22008, detail)
}

/// `22G14 invalid duration unit group`, for an operand whose unit group
/// the other side has nowhere to put.
fn wrong_unit_group(detail: String) -> ZuError {
    gql(codes::C22G14, detail)
}

/// Arithmetic where at least one side is temporal.
///
/// Only the combinations the standard defines are here, and the rest
/// are 22G03, because a date times a number is not an operation with a
/// sensible answer and inventing one is worse than refusing.
fn temporal_arith(op: BinaryOp, a: &Value, b: &Value) -> Result<Value> {
    use BinaryOp::*;
    let refuse = || {
        Err(gql(
            codes::C22G03,
            format!("cannot apply {op:?} to {a:?} and {b:?}"),
        ))
    };
    // A duration scaled by a number is the one mixed operation the
    // standard has, and the overflow it can produce is 22015 rather
    // than 22008: what ran out of room is the length, not a field of
    // any calendar.
    if let (Mul | Div, Value::Temporal(Temporal::Duration(kind, count)), factor)
    | (Mul, factor, Value::Temporal(Temporal::Duration(kind, count))) = (op, a, b)
    {
        let scaled = match (factor, op) {
            (Value::Int(n), Mul) => count.checked_mul(*n),
            (Value::Int(0), Div) => return Err(gql(codes::C22012, "division by zero".into())),
            (Value::Int(n), Div) => count.checked_div(*n),
            (Value::Float(f), Mul) => whole_nanos(*count as f64 * f),
            (Value::Float(f), Div) => whole_nanos(*count as f64 / f),
            _ => return refuse(),
        };
        let scaled = scaled.ok_or_else(|| {
            gql(
                codes::C22015,
                format!(
                    "{} does not scale by {factor:?} inside a duration",
                    Temporal::Duration(*kind, *count)
                ),
            )
        })?;
        return Ok(Value::Temporal(Temporal::Duration(*kind, scaled)));
    }
    let (Value::Temporal(x), Value::Temporal(y)) = (a, b) else {
        return refuse();
    };
    match (op, x, y) {
        // A duration on either side of a plus shifts the instant, and
        // a duration on the right of a minus shifts it back. A minus
        // with the duration on the left is not an operation: an
        // instant subtracted from a length of time means nothing.
        (Add, Temporal::Duration(kind, count), other)
        | (Add, other, Temporal::Duration(kind, count)) => shift(other, *kind, *count),
        (Sub, other, Temporal::Duration(kind, count)) => {
            let count = count.checked_neg().ok_or_else(|| {
                datetime_overflow(format!(
                    "{} has no negation",
                    Temporal::Duration(*kind, *count)
                ))
            })?;
            shift(other, *kind, count)
        }
        // Two instants of one kind subtract to the length between
        // them, which is a day-time duration because the calendar is
        // not involved in counting it.
        (Sub, Temporal::Date(p), Temporal::Date(q)) => Ok(Value::Temporal(Temporal::Duration(
            DurationKind::DayTime,
            (i64::from(*p) - i64::from(*q)) * temporal::NANOS_PER_DAY,
        ))),
        (Sub, Temporal::LocalDatetime(p), Temporal::LocalDatetime(q))
        | (Sub, Temporal::LocalTime(p), Temporal::LocalTime(q)) => Ok(Value::Temporal(
            Temporal::Duration(DurationKind::DayTime, p - q),
        )),
        (
            Sub,
            Temporal::ZonedDatetime { nanos: p, .. },
            Temporal::ZonedDatetime { nanos: q, .. },
        ) => Ok(Value::Temporal(Temporal::Duration(
            DurationKind::DayTime,
            p - q,
        ))),
        _ => refuse(),
    }
}

/// A scaled duration as whole units, `None` when the answer is not a
/// number or does not fit one.
fn whole_nanos(scaled: f64) -> Option<i64> {
    (scaled.is_finite() && scaled.abs() < 9.2e18).then(|| scaled.trunc() as i64)
}

/// An instant shifted by a duration of one kind.
///
/// The two kinds are not interchangeable and this is where that is
/// enforced. Months land on a date by naming the month and clamping
/// the day, and nanoseconds land on a date only when they are a whole
/// number of days, because a date has no room for an hour. That is
/// 22G14 and not a promotion to a datetime: an engine that promoted
/// answered a different question.
fn shift(instant: &Temporal, kind: DurationKind, count: i64) -> Result<Value> {
    let out = match (instant, kind) {
        (Temporal::Date(days), DurationKind::YearMonth) => temporal::add_months(*days, count)
            .map(Temporal::Date)
            .ok_or_else(|| {
                datetime_overflow(format!(
                    "{} shifted by {} leaves the calendar",
                    Temporal::Date(*days),
                    Temporal::Duration(kind, count)
                ))
            })?,
        (Temporal::Date(days), DurationKind::DayTime) => {
            if count % temporal::NANOS_PER_DAY != 0 {
                return Err(wrong_unit_group(format!(
                    "{} has a time of day and {} has nowhere to put one",
                    Temporal::Duration(kind, count),
                    Temporal::Date(*days)
                )));
            }
            let shifted = i64::from(*days) + count / temporal::NANOS_PER_DAY;
            let shifted = i32::try_from(shifted)
                .ok()
                .filter(|d| (temporal::MIN_DAY..=temporal::MAX_DAY).contains(d));
            Temporal::Date(shifted.ok_or_else(|| {
                datetime_overflow(format!(
                    "{} shifted by {} leaves the calendar",
                    Temporal::Date(*days),
                    Temporal::Duration(kind, count)
                ))
            })?)
        }
        (Temporal::LocalTime(nanos), DurationKind::DayTime) => {
            Temporal::LocalTime((nanos + count).rem_euclid(temporal::NANOS_PER_DAY))
        }
        (Temporal::ZonedTime { nanos, offset }, DurationKind::DayTime) => Temporal::ZonedTime {
            nanos: (nanos + count).rem_euclid(temporal::NANOS_PER_DAY),
            offset: *offset,
        },
        (Temporal::LocalDatetime(nanos), DurationKind::DayTime) => {
            Temporal::LocalDatetime(add_nanos(*nanos, count)?)
        }
        (Temporal::ZonedDatetime { nanos, offset }, DurationKind::DayTime) => {
            Temporal::ZonedDatetime {
                nanos: add_nanos(*nanos, count)?,
                offset: *offset,
            }
        }
        // A datetime takes months by taking them on its date, which
        // keeps the time of day and clamps the day the same way.
        (Temporal::LocalDatetime(nanos), DurationKind::YearMonth) => {
            Temporal::LocalDatetime(add_months_to_nanos(*nanos, count)?)
        }
        (Temporal::ZonedDatetime { nanos, offset }, DurationKind::YearMonth) => {
            Temporal::ZonedDatetime {
                nanos: add_months_to_nanos(*nanos, count)?,
                offset: *offset,
            }
        }
        // Two durations of one kind add, and two of different kinds do
        // not, which is the rule the kinds exist for.
        (Temporal::Duration(have, count_a), _) if *have == kind => Temporal::Duration(
            kind,
            count_a.checked_add(count).ok_or_else(|| {
                datetime_overflow("the two durations do not add without overflowing".into())
            })?,
        ),
        (Temporal::Duration(have, _), _) => {
            return Err(wrong_unit_group(format!(
                "a {have:?} duration and a {kind:?} duration do not add"
            )));
        }
        (other, _) => {
            return Err(wrong_unit_group(format!(
                "{} cannot take {}",
                other.logical_type(),
                Temporal::Duration(kind, count)
            )));
        }
    };
    Ok(Value::Temporal(out))
}

/// An instant in nanoseconds shifted, refusing a result off the
/// calendar rather than wrapping into a year the type cannot spell.
fn add_nanos(nanos: i64, count: i64) -> Result<i64> {
    let out = nanos
        .checked_add(count)
        .ok_or_else(|| datetime_overflow("the shifted instant does not fit".into()))?;
    let days = out.div_euclid(temporal::NANOS_PER_DAY);
    if !(i64::from(temporal::MIN_DAY)..=i64::from(temporal::MAX_DAY)).contains(&days) {
        return Err(datetime_overflow(
            "the shifted instant leaves the calendar".into(),
        ));
    }
    Ok(out)
}

/// A datetime shifted by whole months: the date takes the months and
/// the time of day rides along unchanged.
fn add_months_to_nanos(nanos: i64, months: i64) -> Result<i64> {
    let days = nanos.div_euclid(temporal::NANOS_PER_DAY);
    let rest = nanos.rem_euclid(temporal::NANOS_PER_DAY);
    let days = i32::try_from(days)
        .ok()
        .and_then(|d| temporal::add_months(d, months))
        .ok_or_else(|| datetime_overflow("the shifted instant leaves the calendar".into()))?;
    Ok(i64::from(days) * temporal::NANOS_PER_DAY + rest)
}

/// `22012 data exception, division by zero`, for both `/` and `%`. The
/// standard's name says division, and the modulus of a zero divisor is
/// undefined for exactly the same reason.
fn divide_by_zero(op: BinaryOp) -> ZuError {
    let what = if matches!(op, BinaryOp::Mod) {
        "modulus"
    } else {
        "division"
    };
    gql(codes::C22012, format!("{what} by zero"))
}

fn eval(ctx: &mut StageCtx, expr: &BoundExpr) -> Result<Value> {
    match expr {
        BoundExpr::Literal(lit) => Ok(match lit {
            Literal::Null => Value::Null,
            Literal::Bool(b) => Value::Bool(*b),
            Literal::Int(i) => Value::Int(*i),
            Literal::Float(f) => Value::Float(*f),
            Literal::Str(s) => Value::Str(s.clone()),
            Literal::Temporal(t) => Value::Temporal(*t),
        }),
        BoundExpr::Param(ix) => Ok(ctx.params[*ix].clone()),
        BoundExpr::Var(slot) => value_of(ctx, *slot),
        BoundExpr::Property { base, key } => match eval(ctx, base)? {
            Value::Node { table, offset } => ctx.graph.property(table, offset, key),
            // A field the record does not have is null rather than an
            // error, which is what a property a node does not have
            // already answers. A record whose shape a query can rely
            // on is one the query declared, and that is what a cast to
            // a record type is for.
            ref record @ Value::Record(_) => Ok(record.field(key).cloned().unwrap_or(Value::Null)),
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
                    let l = settle(eval(ctx, lhs)?);
                    let r = settle(eval(ctx, rhs)?);
                    Ok(match cmp_eq(&l, &r)? {
                        Some(b) => Value::Bool(if *op == Eq { b } else { !b }),
                        None => Value::Null,
                    })
                }
                Lt | Le | Gt | Ge => {
                    let l = settle(eval(ctx, lhs)?);
                    let r = settle(eval(ctx, rhs)?);
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
                    let l = settle(eval(ctx, lhs)?);
                    let r = settle(eval(ctx, rhs)?);
                    arith(*op, l, r)
                }
                In => {
                    let l = settle(eval(ctx, lhs)?);
                    match settle(eval(ctx, rhs)?) {
                        Value::Null => Ok(Value::Null),
                        Value::List(items) => {
                            let mut saw_null = false;
                            for item in &items {
                                match cmp_eq(&l, item)? {
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
        BoundExpr::IsTyped { expr, ty, negated } => {
            let is_typed = crate::typed::is_of(&eval(ctx, expr)?, ty);
            Ok(Value::Bool(is_typed != *negated))
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
                    // The chain stores its length, so size(r) on a
                    // variable-length rel never materializes the list.
                    Value::Chain(link) => Ok(Value::Int(link.hops as i64)),
                    // A path answers with its element count, which is
                    // the count of the list it used to be, so a query
                    // written against that still gets the same number.
                    // PATH_LENGTH is the edge count and the other
                    // question.
                    Value::List(items) | Value::Path(items) => Ok(Value::Int(items.len() as i64)),
                    Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                    Value::Null => Ok(Value::Null),
                    other => Err(invalid(format!(
                        "size() expects a list or string, got {other:?}"
                    ))),
                }
            }
            // GF12. The same count as size(), over lists only: a
            // string has a length and not a cardinality, and answering
            // anyway would let a query that meant CHAR_LENGTH pass.
            Func::Cardinality => {
                if *star || args.len() != 1 {
                    return Err(invalid("cardinality() takes exactly one argument".into()));
                }
                match eval(ctx, &args[0])? {
                    Value::List(items) => Ok(Value::Int(items.len() as i64)),
                    Value::Null => Ok(Value::Null),
                    other => Err(invalid(format!(
                        "cardinality() expects a list, got {other:?}"
                    ))),
                }
            }
            // GF04. Edges, not elements: a path of two nodes has three
            // elements and a length of one, and a path of one node has
            // a length of zero.
            Func::PathLength => {
                if *star || args.len() != 1 {
                    return Err(invalid("path_length() takes exactly one argument".into()));
                }
                match eval(ctx, &args[0])? {
                    Value::Path(elements) => Ok(Value::Int((elements.len() / 2) as i64)),
                    Value::Null => Ok(Value::Null),
                    other => Err(invalid(format!(
                        "path_length() expects a path, got {other:?}"
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
        // GV45. The fields are evaluated in the order they were
        // written and sorted by name on the way in, so a record is one
        // value however the query spelled it.
        BoundExpr::Map(pairs) => {
            let mut fields = Vec::with_capacity(pairs.len());
            for (name, item) in pairs {
                fields.push((name.clone(), eval(ctx, item)?));
            }
            Ok(Value::record(fields))
        }
        // GE06. A null element nulls the whole path, the way a null
        // endpoint nulls a matched one under OPTIONAL MATCH: there is
        // no path with a hole in it.
        BoundExpr::Path(elements) => {
            let mut out = Vec::with_capacity(elements.len());
            for element in elements {
                match eval(ctx, element)? {
                    Value::Null => return Ok(Value::Null),
                    v => out.push(v),
                }
            }
            Value::path(out)
        }
        BoundExpr::Cast { expr, ty } => crate::cast::cast(eval(ctx, expr)?, ty),
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
    /// Whether a null was dropped on the way in. Set functions ignore
    /// nulls, and the standard wants the caller told that they did:
    /// `01G11 null value eliminated in set function`. A flag rather
    /// than a count, because the warning is raised once and the number
    /// of nulls is not part of it.
    nulls_eliminated: bool,
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
            Func::Id | Func::Size | Func::Cardinality | Func::PathLength => {
                unreachable!("scalar function as an aggregate")
            }
        };
        let distinct = (spec.distinct && !spec.star).then(BTreeSet::new);
        AggState {
            acc,
            distinct,
            nulls_eliminated: false,
        }
    }

    fn add_star(&mut self, mult: i64) {
        if let Acc::Count(n) = &mut self.acc {
            *n += mult;
        }
    }

    fn add(&mut self, v: Value, mult: i64) -> Result<()> {
        if matches!(v, Value::Null) {
            // count(*) never lands here: it has no argument, so it has
            // no null to eliminate. Every other set function does.
            self.nulls_eliminated = true;
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
                        return Err(gql(
                            codes::C22G03,
                            format!("sum() needs numbers, got {other:?}"),
                        ));
                    }
                };
                *acc = Some(match acc.take() {
                    None => scaled,
                    Some(prev) => arith(BinaryOp::Add, prev, scaled)?,
                });
            }
            Acc::Avg { sum, n } => {
                let x = as_f64(&v)
                    .ok_or_else(|| gql(codes::C22G03, format!("avg() needs numbers, got {v:?}")))?;
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

    /// Folds the partial state of a later morsel into this one. Merging
    /// morsel partials in morsel order keeps `collect()` identical to
    /// the sequential run; both states come from the same spec, so the
    /// variants always line up.
    fn merge(&mut self, other: AggState) -> Result<()> {
        self.nulls_eliminated |= other.nulls_eliminated;
        if let (Some(mine), Some(theirs)) = (&mut self.distinct, other.distinct) {
            mine.extend(theirs);
            return Ok(());
        }
        match (&mut self.acc, other.acc) {
            (Acc::Count(n), Acc::Count(m)) => *n += m,
            (Acc::Sum(a), Acc::Sum(b)) => {
                *a = match (a.take(), b) {
                    (Some(x), Some(y)) => Some(arith(BinaryOp::Add, x, y)?),
                    (x, y) => x.or(y),
                };
            }
            (Acc::Avg { sum, n }, Acc::Avg { sum: s2, n: n2 }) => {
                *sum += s2;
                *n += n2;
            }
            (Acc::Min(cur), Acc::Min(Some(v))) => {
                if cur
                    .as_ref()
                    .is_none_or(|c| OrdValue(v.clone()) < OrdValue(c.clone()))
                {
                    *cur = Some(v);
                }
            }
            (Acc::Max(cur), Acc::Max(Some(v))) => {
                if cur
                    .as_ref()
                    .is_none_or(|c| OrdValue(v.clone()) > OrdValue(c.clone()))
                {
                    *cur = Some(v);
                }
            }
            (Acc::Min(_), Acc::Min(None)) | (Acc::Max(_), Acc::Max(None)) => {}
            (Acc::Collect(items), Acc::Collect(more)) => items.extend(more),
            _ => unreachable!("morsel partials built from the same aggregate spec"),
        }
        Ok(())
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
        values.push(settle(eval(ctx, &item.expr)?));
    }
    for (item, v) in sink.items.iter().zip(&values) {
        if let Some(slot) = item_slot(item, query) {
            ctx.overlay.insert(slot, v.clone());
        }
    }
    let mut keys = Vec::new();
    for (expr, _) in sort_exprs(sink) {
        keys.push(OrdValue(settle(eval(ctx, expr)?)));
    }
    let mut extra = BTreeMap::new();
    for &slot in &sink.extra_slots {
        extra.insert(slot, settle(value_of(ctx, slot)?));
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
        keyvals.push(OrdValue(settle(eval(ctx, &item.expr)?)));
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
                let v = settle(eval(ctx, arg)?);
                state.add(v, mult)?;
            }
            Some(c) => {
                let size = ctx.chunks[c].size;
                let others = mult / size as i64;
                for pos in 0..size {
                    ctx.chunks[c].cur = Some(pos);
                    let v = settle(eval(ctx, arg)?);
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
            let state = sit.next().expect("one state per aggregate item");
            // Read the flag before finalize consumes the state. The
            // warning is per statement, not per group: notice() dedupes
            // by status, so a thousand groups that each dropped a null
            // report it once.
            if state.nulls_eliminated {
                ctx.notices.push(DiagnosticRecord::new(
                    codes::C01G11,
                    "a set function ignored one or more null arguments",
                ));
            }
            state.finalize()?
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

/// Evaluates a `SKIP` or `LIMIT` count.
///
/// Two conditions live here and they are not the same mistake. A
/// negative integer is a well typed value the clause cannot use, which
/// is `22G02 negative limit value`; the standard has one code for it and
/// uses it for the offset as well as the limit, so both spellings raise
/// it. Anything that is not an integer at all never gets that far and is
/// `22G03 invalid value type`.
fn count_expr(ctx: &mut StageCtx, expr: &BoundExpr, what: &str) -> Result<usize> {
    ctx.overlay.clear();
    match eval(ctx, expr)? {
        Value::Int(n) if n >= 0 => Ok(n as usize),
        Value::Int(n) => Err(gql(
            codes::C22G02,
            format!("{what} needs a non-negative integer, got {n}"),
        )),
        other => Err(gql(
            codes::C22G03,
            format!("{what} needs an integer, got {other:?}"),
        )),
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

/// Drives one stage to completion. Conditions the stage raised without
/// stopping are left in `ctx.notices` for the caller to drain.
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

// ---------------------------------------------------------------------------
// Morsel scheduler
// ---------------------------------------------------------------------------

/// Splits a stage's driving scan into morsels when the stage is driven
/// by a whole-table scan whose domain spans more than one morsel.
/// Seeded stages driven by an index lookup and later stages fed by the
/// previous stage's rows stay sequential: their driver is one probe or
/// one buffered row set, not a table sweep.
fn plan_morsels(
    stage: &StageDef,
    counts: &BTreeMap<u32, u64>,
    morsel_rows: usize,
) -> Option<Vec<Morsel>> {
    let [OpDesc::Source, OpDesc::Scan { tables, .. }, ..] = &stage.descs[..] else {
        return None;
    };
    let step = if morsel_rows == 0 {
        VECTOR_SIZE as u64
    } else {
        morsel_rows as u64
    };
    let mut morsels = Vec::new();
    for &table in tables {
        let count = *counts.get(&table).unwrap_or(&0);
        let mut start = 0;
        while start < count {
            let end = (start + step).min(count);
            morsels.push(Morsel { table, start, end });
            start = end;
        }
    }
    (morsels.len() > 1).then_some(morsels)
}

/// What one morsel produced: group partials under an aggregating sink,
/// materialized rows otherwise. Partials merge on the main thread in
/// morsel order, which is scan order, so the merged result is exactly
/// what the sequential run produces.
enum MorselOut {
    Groups(BTreeMap<Vec<OrdValue>, Vec<AggState>>),
    Rows(Vec<Row>),
}

/// Everything a worker shares read-only while driving a stage.
struct StageJob<'a> {
    stage: &'a StageDef,
    query: &'a BoundQuery,
    counts: &'a BTreeMap<u32, u64>,
    params: &'a [Value],
}

/// The crossbeam work-finding loop: pop the local deque, refill it
/// from the global injector, steal from a sibling, and retry while any
/// steal reports contention.
fn find_task<T>(
    local: &crossbeam_deque::Worker<T>,
    injector: &crossbeam_deque::Injector<T>,
    stealers: &[crossbeam_deque::Stealer<T>],
) -> Option<T> {
    local.pop().or_else(|| {
        std::iter::repeat_with(|| {
            injector
                .steal_batch_and_pop(local)
                .or_else(|| stealers.iter().map(|s| s.steal()).collect())
        })
        .find(|s| !s.is_retry())
        .and_then(|s| s.success())
    })
}

/// One worker's life: claim morsels until the pool runs dry, driving
/// the whole operator pipeline over each with this worker's own graph
/// reader. Chunks and states reset per morsel; the ASP edge sets
/// survive across morsels, so each worker accumulates once per query.
fn drive_worker(
    job: &StageJob,
    graph: &mut dyn Graph,
    local: &crossbeam_deque::Worker<(usize, Morsel)>,
    injector: &crossbeam_deque::Injector<(usize, Morsel)>,
    stealers: &[crossbeam_deque::Stealer<(usize, Morsel)>],
) -> Result<Vec<(usize, MorselOut)>> {
    let stage = job.stage;
    let sink = &stage.sink;
    let top = stage.descs.len() - 1;
    let mut ctx = StageCtx {
        graph,
        params: job.params,
        counts: job.counts,
        slot_loc: &stage.slot_loc,
        path_shapes: &job.query.path_shapes,
        chunks: Vec::new(),
        states: Vec::new(),
        rows: Vec::new(),
        overlay: BTreeMap::new(),
        scratch: Vec::new(),
        edge_sets: BTreeMap::new(),
        isect: BTreeMap::new(),
        morsel: None,
        stats: Vec::new(),
        live: Vec::new(),
        notices: Vec::new(),
    };
    let mut out = Vec::new();
    while let Some((ix, morsel)) = find_task(local, injector, stealers) {
        ctx.chunks = stage
            .chunk_slots
            .iter()
            .map(|slots| Chunk {
                cols: vec![Vec::new(); slots.len()],
                ..Chunk::default()
            })
            .collect();
        ctx.states = vec![OpState::default(); stage.descs.len()];
        ctx.overlay.clear();
        ctx.morsel = Some(morsel);
        if sink.aggregate {
            let mut groups = BTreeMap::new();
            while next(&stage.descs, &mut ctx, top)? {
                update_groups(sink, &stage.unflat, &mut ctx, &mut groups)?;
            }
            out.push((ix, MorselOut::Groups(groups)));
        } else {
            let mut rows = Vec::new();
            while next(&stage.descs, &mut ctx, top)? {
                each_config(&mut ctx, &stage.unflat, &mut |ctx| {
                    rows.push(materialize(sink, job.query, ctx)?);
                    Ok(())
                })?;
            }
            out.push((ix, MorselOut::Rows(rows)));
        }
    }
    Ok(out)
}

/// Runs one scan-driven stage across the worker pool: morsels go into
/// a global injector, every worker owns a crossbeam deque and steals
/// when its own runs dry, the caller's thread drives the caller's
/// graph as worker zero, and each fork carries one spawned worker.
/// Partials merge in morsel order and the sink's finalize and post
/// operators run once on the merged result.
/// The morsel-parallel counterpart of [`run_stage`]. Workers only
/// accumulate partials, so every condition is raised on the main thread
/// while merging and finalizing, and comes back in `notices`.
fn run_stage_parallel(
    job: &StageJob,
    graph: &mut dyn Graph,
    forks: &mut [Box<dyn Graph + Send>],
    morsels: Vec<Morsel>,
    notices: &mut Vec<DiagnosticRecord>,
) -> Result<Vec<Vec<Value>>> {
    let total = morsels.len();
    let injector = crossbeam_deque::Injector::new();
    for task in morsels.into_iter().enumerate() {
        injector.push(task);
    }
    let workers = (forks.len() + 1).min(total);
    let mut locals: Vec<crossbeam_deque::Worker<(usize, Morsel)>> = (0..workers)
        .map(|_| crossbeam_deque::Worker::new_lifo())
        .collect();
    let stealers: Vec<_> = locals.iter().map(|w| w.stealer()).collect();
    let main_local = locals.remove(0);
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = locals
            .into_iter()
            .zip(forks.iter_mut())
            .map(|(local, fork)| {
                let injector = &injector;
                let stealers = &stealers[..];
                scope.spawn(move || drive_worker(job, fork.as_mut(), &local, injector, stealers))
            })
            .collect();
        let mut all = vec![drive_worker(job, graph, &main_local, &injector, &stealers)];
        for handle in handles {
            all.push(
                handle
                    .join()
                    .unwrap_or_else(|_| Err(invalid("a morsel worker panicked".into()))),
            );
        }
        all
    });
    let mut outs: Vec<Option<MorselOut>> = std::iter::repeat_with(|| None).take(total).collect();
    for result in results {
        for (ix, out) in result? {
            outs[ix] = Some(out);
        }
    }
    let stage = job.stage;
    let sink = &stage.sink;
    let mut ctx = StageCtx {
        graph,
        params: job.params,
        counts: job.counts,
        slot_loc: &stage.slot_loc,
        path_shapes: &job.query.path_shapes,
        chunks: Vec::new(),
        states: Vec::new(),
        rows: Vec::new(),
        overlay: BTreeMap::new(),
        scratch: Vec::new(),
        edge_sets: BTreeMap::new(),
        isect: BTreeMap::new(),
        morsel: None,
        stats: Vec::new(),
        live: Vec::new(),
        notices: Vec::new(),
    };
    let mut rows = Vec::new();
    if sink.aggregate {
        let mut groups: BTreeMap<Vec<OrdValue>, Vec<AggState>> = BTreeMap::new();
        for out in outs.into_iter().flatten() {
            let MorselOut::Groups(part) = out else {
                unreachable!("aggregating sinks produce group partials");
            };
            for (key, states) in part {
                if let Some(mine) = groups.get_mut(&key) {
                    for (state, theirs) in mine.iter_mut().zip(states) {
                        state.merge(theirs)?;
                    }
                } else {
                    groups.insert(key, states);
                }
            }
        }
        if groups.is_empty() && sink.items.iter().all(|it| it.aggregate) {
            groups.insert(Vec::new(), sink.aggs.iter().map(AggState::new).collect());
        }
        for (keyvals, states) in groups {
            rows.push(finalize_group(sink, job.query, &mut ctx, keyvals, states)?);
        }
    } else {
        for out in outs.into_iter().flatten() {
            let MorselOut::Rows(part) = out else {
                unreachable!("row sinks produce row partials");
            };
            rows.extend(part);
        }
    }
    let rows = apply_post(sink, &mut ctx, rows)?;
    notices.append(&mut ctx.notices);
    Ok(rows.into_iter().map(|r| r.values).collect())
}

fn stage_profile(
    stage: &StageDef,
    query: &BoundQuery,
    schema: &Schema,
    stats: &[OpStats],
    out_rows: u64,
    nanos: u64,
) -> StageProfile {
    let mut ops = Vec::with_capacity(stats.len());
    for (i, s) in stats.iter().enumerate() {
        let child = if i == 0 { 0 } else { stats[i - 1].nanos };
        let expected = stage
            .est
            .get(i)
            .copied()
            .flatten()
            .filter(|_| counts_rows(&stage.descs[i]));
        ops.push(OpProfile {
            name: op_name(&stage.descs[i], stage, query, schema),
            pulls: s.pulls,
            rows: s.rows,
            flat: s.flat,
            est: expected.map(|e| e.est),
            bnd: expected.and_then(|e| e.bnd),
            nanos: s.nanos.saturating_sub(child),
        });
    }
    StageProfile {
        ops,
        sink: sink_name(&stage.sink),
        out_rows,
        nanos,
    }
}

fn run_stages(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
    mut profile: Option<&mut Profile>,
) -> Result<QueryResult> {
    let stages = build_stages(plan, query, schema, graph, params, options)?;
    let counts: BTreeMap<u32, u64> = schema
        .nodes()
        .iter()
        .map(|n| (n.id, n.node_count))
        .collect();
    let auto = detected_parallelism().min(8);
    let threads = if options.threads == 0 {
        auto
    } else {
        options.threads
    };
    // Worker readers, forked once on the first parallel stage and
    // reused for the rest of the query. Profiled runs stay sequential
    // so per-operator counters keep their one-linear-chain meaning,
    // and flat runs stay sequential because the differential oracle is
    // the fully sequential baseline.
    let mut forks: Option<Vec<Box<dyn Graph + Send>>> = None;
    let mut rows = Vec::new();
    let mut notices = Vec::new();
    for stage in &stages {
        if threads > 1
            && profile.is_none()
            && !options.flat
            && let Some(morsels) = plan_morsels(stage, &counts, options.morsel_rows)
        {
            let forks = forks.get_or_insert_with(|| {
                (1..threads.min(morsels.len()))
                    .map_while(|_| graph.fork())
                    .collect()
            });
            if !forks.is_empty() {
                let job = StageJob {
                    stage,
                    query,
                    counts: &counts,
                    params,
                };
                rows = run_stage_parallel(&job, graph, forks, morsels, &mut notices)?;
                continue;
            }
        }
        let mut ctx = StageCtx {
            graph: &mut *graph,
            params,
            counts: &counts,
            slot_loc: &stage.slot_loc,
            path_shapes: &query.path_shapes,
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
            edge_sets: BTreeMap::new(),
            isect: BTreeMap::new(),
            morsel: None,
            stats: if profile.is_some() {
                vec![OpStats::default(); stage.descs.len()]
            } else {
                Vec::new()
            },
            live: if profile.is_some() {
                live_unflat(&stage.descs)
            } else {
                Vec::new()
            },
            notices: Vec::new(),
        };
        let started = Instant::now();
        rows = run_stage(stage, query, &mut ctx)?;
        notices.append(&mut ctx.notices);
        if let Some(p) = profile.as_deref_mut() {
            p.stages.push(stage_profile(
                stage,
                query,
                schema,
                &ctx.stats,
                rows.len() as u64,
                started.elapsed().as_nanos() as u64,
            ));
        }
    }
    let mut result = QueryResult::new(query.columns.clone(), rows);
    for record in notices {
        result.notice(record);
    }
    Ok(result)
}

/// available_parallelism, resolved once for the process. On Linux the
/// std call walks the cgroup hierarchy every time (five file opens
/// plus statx each), which on a vCPU host costs more than a whole
/// warm point read; the answer does not change under us, so pay once.
pub(crate) fn detected_parallelism() -> usize {
    static DETECTED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DETECTED.get_or_init(|| std::thread::available_parallelism().map_or(1, |p| p.get()))
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
    run_stages(plan, query, schema, graph, params, options, None)
}

/// Runs an optimized plan with per-operator counters and returns the
/// result rows next to the EXPLAIN ANALYZE profile. Timing wraps every
/// pull, so profiled runs pay two clock reads per pull; use `execute`
/// on the hot path.
pub fn execute_profiled(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
) -> Result<(QueryResult, Profile)> {
    let mut profile = Profile { stages: Vec::new() };
    let result = run_stages(
        plan,
        query,
        schema,
        graph,
        params,
        options,
        Some(&mut profile),
    )?;
    Ok((result, profile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{NodeDef, RelDef};

    #[test]
    fn a_ceiling_is_violated_by_a_factor_and_never_by_a_rounding() {
        let op = |flat, bnd| OpProfile {
            name: "Expand".into(),
            pulls: 1,
            rows: flat,
            flat,
            est: Some(1.0),
            bnd,
            nanos: 0,
        };
        // The Holder terms are roots and fractional powers, so a bound
        // that is exactly the edge count in real arithmetic comes back
        // a couple of ULPs under it.
        assert!(!op(20_000, Some(19_999.999_999_999_993)).bound_violation());
        assert!(!op(20_000, Some(20_000.0)).bound_violation());
        assert!(op(20_001, Some(20_000.0)).bound_violation());
        // No statistics, no promise, so nothing to violate.
        assert!(!op(u64::MAX, None).bound_violation());
    }

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
                // A property the binder cannot type as numeric, so an
                // operator that wants a number has to find out when the
                // row arrives rather than when the plan is built. That
                // is the only way to exercise the run-time half of the
                // type checks from here.
                "name" => Ok(Value::Str(format!("p{offset}"))),
                other => Err(invalid(format!("unknown property '{other}'"))),
            }
        }

        fn fork(&self) -> Option<Box<dyn Graph + Send>> {
            Some(Box::new(MockGraph {
                edges: self.edges.clone(),
            }))
        }

        /// Stub kernels with recognizable per-offset values: the real
        /// ones live in the engines, these pin the CALL plumbing.
        fn table_function(
            &mut self,
            name: &str,
            rel: u32,
            args: &[Value],
        ) -> Result<Vec<Vec<Value>>> {
            let n = if rel == 2 { 6 } else { 3 };
            match name {
                "pagerank" => Ok((0..n)
                    .map(|o| vec![Value::Float(o as f64 / 10.0)])
                    .collect()),
                "wcc" | "louvain" => Ok((0..n).map(|o| vec![Value::Int(o % 2)]).collect()),
                "sssp" => {
                    let Some(Value::Int(source)) = args.first() else {
                        return Err(invalid("mock sssp needs a source".into()));
                    };
                    Ok((0..n)
                        .map(|o| {
                            vec![if o == *source {
                                Value::Int(0)
                            } else {
                                Value::Null
                            }]
                        })
                        .collect())
                }
                other => Err(invalid(format!("mock has no kernel '{other}'"))),
            }
        }
    }

    fn run_opts(source: &str, params: &[(&str, Value)], options: Options) -> QueryResult {
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
        execute(&optimized, &query, &schema, &mut graph, &args, &options).expect("execute")
    }

    fn run_with(source: &str, params: &[(&str, Value)], flat: bool) -> QueryResult {
        run_opts(
            source,
            params,
            Options {
                flat,
                threads: 1,
                ..Options::default()
            },
        )
    }

    fn run(source: &str, params: &[(&str, Value)]) -> QueryResult {
        run_with(source, params, false)
    }

    /// The GQLSTATUS a statement that cannot run comes back with. Panics
    /// when it succeeds, or when it fails without a code, since an
    /// uncoded failure on a well formed statement is the bug this is
    /// here to catch.
    fn status_of(source: &str) -> &'static str {
        let schema = schema();
        let parsed = match crate::parser::parse(source) {
            Ok(p) => p,
            Err(e) => return coded(&e),
        };
        let query = match crate::binder::bind(&parsed, &schema) {
            Ok(q) => q,
            Err(e) => return coded(&e),
        };
        let built = crate::plan::build(&query).expect("plan");
        let optimized = crate::optimizer::optimize(built, &query, &schema).expect("optimize");
        let mut graph = mock();
        match execute(
            &optimized,
            &query,
            &schema,
            &mut graph,
            &[],
            &Options {
                threads: 1,
                ..Options::default()
            },
        ) {
            Ok(_) => panic!("{source:?} was expected to fail"),
            Err(e) => coded(&e),
        }
    }

    fn coded(e: &ZuError) -> &'static str {
        e.gqlstatus()
            .unwrap_or_else(|| panic!("failure with no GQLSTATUS: {e}"))
            .code()
    }

    /// Runs morsel-parallel with 2-row morsels over four workers, so
    /// the 6-person mock splits into three morsels and the scheduler
    /// path actually executes.
    fn run_par(source: &str, params: &[(&str, Value)]) -> QueryResult {
        run_opts(
            source,
            params,
            Options {
                threads: 4,
                morsel_rows: 2,
                ..Options::default()
            },
        )
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
    fn asp_join_retains_the_neighbor_vector() {
        // Unseeded triangle count with the WCOJ fusion pinned off, the
        // binary fallback path: the closing expand upgrades to the ASP
        // hash join, and with nothing reading c or the closing rel the
        // probe retains c's neighbor vector in place, no flatten.
        let (r, p) = profiled_opts(
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
             RETURN count(*) AS triangles",
            &[],
            no_wcoj(),
        );
        assert_eq!(int_rows(&r), [[1]]);
        let names: Vec<&str> = p.stages[0].ops.iter().map(|o| o.name.as_str()).collect();
        assert!(
            names.iter().any(|n| n.starts_with("AspJoin (retain)")),
            "got: {names:?}"
        );
    }

    #[test]
    fn asp_join_probes_flat_when_probe_rows_share_a_vector() {
        // The order the optimizer picks under COLOR summaries on a
        // real hub graph: scan c, expand both a and b backward from c,
        // close a to b. The two lists are a cross product over c, so
        // after flattening a, several probe rows share one b vector
        // per c. Retaining survivors in place would let the first a's
        // filter narrow every later a's probe: on this graph a=1 keeps
        // only b=3 and a=2 then misses b=4, halving the count. The
        // fusion must stay off and the join probe one configuration at
        // a time.
        let schema = Schema::new(
            vec![NodeDef {
                id: 0,
                name: "Person".into(),
                node_count: 5,
            }],
            vec![RelDef {
                id: 2,
                name: "KNOWS".into(),
                from: 0,
                to: 0,
                edge_count: 6,
            }],
        )
        .expect("schema");
        let parsed = crate::parser::parse(
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
             RETURN count(*) AS triangles",
        )
        .expect("parse");
        let query = crate::binder::bind(&parsed, &schema).expect("bind");
        // Harvest slots and the aggregate wrapper from the built plan,
        // then rewire the match into the shared-vector order by hand
        // so the shape cannot drift with the optimizer.
        let built = crate::plan::build(&query).expect("plan");
        let LogicalPlan::Aggregate { input, keys, aggs } = built else {
            panic!("count(*) builds an aggregate");
        };
        let LogicalPlan::Expand {
            rel: close,
            from: a,
            to: c,
            input,
            ..
        } = *input
        else {
            panic!("the built plan closes a to c");
        };
        let LogicalPlan::Expand {
            rel: hop2,
            from: b,
            input,
            ..
        } = *input
        else {
            panic!("the built plan hops b to c");
        };
        let LogicalPlan::Expand { rel: hop1, .. } = *input else {
            panic!("the built plan hops a to b");
        };
        let scan = LogicalPlan::ScanNodes {
            input: Box::new(LogicalPlan::Empty),
            slot: c,
            optional: None,
        };
        let expand = |input, rel, from, to, direction, into, asp| LogicalPlan::Expand {
            input: Box::new(input),
            rel,
            from,
            to,
            direction,
            range: None,
            into,
            asp,
            wcoj: false,
            optional: None,
        };
        let b_from_c = expand(scan, hop2, c, b, RelDirection::In, false, false);
        let a_from_c = expand(b_from_c, close, c, a, RelDirection::In, false, false);
        let close_a_b = expand(a_from_c, hop1, a, b, RelDirection::Out, true, true);
        let plan = LogicalPlan::Aggregate {
            input: Box::new(close_a_b),
            keys,
            aggs,
        };
        // Everyone points at 0, plus 1 knows 3 and 2 knows 4: the
        // triangles are (1, 3, 0) and (2, 4, 0).
        let mut graph = MockGraph {
            edges: BTreeMap::from([(2u32, vec![(1, 0), (2, 0), (3, 0), (4, 0), (1, 3), (2, 4)])]),
        };
        let options = Options {
            threads: 1,
            ..Options::default()
        };
        let (r, p) =
            execute_profiled(&plan, &query, &schema, &mut graph, &[], &options).expect("execute");
        assert_eq!(int_rows(&r), [[2]]);
        let names: Vec<&str> = p.stages[0].ops.iter().map(|o| o.name.as_str()).collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("AspJoin (") && !n.contains("retain")),
            "got: {names:?}"
        );
    }

    #[test]
    fn asp_join_probes_flat_when_the_far_node_is_read() {
        // Same close, but the projection reads c, so the retain fusion
        // stays off and the join probes one configuration at a time.
        let (r, p) = profiled_opts(
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
             RETURN a.id AS a, b.id AS b, c.id AS c",
            &[],
            no_wcoj(),
        );
        assert_eq!(int_rows(&r), [[0, 1, 2]]);
        let names: Vec<&str> = p.stages[0].ops.iter().map(|o| o.name.as_str()).collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("AspJoin (") && !n.contains("retain")),
            "got: {names:?}"
        );
    }

    fn wcoj() -> Options {
        Options {
            wcoj: Wcoj::Force,
            ..Options::default()
        }
    }

    fn no_wcoj() -> Options {
        Options {
            wcoj: Wcoj::Off,
            ..Options::default()
        }
    }

    fn op_names(p: &Profile) -> Vec<&str> {
        p.stages[0].ops.iter().map(|o| o.name.as_str()).collect()
    }

    #[test]
    fn gallop_finds_the_first_index_at_or_after_the_target() {
        let list = [2, 4, 4, 7, 11, 15, 15, 20];
        for from in 0..list.len() {
            for target in 0..25 {
                let want = (from..list.len())
                    .find(|&ix| list[ix] >= target)
                    .unwrap_or(list.len());
                assert_eq!(
                    gallop(&list, target, from),
                    want,
                    "target {target} from {from}"
                );
            }
        }
    }

    #[test]
    fn leapfrog_emits_one_hit_per_seed_occurrence() {
        // A duplicate in the seed list is a real multi-edge row and
        // emits twice; a duplicate in the probe list is only an
        // existence check and adds nothing.
        let mut hits = Vec::new();
        leapfrog(&[2, 2, 3, 7, 9, 12], &[1, 2, 7, 7, 8, 12], |v| hits.push(v));
        assert_eq!(hits, [2, 2, 7, 12]);
        hits.clear();
        leapfrog(&[5], &[], |v| hits.push(v));
        leapfrog(&[], &[5], |v| hits.push(v));
        assert_eq!(hits, []);
    }

    #[test]
    fn multiway_intersect_closes_the_triangle() {
        // The expand into c and the probe of the closing edge fuse
        // into one galloping intersection; results must match the
        // binary-join baseline exactly.
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                      RETURN a.id AS a, b.id AS b, c.id AS c";
        let (r, p) = profiled_opts(source, &[], wcoj());
        assert_eq!(int_rows(&r), [[0, 1, 2]]);
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
        let names = op_names(&p);
        assert!(
            names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "got: {names:?}"
        );
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with("AspJoin") || n.starts_with("ExpandInto")),
            "the fused pair must be gone, got: {names:?}"
        );
    }

    #[test]
    fn multiway_intersect_counts_triangles() {
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                      RETURN count(*) AS triangles";
        let (r, p) = profiled_opts(source, &[], wcoj());
        assert_eq!(int_rows(&r), [[1]]);
        let names = op_names(&p);
        assert!(
            names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "got: {names:?}"
        );
    }

    #[test]
    fn mirrored_closes_fuse_when_the_dp_walks_backwards() {
        // An in-fanout below the out-fanout flips the DP to the
        // reversed walk, which orients the close out of the last
        // introduced node instead of into it. The mirrored fusion
        // probes the bound end's in-lists and the rows must still
        // match the pinned binary join.
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                      RETURN a.id AS a, b.id AS b, c.id AS c";
        let mut schema = schema();
        schema.set_degree_hists([(2u32, [vec![0, 2], vec![4]])].into_iter().collect());
        let parsed = crate::parser::parse(source).expect("parse");
        let query = crate::binder::bind(&parsed, &schema).expect("bind");
        let built = crate::plan::build(&query).expect("plan");
        let optimized = crate::optimizer::optimize(built, &query, &schema).expect("optimize");
        let mut graph = mock();
        let (r, p) = execute_profiled(
            &optimized,
            &query,
            &schema,
            &mut graph,
            &[],
            &Options::default(),
        )
        .expect("execute profiled");
        assert_eq!(int_rows(&r), [[0, 1, 2]]);
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
        let names = op_names(&p);
        assert!(
            names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "the mirrored close must fuse, got: {names:?}"
        );
    }

    #[test]
    fn multiway_intersect_takes_an_in_direction_seed() {
        // Co-citation wedge closed by an out edge: the seed side reads
        // b's in-neighbors, the probe side a's out-neighbors. The only
        // hit is a=0, b=2, c=1: 0 and 1 both point at 2, and 0 knows 1.
        let source = "MATCH (a:Person)-[:KNOWS]->(b)<-[:KNOWS]-(c), (a)-[:KNOWS]->(c) \
                      RETURN a.id AS a, b.id AS b, c.id AS c";
        let (r, p) = profiled_opts(source, &[], wcoj());
        assert_eq!(int_rows(&r), [[0, 2, 1]]);
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
        let names = op_names(&p);
        assert!(
            names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "got: {names:?}"
        );
    }

    #[test]
    fn multiway_intersect_materializes_rels_when_read() {
        // Named rels force both rel columns to materialize. The exact
        // values pin the orientation: the seed rel runs b to c, the
        // probe rel a to c, never the reverse.
        let source = "MATCH (a:Person)-[r:KNOWS]->(b)-[s:KNOWS]->(c), (a)-[t:KNOWS]->(c) \
                      RETURN r AS r, s AS s, t AS t";
        let r = run_opts(source, &[], wcoj());
        assert_eq!(
            r.rows,
            [[knows(0, 1), knows(1, 2), knows(0, 2)]],
            "rel orientation diverged"
        );
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
    }

    #[test]
    fn multiway_intersect_orients_an_in_direction_rel() {
        // The in-direction seed of the co-citation wedge: the stored
        // edge runs c to b, so the emitted rel must too.
        let source = "MATCH (a:Person)-[r:KNOWS]->(b)<-[:KNOWS]-(c), (a)-[t:KNOWS]->(c) \
                      RETURN r AS r, t AS t";
        let r = run_opts(source, &[], wcoj());
        assert_eq!(r.rows, [[knows(0, 2), knows(0, 1)]]);
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
        let with_s = "MATCH (a:Person)-[:KNOWS]->(b)<-[s:KNOWS]-(c), (a)-[:KNOWS]->(c) \
                      RETURN s AS s";
        let r = run_opts(with_s, &[], wcoj());
        assert_eq!(r.rows, [[knows(1, 2)]]);
    }

    #[test]
    fn multiway_intersect_takes_the_undirected_close() {
        // KNOWS runs Person to Person, so an undirected end is the two
        // stored lists of the one table: the seed side walks them in
        // turn and the probe side answers out of their union. The
        // fusion takes it and the count matches the binary pair.
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]-(c) \
                      RETURN count(*) AS triangles";
        let (r, p) = profiled_opts(source, &[], wcoj());
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
        let names = op_names(&p);
        assert!(
            names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "got: {names:?}"
        );
    }

    #[test]
    fn the_undirected_intersect_orients_the_rels_it_emits() {
        // Both ends undirected and every rel returned, so the fusion
        // has to say which way each edge it kept was stored. The probe
        // side is the one that can go either way and the emitted rels
        // have to come out the same as the pair the fusion replaced.
        let source = "MATCH (a:Person)-[r1:KNOWS]-(b)-[r2:KNOWS]-(c), (a)-[r3:KNOWS]-(c) \
                      RETURN r1, r2, r3";
        let (r, p) = profiled_opts(source, &[], wcoj());
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
        let names = op_names(&p);
        assert!(
            names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "got: {names:?}"
        );
    }

    #[test]
    fn multiway_intersect_inside_an_optional_group() {
        // The fusion fires inside the optional group and a miss still
        // nulls the whole fused chunk, c and both rels.
        let source = "MATCH (a:Person)-[:KNOWS]->(b) \
                      OPTIONAL MATCH (b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                      RETURN count(a) AS pairs, count(c) AS closed";
        let r = run_opts(source, &[], wcoj());
        assert_eq!(int_rows(&r), [[8, 1]]);
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
    }

    #[test]
    fn multiway_intersect_declines_an_optional_close() {
        // The closing probe lives in its own optional group here, so
        // fusing it into the required expand would turn the left outer
        // join inner and drop the nine open 2-paths.
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) \
                      OPTIONAL MATCH (a)-[t:KNOWS]->(c) \
                      RETURN count(*) AS paths, count(t) AS closed";
        let (r, p) = profiled_opts(source, &[], wcoj());
        assert_eq!(int_rows(&r), [[10, 1]]);
        assert_eq!(r, run_opts(source, &[], no_wcoj()));
        let names = op_names(&p);
        assert!(
            !names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "got: {names:?}"
        );
    }

    #[test]
    fn multiway_intersect_runs_morsel_parallel() {
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                      RETURN a.id AS a, b.id AS b, c.id AS c";
        let r = run_opts(
            source,
            &[],
            Options {
                threads: 4,
                morsel_rows: 2,
                wcoj: Wcoj::Force,
                ..Options::default()
            },
        );
        assert_eq!(int_rows(&r), [[0, 1, 2]]);
    }

    #[test]
    fn the_optimizer_marks_the_triangle_close_on_its_own() {
        // No switch anywhere: default options, and the intersection
        // still runs because the optimizer marked the cyclic close.
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                      RETURN a.id AS a, b.id AS b, c.id AS c";
        let (r, p) = profiled(source, &[]);
        assert_eq!(int_rows(&r), [[0, 1, 2]]);
        let names = op_names(&p);
        assert!(
            names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "got: {names:?}"
        );
    }

    #[test]
    fn wcoj_off_pins_the_binary_join() {
        // The off mode is the differential baseline: the optimizer's
        // mark is ignored and the close runs the binary pair.
        let source = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                      RETURN a.id AS a, b.id AS b, c.id AS c";
        let (r, p) = profiled_opts(source, &[], no_wcoj());
        assert_eq!(int_rows(&r), [[0, 1, 2]]);
        let names = op_names(&p);
        assert!(
            !names.iter().any(|n| n.starts_with("MultiwayIntersect")),
            "got: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("AspJoin") || n.starts_with("ExpandInto")),
            "got: {names:?}"
        );
    }

    #[test]
    fn asp_join_closes_undirected_patterns() {
        // The one undirected triangle {0, 1, 2} counts once per
        // ordering of its three corners.
        let r = run(
            "MATCH (a:Person)-[:KNOWS]-(b)-[:KNOWS]-(c), (a)-[:KNOWS]-(c) \
             RETURN count(*) AS walks",
            &[],
        );
        assert_eq!(int_rows(&r), [[6]]);
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
    fn call_yields_a_row_per_node_and_composes_with_sort() {
        let r = run(
            "CALL pagerank('KNOWS') YIELD node, rank \
             RETURN node.id AS id, rank ORDER BY rank DESC LIMIT 2",
            &[],
        );
        assert_eq!(r.columns, ["id", "rank"]);
        assert_eq!(
            r.rows,
            [
                [Value::Int(5), Value::Float(0.5)],
                [Value::Int(4), Value::Float(0.4)],
            ]
        );
    }

    #[test]
    fn call_output_expands_into_a_following_match() {
        // Every KNOWS edge appears once when the match starts from the
        // call's synthesized nodes, so the plumbing produced real node
        // values the expand can walk.
        let r = run(
            "CALL wcc('KNOWS') YIELD node, component \
             MATCH (node)-[:KNOWS]->(m) \
             RETURN count(*) AS paths",
            &[],
        );
        assert_eq!(int_rows(&r), [[8]]);
    }

    #[test]
    fn call_passes_the_sssp_source_through() {
        let r = run(
            "CALL sssp('KNOWS', 3) YIELD node, distance \
             WITH node, distance WHERE distance IS NOT NULL \
             RETURN node.id AS id, distance",
            &[],
        );
        assert_eq!(int_rows(&r), [[3, 0]]);
    }

    #[test]
    fn call_rejects_bad_shapes_at_bind_time() {
        let schema = schema();
        for (source, want) in [
            (
                "CALL pagerank('KNOWS') YIELD node, score RETURN score",
                "yields the columns node, rank",
            ),
            (
                "CALL nonsense('KNOWS') YIELD node, rank RETURN rank",
                "unknown table function",
            ),
            (
                "CALL pagerank('IS_LOCATED_IN') YIELD node, rank RETURN rank",
                "over one node table",
            ),
            (
                "CALL pagerank('KNOWS', 1) YIELD node, rank RETURN rank",
                "takes only the rel table",
            ),
            (
                "CALL sssp('KNOWS') YIELD node, distance RETURN distance",
                "source node id",
            ),
            (
                "MATCH (a:Person) CALL wcc('KNOWS') YIELD node, component RETURN component",
                "first clause",
            ),
        ] {
            let parsed = crate::parser::parse(source).expect("parse");
            let err = crate::binder::bind(&parsed, &schema).expect_err(source);
            assert!(err.to_string().contains(want), "{source}: {err}");
        }
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
    fn dividing_by_zero_is_22012_whatever_the_numeric_type() {
        assert_eq!(status_of("MATCH (a:Person) RETURN 1 / 0 AS x"), "22012");
        assert_eq!(status_of("MATCH (a:Person) RETURN 1 % 0 AS x"), "22012");
        // IEEE would answer infinity for the approximate case. The
        // standard asks for the condition, so we raise it.
        assert_eq!(status_of("MATCH (a:Person) RETURN 1.0 / 0.0 AS x"), "22012");
        assert_eq!(status_of("MATCH (a:Person) RETURN 1 / 0.0 AS x"), "22012");
    }

    #[test]
    fn a_negative_count_is_22g02_and_a_non_integer_one_is_22g03() {
        // The standard names 22G02 for the limit and uses the same code
        // for the offset, so both spellings answer with it.
        assert_eq!(
            status_of("MATCH (a:Person) RETURN a.id AS id LIMIT 0 - 1"),
            "22G02"
        );
        assert_eq!(
            status_of("MATCH (a:Person) RETURN a.id AS id SKIP 0 - 1"),
            "22G02"
        );
        // Not an integer at all, so it never gets as far as being
        // negative. A different mistake and a different code.
        assert_eq!(
            status_of("MATCH (a:Person) RETURN a.id AS id LIMIT 'two'"),
            "22G03"
        );
    }

    #[test]
    fn a_type_the_operator_does_not_take_is_22g03_wherever_it_is_caught() {
        // Every one of these is the same mistake, an operand whose type
        // the position does not accept, and every one has to answer with
        // the same code. Some are decided from the catalog before the
        // plan is built and some only once a value arrives, and which
        // side catches it is not something a statement's author can see.
        for source in [
            // Caught statically: both operand types are known.
            "MATCH (a:Person) RETURN 1 + 'a' AS v",
            "MATCH (a:Person) RETURN 'x' - 1 AS v",
            "MATCH (a:Person) RETURN -'x' AS v",
            "MATCH (a:Person) RETURN NOT 1 AS v",
            "MATCH (a:Person) RETURN 1 AND true AS v",
            "MATCH (a:Person) RETURN 1 STARTS WITH 'a' AS v",
            "MATCH (a:Person) RETURN 1 IN 2 AS v",
            "MATCH (a:Person) RETURN size(1) AS v",
            "MATCH (a:Person) RETURN id(1) AS v",
            "MATCH (a:Person) RETURN sum(a.name) AS v",
            "MATCH (a:Person) RETURN avg(a.name) AS v",
            // Caught at run time: the column answers a string and the
            // operator only finds out when the row arrives.
            "MATCH (a:Person) RETURN 1 + a.name AS v",
        ] {
            assert_eq!(status_of(source), "22G03", "for {source}");
        }
    }

    #[test]
    fn a_name_that_does_not_resolve_is_42002_not_42001() {
        // The statement parses. Nothing is wrong with its syntax; it
        // just mentions something that is not there, which is the
        // distinction between 42001 and 42002.
        assert_eq!(status_of("MATCH (a:Person) RETURN nope AS x"), "42002");
        assert_eq!(status_of("MATCH (a:Nonexistent) RETURN a.id AS x"), "42002");
        assert_eq!(
            status_of("MATCH (a:Person)-[:NOPE]->(b) RETURN a.id AS x"),
            "42002"
        );
        assert_eq!(
            status_of("MATCH (a:Person) RETURN nosuchfunc(a) AS x"),
            "42002"
        );
        assert_eq!(
            status_of("MATCH (a:Person) WITH a.id AS x, a.id AS x RETURN x"),
            "42002"
        );
        // Contrast: this one really is malformed.
        assert_eq!(status_of("MATCH (a:Person) RETURN"), "42001");
        // Not a contrast to make by accident: naming a bound variable
        // again is a join, not a redefinition, and stays legal.
        let r = run("MATCH (a:Person) MATCH (a:Person) RETURN a.id AS x", &[]);
        assert_eq!(r.rows.len(), 6);
    }

    #[test]
    fn an_aggregate_that_skips_a_null_warns_with_01g11() {
        // The optional group misses for most people, so avg() has a
        // null argument on those rows and ignores it. The answer is
        // still an answer, so it comes back with a warning beside it
        // rather than an error instead of it.
        let r = run(
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 \
             RETURN avg(b.id) AS avg_friend",
            &[],
        );
        assert_eq!(r.notices.len(), 1);
        assert_eq!(r.notices[0].status.code(), "01G11");
        assert!(r.notices[0].severity().is_success());
        // A warning is not an exception: the rows survived.
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn the_warning_is_raised_once_however_many_groups_dropped_a_null() {
        let r = run(
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 \
             RETURN a.id AS id, avg(b.id) AS avg_friend ORDER BY id",
            &[],
        );
        assert!(r.rows.len() > 1, "several groups, so several chances");
        assert_eq!(
            r.notices.len(),
            1,
            "one warning per statement, not per group"
        );
        assert_eq!(r.notices[0].status.code(), "01G11");
    }

    #[test]
    fn an_aggregate_with_nothing_to_skip_says_nothing() {
        let r = run("MATCH (a:Person) RETURN avg(a.id) AS avg_id", &[]);
        assert!(r.notices.is_empty());
        // count(*) has no argument, so it has no null to eliminate.
        let star = run("MATCH (a:Person) RETURN count(*) AS n", &[]);
        assert!(star.notices.is_empty());
    }

    #[test]
    fn the_parallel_path_reports_the_same_warning_as_the_sequential_one() {
        let source = "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 \
                      RETURN avg(b.id) AS avg_friend";
        let seq = run(source, &[]);
        let par = run_par(source, &[]);
        // Workers only accumulate partials, so the flag has to survive
        // the merge for these two to agree.
        assert_eq!(seq.notices, par.notices);
        assert_eq!(seq.rows, par.rows);
    }

    #[test]
    fn a_result_with_no_columns_completes_with_00001() {
        // No statement zu parses reaches this yet: the grammar requires
        // a projection, so `columns` is never empty from a real query.
        // The rule lives on the result anyway, so the first write
        // statement in milestone G3 gets it without anyone remembering to.
        let omitted = QueryResult::new(Vec::new(), Vec::new());
        assert_eq!(omitted.status().code(), "00001");
        assert!(omitted.status().severity().is_success());
        assert!(omitted.notices.is_empty(), "the outcome is not a notice");
    }

    #[test]
    fn a_statement_that_projects_completes_with_00000() {
        let ok = run("MATCH (a:Person) RETURN a.id AS id", &[]);
        assert_eq!(ok.status().code(), "00000");
        assert!(ok.notices.is_empty());
        // Zero rows is still a successful completion. `02000 no data` is
        // for a positioned operation that found nothing to act on, not
        // for a query whose binding table came back empty, and reporting
        // it here would be inventing a condition the standard does not
        // raise.
        let empty = run(
            "MATCH (a:Person) WHERE a.id > 100000 RETURN a.id AS id",
            &[],
        );
        assert!(empty.rows.is_empty());
        assert_eq!(empty.status().code(), "00000");
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
    fn var_length_reaches_one_and_two_hops() {
        let r = run(
            "MATCH (a:Person {id: 0})-[:KNOWS*1..2]->(b) RETURN b.id AS b ORDER BY b",
            &[],
        );
        // Depth one reaches 1 and 2; depth two reaches 2 and 3 via 1
        // and 4 via 2. Node 2 arrives once per trail.
        assert_eq!(int_rows(&r), [[1], [2], [2], [3], [4]]);
    }

    #[test]
    fn var_length_lower_bound_skips_short_trails() {
        let r = run(
            "MATCH (a:Person {id: 0})-[:KNOWS*2..2]->(b) RETURN b.id AS b ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[2], [3], [4]]);
    }

    #[test]
    fn var_length_binds_the_rel_list() {
        let r = run(
            "MATCH (a:Person {id: 0})-[r:KNOWS*1..3]->(b) \
             RETURN b.id AS b, size(r) AS hops ORDER BY b, hops",
            &[],
        );
        assert_eq!(
            int_rows(&r),
            [
                [1, 1],
                [2, 1],
                [2, 2],
                [3, 2],
                [4, 2],
                [4, 3],
                [4, 3],
                [5, 3],
            ]
        );
    }

    #[test]
    fn var_length_trails_never_reuse_an_edge() {
        // Length-six trails from node 0 revisit node 0 through the
        // 5 -> 0 edge and continue on the still-unused outgoing edge,
        // so this passes only if uniqueness is per edge, not per node.
        let r = run(
            "MATCH (a:Person {id: 0})-[:KNOWS*6..6]->(b) RETURN b.id AS b ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[2], [2], [2], [3]]);
    }

    #[test]
    fn var_length_undirected_walks_both_ways() {
        let r = run(
            "MATCH (a:Person {id: 3})-[:KNOWS*1..2]-(b) RETURN b.id AS b ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[0], [1], [2], [2], [4], [5]]);
    }

    #[test]
    fn walk_mode_reuses_edges_a_trail_cannot() {
        // The five-hop trails from 0 are 0-1-2-4-5-0, 0-1-3-4-5-0, and
        // 0-2-4-5-0-1. A walk adds 0-2-4-5-0-2, reusing the 0->2 edge.
        let r = run(
            "MATCH (a:Person {id: 0})-[:KNOWS*5..5]->(b) RETURN b.id AS b ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[0], [0], [1]]);
        let r = run(
            "MATCH WALK (a:Person {id: 0})-[:KNOWS*5..5]->(b) RETURN b.id AS b ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[0], [0], [1], [2]]);
    }

    #[test]
    fn acyclic_mode_never_revisits_a_node() {
        // The four-hop trails from 0 include the cycle 0-2-4-5-0;
        // ACYCLIC drops it because the start node repeats.
        let r = run(
            "MATCH (a:Person {id: 0})-[:KNOWS*4..4]->(b) RETURN b.id AS b ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[0], [5], [5]]);
        let r = run(
            "MATCH ACYCLIC (a:Person {id: 0})-[:KNOWS*4..4]->(b) RETURN b.id AS b ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[5], [5]]);
    }

    #[test]
    fn any_shortest_keeps_one_minimum_hop_path_per_endpoint() {
        let r = run(
            "MATCH ANY SHORTEST (a:Person {id: 0})-[r:KNOWS*]->(b) \
             RETURN b.id AS b, size(r) AS hops ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[1, 1], [2, 1], [3, 2], [4, 2], [5, 3]]);
    }

    #[test]
    fn all_shortest_returns_every_minimum_hop_path() {
        // Undirected from 3, node 2 sits two hops away through both 1
        // and 4; every other endpoint has one shortest path.
        let r = run(
            "MATCH ALL SHORTEST (a:Person {id: 3})-[r:KNOWS*]-(b) \
             RETURN b.id AS b, size(r) AS hops ORDER BY b, hops",
            &[],
        );
        assert_eq!(
            int_rows(&r),
            [[0, 2], [1, 1], [2, 2], [2, 2], [4, 1], [5, 2]]
        );
        let r = run(
            "MATCH ANY SHORTEST (a:Person {id: 3})-[r:KNOWS*]-(b) \
             RETURN b.id AS b, size(r) AS hops ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[0, 2], [1, 1], [2, 2], [4, 1], [5, 2]]);
    }

    #[test]
    fn shortest_selectors_agree_with_trail_minimums() {
        // On the directed graph every endpoint's shortest path is
        // unique, so ANY, ALL, and a minimum over plain trails agree.
        let all = run(
            "MATCH ALL SHORTEST (a:Person {id: 0})-[r:KNOWS*]->(b) \
             RETURN b.id AS b, size(r) AS hops ORDER BY b",
            &[],
        );
        let any = run(
            "MATCH ANY SHORTEST (a:Person {id: 0})-[r:KNOWS*]->(b) \
             RETURN b.id AS b, size(r) AS hops ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&all), int_rows(&any));
        // Trails reach the start again through the cycle, but a
        // shortest selector never emits the start: its zero-hop path
        // sits below the lower bound of one.
        let trails = run(
            "MATCH (a:Person {id: 0})-[r:KNOWS*1..8]->(b) WHERE b.id <> a.id \
             RETURN b.id AS b, min(size(r)) AS hops ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&any), int_rows(&trails));
    }

    fn node(offset: u64) -> Value {
        Value::Node { table: 0, offset }
    }

    fn knows(src: u64, dst: u64) -> Value {
        Value::Rel { table: 2, src, dst }
    }

    /// The expected path, built through the constructor so the elements
    /// a test writes down have to be a walk somebody can take. A test
    /// that expected a malformed path would be asserting against a
    /// value the engine cannot produce.
    fn path(elements: Vec<Value>) -> Value {
        Value::path(elements).expect("the expected path is a path")
    }

    #[test]
    fn path_variables_return_the_alternating_list() {
        let r = run(
            "MATCH p = (a:Person {id: $src})-[:KNOWS]->(b) \
             RETURN p, b.id AS id ORDER BY id",
            &[("src", Value::Int(0))],
        );
        assert_eq!(
            r.rows,
            [
                vec![path(vec![node(0), knows(0, 1), node(1)]), Value::Int(1)],
                vec![path(vec![node(0), knows(0, 2), node(2)]), Value::Int(2)],
            ]
        );
    }

    #[test]
    fn var_length_paths_splice_interior_nodes() {
        // Two hops from 0: through 1 to 2 and 3, through 2 to 4. The
        // chain's interior node appears between the endpoints.
        let r = run(
            "MATCH p = (a:Person {id: 0})-[:KNOWS*2..2]->(b) RETURN p ORDER BY p",
            &[],
        );
        assert_eq!(
            r.rows,
            [
                [path(vec![
                    node(0),
                    knows(0, 1),
                    node(1),
                    knows(1, 2),
                    node(2)
                ])],
                [path(vec![
                    node(0),
                    knows(0, 1),
                    node(1),
                    knows(1, 3),
                    node(3)
                ])],
                [path(vec![
                    node(0),
                    knows(0, 2),
                    node(2),
                    knows(2, 4),
                    node(4)
                ])],
            ]
        );
    }

    #[test]
    fn shortest_path_variables_return_whole_paths() {
        let r = run(
            "MATCH p = ANY SHORTEST (a:Person {id: 0})-[:KNOWS*]->(b) RETURN p ORDER BY p",
            &[],
        );
        assert_eq!(
            r.rows,
            [
                [path(vec![node(0), knows(0, 1), node(1)])],
                [path(vec![
                    node(0),
                    knows(0, 1),
                    node(1),
                    knows(1, 3),
                    node(3)
                ])],
                [path(vec![node(0), knows(0, 2), node(2)])],
                [path(vec![
                    node(0),
                    knows(0, 2),
                    node(2),
                    knows(2, 4),
                    node(4)
                ])],
                [path(vec![
                    node(0),
                    knows(0, 2),
                    node(2),
                    knows(2, 4),
                    node(4),
                    knows(4, 5),
                    node(5)
                ])],
            ]
        );
    }

    #[test]
    fn with_carries_a_materialized_path() {
        // The defining stage assembles p once at the WITH boundary;
        // the next stage reads the settled list from its chunks.
        let r = run(
            "MATCH p = (a:Person {id: 0})-[:KNOWS*2..2]->(b) WITH p, b WHERE b.id = 4 \
             RETURN size(p) AS n, p",
            &[],
        );
        assert_eq!(
            r.rows,
            [vec![
                Value::Int(5),
                path(vec![node(0), knows(0, 2), node(2), knows(2, 4), node(4)]),
            ]]
        );
    }

    #[test]
    fn var_length_rel_variables_settle_to_edge_lists() {
        let r = run(
            "MATCH (a:Person {id: 0})-[r:KNOWS*2..2]->(b) WHERE b.id = 4 RETURN r",
            &[],
        );
        assert_eq!(r.rows, [[Value::List(vec![knows(0, 2), knows(2, 4)])]]);
    }

    #[test]
    fn optional_match_paths_bind_null() {
        // Node 4 has exactly one 3-hop continuation (4, 5, 0, 1 or 2);
        // restricting b to an impossible id nulls the whole path.
        let r = run(
            "MATCH (a:Person {id: 4}) \
             OPTIONAL MATCH p = (a)-[:KNOWS]->(b {id: $none}) RETURN a.id AS id, p",
            &[("none", Value::Int(99))],
        );
        assert_eq!(r.rows, [vec![Value::Int(4), Value::Null]]);
    }

    #[test]
    fn pmr_chains_share_prefix_links_across_paths() {
        // Enumerate every trail from node 0 directly and count the
        // distinct chain links behind the emitted paths. The DFS emits
        // one link per path plus the shared root, while the settled
        // lists cost the sum of all path lengths, which is the memory
        // claim behind the representation.
        let schema = schema();
        let counts: BTreeMap<u32, u64> = schema
            .nodes()
            .iter()
            .map(|n| (n.id, n.node_count))
            .collect();
        let slot_loc = BTreeMap::new();
        let shapes = BTreeMap::new();
        let mut graph = mock();
        let mut ctx = StageCtx {
            graph: &mut graph,
            params: &[],
            counts: &counts,
            slot_loc: &slot_loc,
            path_shapes: &shapes,
            chunks: Vec::new(),
            states: Vec::new(),
            rows: Vec::new(),
            overlay: BTreeMap::new(),
            scratch: Vec::new(),
            edge_sets: BTreeMap::new(),
            isect: BTreeMap::new(),
            morsel: None,
            stats: Vec::new(),
            live: Vec::new(),
            notices: Vec::new(),
        };
        let rels = [RelStep {
            id: 2,
            from_table: 0,
            to_table: 0,
        }];
        let spec = VarSpec {
            rels: &rels,
            direction: RelDirection::Out,
            to_tables: &[0],
            min: 1,
            max: Some(6),
            mode: PathMode::Trail,
            levels: None,
        };
        let root = chain_root(0, 0);
        let (mut far, mut trails) = (Vec::new(), Vec::new());
        enumerate_paths(&mut ctx, &spec, 0, 0, &root, &mut far, &mut trails).expect("enumerate");
        let mut links = BTreeSet::new();
        let mut total_hops = 0u64;
        for trail in &trails {
            let Value::Chain(link) = trail else {
                panic!("var expand emits chains, got {trail:?}");
            };
            total_hops += link.hops;
            let mut cur = Some(link);
            while let Some(l) = cur {
                links.insert(Arc::as_ptr(l) as usize);
                cur = l.prev.as_ref();
            }
        }
        assert_eq!(links.len(), trails.len() + 1, "one link per path plus root");
        assert!(
            total_hops > links.len() as u64,
            "sharing beats materialized lists: {total_hops} hops in {} links",
            links.len()
        );
    }

    fn profiled(source: &str, params: &[(&str, Value)]) -> (QueryResult, Profile) {
        profiled_opts(source, params, Options::default())
    }

    fn profiled_opts(
        source: &str,
        params: &[(&str, Value)],
        options: Options,
    ) -> (QueryResult, Profile) {
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
        execute_profiled(&optimized, &query, &schema, &mut graph, &args, &options)
            .expect("execute profiled")
    }

    #[test]
    fn profile_counts_pulls_rows_and_names_ops() {
        let (r, p) = profiled(
            "MATCH (a:Person {id: 0})-[:KNOWS]->(b) RETURN b.id AS friend",
            &[],
        );
        assert_eq!(r.rows.len(), 2);
        assert_eq!(p.stages.len(), 1);
        let stage = &p.stages[0];
        assert_eq!(stage.sink, "Project");
        assert_eq!(stage.out_rows, 2);
        assert!(stage.nanos > 0);
        let names: Vec<&str> = stage.ops.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Source",
                "IndexLookup a: Person [id = 0]",
                "Flatten a",
                "Expand (a)-[:KNOWS]->(b)",
            ]
        );
        let lookup = &stage.ops[1];
        assert_eq!((lookup.pulls, lookup.rows), (1, 1));
        let expand = &stage.ops[3];
        assert_eq!((expand.pulls, expand.rows), (1, 2));
    }

    #[test]
    fn profile_shows_the_factorized_second_hop() {
        let (r, p) = profiled(
            "MATCH (a:Person {id: 0})-[:KNOWS]->(b)-[:KNOWS]->(c) \
             RETURN count(c) AS paths",
            &[],
        );
        assert_eq!(int_rows(&r), [[3]]);
        let stage = &p.stages[0];
        assert_eq!(stage.sink, "Aggregate");
        assert_eq!(stage.out_rows, 1);
        let expand_c = stage.ops.last().expect("ops");
        assert_eq!(expand_c.name, "ExpandCount (b)-[:KNOWS]->(c)");
        // One pull for the whole absorbed neighbor vector of node 0,
        // reporting three counted paths without materializing a list:
        // the count-to-degree rewrite at work.
        assert_eq!((expand_c.pulls, expand_c.rows), (1, 3));
        // No flatten sits between the two expands anymore.
        assert!(
            !stage.ops.iter().any(|o| o.name == "Flatten b"),
            "absorbed source still flattens: {:?}",
            stage.ops.iter().map(|o| &o.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn count_to_degree_matches_flat_execution() {
        let queries = [
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN count(c) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN count(*) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) \
             RETURN b.id AS id, count(c) AS n ORDER BY id",
            "MATCH (a:Person {id: 2})-[:KNOWS]-(b)-[:KNOWS]-(c) RETURN count(c) AS n",
            "MATCH (a:Person)<-[:KNOWS]-(b)<-[:KNOWS]-(c) RETURN count(c) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b)-[r:KNOWS]->(c) RETURN count(r) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b) WITH b MATCH (b)-[:KNOWS]->(c) \
             RETURN count(c) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN count(DISTINCT c) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) WHERE c.id > 1 \
             RETURN count(c) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) WHERE a.id < c.id \
             RETURN count(c) AS n",
            // Bound but unread rels: the dead-column pass leaves their
            // columns empty on both the project and aggregate paths,
            // and the compacting filter walks the survivors.
            "MATCH (a:Person)-[r:KNOWS]->(b) RETURN b.id AS id ORDER BY id",
            "MATCH (a:Person)-[r:KNOWS]->(b) WHERE b.id > 0 RETURN b.id AS id ORDER BY id",
            "MATCH (a:Person)-[r:KNOWS]->(b)-[s:KNOWS]->(c) RETURN count(c) AS n",
        ];
        for q in queries {
            let fac = run_with(q, &[], false);
            let flat = run_with(q, &[], true);
            assert_eq!(sorted(fac.rows), sorted(flat.rows), "query: {q}");
        }
    }

    #[test]
    fn referenced_rel_columns_still_materialize() {
        // The one read the dead-column pass must never break: a rel
        // slot the sink returns keeps its column.
        let out = run_with(
            "MATCH (a:Person {id: 0})-[r:KNOWS]->(b) RETURN r AS r",
            &[],
            false,
        );
        let mut rows = out.rows;
        rows.sort_by_key(|row| match row[0] {
            Value::Rel { dst, .. } => dst,
            _ => u64::MAX,
        });
        assert_eq!(
            rows,
            vec![
                vec![Value::Rel {
                    table: 2,
                    src: 0,
                    dst: 1
                }],
                vec![Value::Rel {
                    table: 2,
                    src: 0,
                    dst: 2
                }],
            ]
        );
    }

    #[test]
    fn count_to_degree_rewrites_only_untouched_chunks() {
        let cases = [
            (
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN count(c) AS n",
                true,
            ),
            (
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN count(*) AS n",
                true,
            ),
            // A key on b blocks the absorb but not the degree read.
            (
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) \
                 RETURN b.id AS id, count(c) AS n",
                true,
            ),
            // The trailing expand of a later stage absorbs the row
            // source feeding it.
            (
                "MATCH (a:Person)-[:KNOWS]->(b) WITH b MATCH (b)-[:KNOWS]->(c) \
                 RETURN count(c) AS n",
                true,
            ),
            // DISTINCT needs the values.
            (
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) \
                 RETURN count(DISTINCT c) AS n",
                false,
            ),
            // A second aggregate reads the chunk.
            (
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) \
                 RETURN count(c) AS n, min(c.id) AS m",
                false,
            ),
            // A predicate on one endpoint just flips the pattern: the
            // optimizer starts the join from the filtered side and the
            // expand on the far end still counts by degree, which the
            // differential test above pins as correct.
            (
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) WHERE c.id > 1 \
                 RETURN count(c) AS n",
                true,
            ),
            // A predicate across both ends of the trailing expand
            // reads its chunk, whichever end the join starts from.
            (
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) WHERE a.id < c.id \
                 RETURN count(c) AS n",
                false,
            ),
        ];
        for (q, want) in cases {
            let (_, p) = profiled(q, &[]);
            let got = p
                .stages
                .iter()
                .flat_map(|s| &s.ops)
                .any(|o| o.name.starts_with("ExpandCount"));
            assert_eq!(got, want, "query: {q}");
        }
    }

    #[test]
    fn profile_renders_one_block_per_stage() {
        let (_, p) = profiled(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg WHERE deg > 1 \
             RETURN a.id AS person ORDER BY person",
            &[],
        );
        assert_eq!(p.stages.len(), 2);
        let text = p.render();
        assert!(text.contains("stage 1: Aggregate + Filter"), "got:\n{text}");
        assert!(text.contains("stage 2: Project + Sort"), "got:\n{text}");
        assert!(text.contains("Scan a: Person"), "got:\n{text}");
        assert!(text.contains("RowSource a, deg"), "got:\n{text}");
        assert!(text.contains("pulls"), "got:\n{text}");
        let scan = p.stages[0]
            .ops
            .iter()
            .find(|o| o.name.starts_with("Scan"))
            .expect("scan op");
        assert_eq!((scan.pulls, scan.rows), (1, 6));
    }

    #[test]
    fn optional_match_keeps_rows_without_a_match() {
        // Edges into {4, 5}: 2->4, 3->4, 4->5. The other three people
        // keep their row with a null friend, which is exactly what an
        // inner gate would destroy.
        let r = run(
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id >= 4 \
             RETURN a.id AS id, b.id AS friend ORDER BY id",
            &[],
        );
        assert_eq!(
            r.rows,
            [
                [Value::Int(0), Value::Null],
                [Value::Int(1), Value::Null],
                [Value::Int(2), Value::Int(4)],
                [Value::Int(3), Value::Int(4)],
                [Value::Int(4), Value::Int(5)],
                [Value::Int(5), Value::Null],
            ]
        );
    }

    #[test]
    fn optional_props_never_fuse_into_the_outer_scan() {
        // The `{id: 2}` filter belongs to the optional group. Fusing it
        // into the outer scan would shrink the scan to one person and
        // silently turn the left-outer join inner.
        let r = run(
            "MATCH (a:Person) OPTIONAL MATCH (a {id: 2})-[:KNOWS]->(b) \
             RETURN a.id AS id, b.id AS friend ORDER BY id",
            &[],
        );
        assert_eq!(
            r.rows,
            [
                [Value::Int(0), Value::Null],
                [Value::Int(1), Value::Null],
                [Value::Int(2), Value::Int(4)],
                [Value::Int(3), Value::Null],
                [Value::Int(4), Value::Null],
                [Value::Int(5), Value::Null],
            ]
        );
    }

    #[test]
    fn optional_scan_miss_binds_null() {
        let r = run(
            "MATCH (a:Person {id: 0}) OPTIONAL MATCH (b:Person {id: 99}) \
             RETURN a.id AS a, b.id AS b",
            &[],
        );
        assert_eq!(r.rows, [[Value::Int(0), Value::Null]]);
    }

    #[test]
    fn chained_optional_matches_propagate_null() {
        // The second optional expands from b, which is null wherever
        // the first group missed; a null source matches nothing, so
        // the place stays null instead of erroring.
        let r = run(
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 \
             OPTIONAL MATCH (b)-[:IS_LOCATED_IN]->(p) \
             RETURN a.id AS id, b.id AS friend, p.id AS place ORDER BY id",
            &[],
        );
        assert_eq!(
            r.rows,
            [
                [Value::Int(0), Value::Null, Value::Null],
                [Value::Int(1), Value::Null, Value::Null],
                [Value::Int(2), Value::Int(4), Value::Int(1)],
                [Value::Int(3), Value::Int(4), Value::Int(1)],
                [Value::Int(4), Value::Int(5), Value::Int(2)],
                [Value::Int(5), Value::Null, Value::Null],
            ]
        );
    }

    #[test]
    fn optional_counts_skip_missing_values() {
        // count(a) sees every outer row, count(b) skips the nulls the
        // misses produced.
        let r = run(
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id >= 4 \
             RETURN count(a) AS people, count(b) AS friends",
            &[],
        );
        assert_eq!(int_rows(&r), [[6, 3]]);
    }

    #[test]
    fn multi_hop_optional_resets_per_outer_row() {
        // Two-hop trails ending at 0: only 4 -> 5 -> 0. The in-group
        // flatten between the expands must rearm per outer row.
        let r = run(
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c) \
             WHERE c.id = 0 RETURN a.id AS id, c.id AS c ORDER BY id",
            &[],
        );
        assert_eq!(
            r.rows,
            [
                [Value::Int(0), Value::Null],
                [Value::Int(1), Value::Null],
                [Value::Int(2), Value::Null],
                [Value::Int(3), Value::Null],
                [Value::Int(4), Value::Int(0)],
                [Value::Int(5), Value::Null],
            ]
        );
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
                "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                 RETURN count(*) AS triangles",
                &[],
            ),
            (
                "MATCH (a:Person)-[:KNOWS]-(b)-[:KNOWS]-(c), (a)-[:KNOWS]-(c) \
                 RETURN count(*) AS walks",
                &[],
            ),
            (
                "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg WHERE deg > 1 \
                 MATCH (a)-[:IS_LOCATED_IN]->(pl) RETURN a.id AS person, pl.id AS place",
                &[],
            ),
            (
                "MATCH (a:Person {id: 0})-[r:KNOWS*1..3]->(b) \
                 RETURN b.id AS b, size(r) AS hops",
                &[],
            ),
            (
                "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id >= 4 \
                 RETURN a.id AS id, b.id AS friend",
                &[],
            ),
            (
                "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c) \
                 WHERE c.id = 0 RETURN a.id AS id, c.id AS c",
                &[],
            ),
            (
                "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 \
                 OPTIONAL MATCH (b)-[:IS_LOCATED_IN]->(p) \
                 RETURN a.id AS id, count(p) AS places",
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

    /// The scheduler's contract is stronger than same-rows: partials
    /// merge in morsel order, which is scan order, so a parallel run
    /// must reproduce the sequential result exactly, row order,
    /// collect() order, and LIMIT cutoffs included.
    #[test]
    fn parallel_execution_matches_sequential_exactly() {
        let cases = [
            "MATCH (a:Person) RETURN a.id AS id",
            "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.id AS a, b.id AS b",
            "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.id AS a, b.id AS b ORDER BY b DESC, a",
            "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.id AS a, b.id AS b ORDER BY b, a \
             SKIP 2 LIMIT 3",
            "MATCH (a:Person)-[:KNOWS]->(b) RETURN DISTINCT b.id AS b",
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN count(c) AS paths",
            "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.id AS a, count(*) AS deg",
            "MATCH (a:Person)-[:KNOWS]->(b) RETURN a.id AS a, collect(b.id) AS friends",
            "MATCH (a:Person)-[:KNOWS]->(b) RETURN count(DISTINCT b.id) AS heads",
            "MATCH (a:Person)-[:KNOWS]->(b) \
             RETURN min(b.id) AS lo, max(b.id) AS hi, avg(b.id) AS mid, sum(b.id) AS total",
            "MATCH (a:Person)-[:KNOWS]->(b) WHERE a.id > 100 RETURN count(*) AS n",
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
             RETURN count(*) AS triangles",
            "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
             RETURN a.id AS a, b.id AS b, c.id AS c",
            "MATCH (a:Person)-[:KNOWS]-(b)-[:KNOWS]-(c), (a)-[:KNOWS]-(c) \
             RETURN count(*) AS walks",
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg WHERE deg > 1 \
             MATCH (a)-[:IS_LOCATED_IN]->(pl) RETURN a.id AS person, pl.id AS place",
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) WHERE b.id >= 4 \
             RETURN a.id AS id, b.id AS friend",
            "MATCH (a:Person)-[r:KNOWS*1..3]->(b) RETURN a.id AS a, b.id AS b, size(r) AS hops",
        ];
        for source in cases {
            let seq = run(source, &[]);
            let par = run_par(source, &[]);
            assert_eq!(
                seq.rows, par.rows,
                "parallel diverged from sequential on: {source}"
            );
        }
    }
}
