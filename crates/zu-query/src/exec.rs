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
//! from the start so only minimum-hop paths enumerate. An ANY SHORTEST
//! whose far end is pinned by the filter above it absorbs that filter
//! and answers as a single-pair search instead, two frontiers growing
//! towards each other from the two ends. That is the correctness-first
//! baseline; wiring the RecursiveBFS frontier engine underneath is the
//! rest of milestone 4.
//!
//! OPTIONAL MATCH executes as a left-outer group. Every flatten the
//! group needs on outer chunks sits below a `BracketBegin` that
//! yields each outer configuration exactly once per activation, the
//! group's operators run above it, and `BracketEnd` passes matches
//! through or, when an outer configuration produced nothing, binds the
//! group's chunks to a single null row. Filters born inside the
//! bracketed clause compile into the group, so a WHERE there gates
//! matches within the group instead of dropping the null row.
//!
//! `EXISTS { ... }` and `NOT EXISTS { ... }` compile into the same
//! bracket with a different kind on the end. A semi bracket hands the
//! outer row up on the first match and never asks the group for a
//! second one, an anti bracket does the opposite, and both collapse the
//! group's chunks to one row on the way out because the slots the block
//! introduced are out of scope above it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use zu_common::gqlstatus::{DiagnosticRecord, GqlStatus, codes};
use zu_common::{Clock, DurationKind, Interrupt, Result, Temporal, ZuError, temporal};

use crate::ast::{
    BinaryOp, Conjunction, EdgeEnd, Literal, PathMode, RelDirection, Selector, SetOp, SortKey,
    UnaryOp,
};
use crate::binder::{
    BoundExpr, BoundItem, BoundQuery, Deviation, Func, PathPart, Percentile, Schema, TableFunc,
};
use crate::column::Held;
use crate::plan::{Bracket, BracketKind, LogicalPlan, Side, expr_text};
use crate::refs::{BindingTable, GraphHandle};
use crate::row::{Batch, Flow};

/// Vector width of one chunk fill.
pub const VECTOR_SIZE: usize = 2048;

/// An engine-internal failure: a slot that is not bound, a multiplicity
/// that overflowed, a shape the optimizer should never have produced.
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
        /// Which edge of `src -> dst` this is: the row its properties
        /// sit in, which is the edge's place in the load order. The
        /// endpoints do not name an edge on their own, because a pair
        /// may run more than once and each copy carries its own values.
        /// [`Graph::edge_ordinal`] resolves one for an edge that
        /// arrived as a pair and nothing else, and answers with the
        /// first of the run.
        ord: u64,
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
    /// GV60. A graph reference: which graph, not the graph. The engine
    /// hands one out and a query carries it; there is no literal that
    /// writes one, because a graph is in the catalog and not in the
    /// text.
    Graph(GraphHandle),
    /// GV61. A binding table reference: the rows of some earlier
    /// result, behind a handle so that passing the table costs a
    /// pointer rather than a copy.
    BindingTable(Arc<BindingTable>),
}

impl Value {
    /// The `ord` of an edge that has no row in the stored property
    /// columns: one a write staged and no fold has landed yet, or one
    /// a pattern matched over a different rel table. A property read
    /// off such an edge answers null rather than reading somebody
    /// else's row.
    pub const NO_REL_ROW: u64 = u64::MAX;

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
pub(crate) fn settle(v: Value) -> Value {
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

/// The node a chain begins at, which is the root link's, read by
/// walking to the root.
///
/// Only SIMPLE asks, and it asks once per node the walk stands on
/// rather than once per edge, so the walk it costs is the one
/// [`chain_has_node`] does anyway.
fn chain_start(link: &PathLink) -> (u32, u64) {
    let mut cur = link;
    while let Some(prev) = cur.prev.as_deref() {
        cur = prev;
    }
    match cur.node {
        Value::Node { table, offset } => (table, offset),
        // A chain's links hold nodes, and the root's is the node the
        // walk was started from.
        ref other => unreachable!("a path chain begins at a node, not at {other:?}"),
    }
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
    pub rows: Rows,
    pub notices: Vec<DiagnosticRecord>,
}

/// The answer's rows, which the executor may not have built.
///
/// A sink that filled columns has the whole answer already, in a shape
/// a third of the size and one a columnar client takes as it stands.
/// Building the rows out of them costs an allocation each, and most of
/// the callers that would pay it never look, so the rows are built the
/// first time somebody reads them and not before.
///
/// It derefs to the `Vec<Vec<Value>>` it always was, so a caller writes
/// what a caller always wrote. `len` and `is_empty` are answered off
/// the columns without building anything, because "how many rows" is
/// the question a caller who wants no rows asks.
///
/// A caller who reads both keeps both, since the columns are what a
/// second reader is answered from and a result may be read twice. The
/// one who wants only rows says so with [`Rows::into_vec`], which takes
/// them and drops everything else.
#[derive(Debug, Default)]
pub struct Rows {
    /// What the sink filled, when it filled columns.
    held: Option<Held>,
    /// The rows, once somebody has asked for them.
    rows: OnceLock<Vec<Vec<Value>>>,
}

impl Rows {
    /// The rows a caller handed over, which are already built.
    pub fn of(rows: Vec<Vec<Value>>) -> Rows {
        Rows {
            held: None,
            rows: OnceLock::from(rows),
        }
    }

    /// The columns a sink filled, whose rows nobody has asked for.
    pub fn held(held: Held) -> Rows {
        Rows {
            held: Some(held),
            rows: OnceLock::new(),
        }
    }

    /// How many rows, without building one.
    pub fn len(&self) -> usize {
        match (&self.held, self.rows.get()) {
            (_, Some(rows)) => rows.len(),
            (Some(held), None) => held.rows,
            (None, None) => 0,
        }
    }

    /// Whether the answer has no rows, without building one.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The columns the sink filled, when it filled any.
    pub fn columns(&self) -> Option<&Held> {
        self.held.as_ref()
    }

    /// The rows, taken by a caller who wants them and nothing else.
    pub fn into_vec(mut self) -> Vec<Vec<Value>> {
        std::mem::take(&mut *self)
    }

    /// The columns themselves, out of a result nobody will read again.
    ///
    /// [`Rows::columns`] lends them, which is what lets a result be read
    /// twice and is also what makes an export copy: a buffer that leaves
    /// through a borrow has to be copied, because the one behind it stays
    /// where it is. This is the other half of that bargain, and it is
    /// crate-private on purpose. Taking the columns leaves a `Rows` that
    /// says it has none, so the only safe place to call it from is
    /// [`QueryResult::into_columns`], which owns the whole result and
    /// gives nobody the chance to look at what is left.
    pub(crate) fn take_columns(&mut self) -> Option<Held> {
        self.held.take()
    }

    /// The rows, built out of the columns the first time this is asked.
    fn built(&self) -> &Vec<Vec<Value>> {
        self.rows
            .get_or_init(|| self.held.as_ref().map(Held::rows).unwrap_or_default())
    }
}

impl std::ops::Deref for Rows {
    type Target = Vec<Vec<Value>>;

    fn deref(&self) -> &Vec<Vec<Value>> {
        self.built()
    }
}

impl std::ops::DerefMut for Rows {
    /// The rows, built and then owned by the caller.
    ///
    /// The columns go here, because a caller holding the rows mutably is
    /// a caller who may be about to change them, and a copy of the same
    /// answer that does not change with them is a copy that lies.
    fn deref_mut(&mut self) -> &mut Vec<Vec<Value>> {
        self.built();
        self.held = None;
        self.rows.get_mut().expect("built above")
    }
}

impl From<Vec<Vec<Value>>> for Rows {
    fn from(rows: Vec<Vec<Value>>) -> Rows {
        Rows::of(rows)
    }
}

impl IntoIterator for Rows {
    type Item = Vec<Value>;
    type IntoIter = std::vec::IntoIter<Vec<Value>>;

    fn into_iter(mut self) -> Self::IntoIter {
        std::mem::take(&mut *self).into_iter()
    }
}

impl<'a> IntoIterator for &'a Rows {
    type Item = &'a Vec<Value>;
    type IntoIter = std::slice::Iter<'a, Vec<Value>>;

    fn into_iter(self) -> Self::IntoIter {
        self.built().iter()
    }
}

impl Clone for Rows {
    fn clone(&self) -> Rows {
        Rows {
            held: self.held.clone(),
            rows: self.rows.clone(),
        }
    }
}

impl PartialEq for Rows {
    /// Two answers are equal when their rows are, whichever of them the
    /// executor happened to build columns for.
    fn eq(&self, other: &Rows) -> bool {
        self.len() == other.len() && **self == **other
    }
}

/// Equal to the rows themselves, in the three spellings a caller
/// writes them in, so a test that knows what it expects writes the
/// rows it expects and nothing about how they were built.
impl<T> PartialEq<Vec<T>> for Rows
where
    Vec<Value>: PartialEq<T>,
{
    fn eq(&self, other: &Vec<T>) -> bool {
        **self == *other
    }
}

impl<T> PartialEq<[T]> for Rows
where
    Vec<Value>: PartialEq<T>,
{
    fn eq(&self, other: &[T]) -> bool {
        **self == *other
    }
}

impl<T, const N: usize> PartialEq<[T; N]> for Rows
where
    Vec<Value>: PartialEq<T>,
{
    fn eq(&self, other: &[T; N]) -> bool {
        **self == *other
    }
}

impl PartialEq<Rows> for Vec<Vec<Value>> {
    fn eq(&self, other: &Rows) -> bool {
        *self == **other
    }
}

impl QueryResult {
    /// The result of a statement that ran to completion.
    pub fn new(columns: Vec<String>, rows: Vec<Vec<Value>>) -> Self {
        QueryResult {
            columns,
            rows: Rows::of(rows),
            notices: Vec::new(),
        }
    }

    /// The result of a statement whose sink filled columns.
    pub fn held(columns: Vec<String>, held: Held) -> Self {
        QueryResult {
            columns,
            rows: Rows::held(held),
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

/// The rows each table has lost to `DELETE`, ascending, keyed by
/// table id. The sets are shared rather than copied because a reader
/// hands the same one to every worker of a query.
///
/// It comes in two halves because that is how the storage layer holds
/// it, and putting them together would cost the length of the whole
/// set on every statement. `sealed` is what the tombstone chains in
/// the file say, and it only changes when a fold rewrites them;
/// `fresh` is what the commits since then took away. A row is gone
/// when either half names it, so a lookup is two binary searches on a
/// table that has deleted something and two map misses on one that
/// has not.
#[derive(Clone, Debug, Default)]
pub struct DeletedRows {
    sealed: BTreeMap<u32, Arc<[u64]>>,
    fresh: BTreeMap<u32, Arc<[u64]>>,
}

impl DeletedRows {
    /// Nothing deleted, which is what an engine that cannot delete
    /// answers and what a file that has only been written to holds.
    pub fn new() -> Self {
        Self::default()
    }

    /// The two halves as the storage layer has them.
    pub fn of(sealed: BTreeMap<u32, Arc<[u64]>>, fresh: BTreeMap<u32, Arc<[u64]>>) -> Self {
        Self { sealed, fresh }
    }

    /// Whether any table has lost a row.
    pub fn is_empty(&self) -> bool {
        self.sealed.is_empty() && self.fresh.is_empty()
    }

    /// Whether one row of one table is a row a `DELETE` took away.
    pub fn holds(&self, table: u32, offset: u64) -> bool {
        let names = |half: &BTreeMap<u32, Arc<[u64]>>| {
            half.get(&table)
                .is_some_and(|rows| rows.binary_search(&offset).is_ok())
        };
        names(&self.sealed) || names(&self.fresh)
    }
}

impl FromIterator<(u32, Arc<[u64]>)> for DeletedRows {
    fn from_iter<I: IntoIterator<Item = (u32, Arc<[u64]>)>>(rows: I) -> Self {
        Self {
            sealed: rows.into_iter().collect(),
            fresh: BTreeMap::new(),
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
    /// The property row of the edge `src -> dst`, `None` when there is
    /// no such edge. A pair that runs more than once names as many
    /// edges as it has copies and this answers with the first of them,
    /// which is all a caller holding nothing but the pair can be told.
    /// A caller walking a list takes [`Graph::neighbor_ordinals`]
    /// instead and gets every copy's own row.
    ///
    /// The default is the answer for an engine that stores nothing on
    /// an edge: the probe decides whether there is a row and the row
    /// number is never read.
    fn edge_ordinal(&mut self, rel: u32, src: u64, dst: u64) -> Result<Option<u64>> {
        Ok(self.has_edge(rel, src, dst)?.then_some(0))
    }
    /// The whole run of `src -> dst`: the row of its first edge and how
    /// many edges the pair holds, `None` when there is no such edge.
    ///
    /// A pattern that binds both of its endpoints matches once per edge
    /// and not once per pair, so an operator closing onto a bound node
    /// needs the count as well as the row. The default is one edge per
    /// pair, which is right for an engine whose adjacency cannot hold a
    /// pair twice.
    fn edge_run(&mut self, rel: u32, src: u64, dst: u64) -> Result<Option<(u64, u64)>> {
        Ok(self.edge_ordinal(rel, src, dst)?.map(|ord| (ord, 1)))
    }
    /// Replaces `out` with the property row of every edge
    /// [`Graph::neighbors`] lists for the same arguments, in the same
    /// order, so a caller that walked the list holds a row per edge
    /// rather than a row per pair. `len` is the length of the list the
    /// caller got, which every implementation has to fill.
    ///
    /// The default is the answer for an engine that stores nothing on
    /// an edge, one row number per neighbor and none of them read.
    fn neighbor_ordinals(
        &mut self,
        rel: u32,
        node: u64,
        reversed: bool,
        len: usize,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let _ = (rel, node, reversed);
        out.clear();
        out.resize(len, 0);
        Ok(())
    }
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
    /// The same property of many rows of one table, in `out` in the
    /// caller's row order, the bulk read behind a filter over a
    /// scanned vector.
    ///
    /// The default loops [`Graph::property`], which is what every
    /// engine did before there was a way to ask for more than one at a
    /// time. An engine that stores a column together overrides it: a
    /// vector of rows wants one column, and resolving the name and
    /// finding the column's chunk once for the vector is the whole
    /// difference between a filter that reads a column and one that
    /// looks a property up a thousand times.
    fn properties(
        &mut self,
        table: u32,
        rows: &[u64],
        key: &str,
        out: &mut Vec<Value>,
    ) -> Result<()> {
        out.clear();
        out.reserve(rows.len());
        for &row in rows {
            out.push(self.property(table, row, key)?);
        }
        Ok(())
    }
    /// The labels one node carries, one bit per label id of the
    /// graph's dictionary. The default is what an engine that stores no
    /// label beyond the table's own says, and the binder has already
    /// answered that one by narrowing the tables, so a test that
    /// reaches here is asking about a label such an engine cannot have.
    fn labels(&mut self, table: u32, offset: u64) -> Result<u64> {
        let _ = (table, offset);
        Ok(0)
    }
    /// One property of one edge, named by the property row a rel value
    /// carries. The default is the answer an engine that stores nothing
    /// on an edge gives, which is the answer every engine gave before
    /// edges could hold anything.
    fn rel_property(&mut self, rel: u32, ord: u64, key: &str) -> Result<Value> {
        let _ = (rel, ord, key);
        Ok(Value::Null)
    }
    /// G115. Whether the nodes of a table carry a property of this
    /// name. It is a question about the table rather than about one
    /// row, so a property that is there and null answers true, and the
    /// row is passed in only because a store may keep its properties
    /// per row rather than per table.
    ///
    /// The default asks for the value and reads a refused read as an
    /// absent property, which is the answer for a store whose
    /// properties are its columns. An engine whose reads fail for other
    /// reasons answers the question directly instead.
    fn has_property(&mut self, table: u32, offset: u64, key: &str) -> Result<bool> {
        Ok(self.property(table, offset, key).is_ok())
    }
    /// The same question asked of an edge's table.
    ///
    /// The default reads the property and calls a null absent, which is
    /// as close as a store that answers reads and nothing else can
    /// come, and is exact for the engines whose edges hold nothing.
    fn has_rel_property(&mut self, rel: u32, ord: u64, key: &str) -> Result<bool> {
        Ok(!matches!(self.rel_property(rel, ord, key)?, Value::Null))
    }
    /// How many rows a table has taken on past the count the schema
    /// carries for it, read once per query beside the deleted set.
    ///
    /// A schema says how many rows a table holds, and a scan of it
    /// walks that many, so a commit the engine has not yet folded into
    /// the schema's own source leaves its rows above the count. This is
    /// how many, and a scan reaches them by walking that much further.
    /// The default is what an engine whose count is always current
    /// answers.
    fn appended(&mut self, table: u32) -> Result<u64> {
        let _ = table;
        Ok(0)
    }
    /// The rows a `DELETE` took away, read once per query. A delete
    /// does not compact, because every edge names its endpoints by row
    /// offset, so a scan still walks the row it took and this is what
    /// says the row is gone. The default is what an engine whose rows
    /// only ever arrive answers.
    fn deleted(&mut self) -> Result<DeletedRows> {
        Ok(DeletedRows::new())
    }
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
    /// What the sink of a plain projection keeps.
    pub sink: Sink,
    /// The handle whoever asked for the statement can stop it through,
    /// and the count of rows it has read so far. Here rather than as a
    /// parameter of its own because every execution path already
    /// carries the switches, and a cancellation that only some of them
    /// were handed would be a cancellation that works on some
    /// statements. The default is the handle nobody armed, so a caller
    /// who wants none pays a branch per chunk.
    pub interrupt: Interrupt,
    /// The instant the datetime value functions of ISO 20.27 answer,
    /// `None` for a run that has not read a clock yet.
    ///
    /// It is here for the reason the interrupt is: it is state one
    /// statement has and every path through the executor already
    /// carries the switches. [`run_stages`] fills it in on the way in
    /// and hands the filled copy down, so a query written inside an
    /// expression answers the same instant as the query around it
    /// rather than reading a clock of its own halfway through the scan.
    /// A caller may set it before running, which is how a test pins the
    /// time.
    pub clock: Option<Clock>,
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

/// What the sink of a projection with nothing above it keeps.
/// `Columns` fills the buffers the executor computed in, which is the
/// default and is what a columnar client reads without a transpose.
/// `Rows` flattens each row as it is found, the way every other sink
/// does, and is the baseline the columns are measured against and the
/// answer they have to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sink {
    #[default]
    Columns,
    Rows,
}

// ---------------------------------------------------------------------------
// Profiling
// ---------------------------------------------------------------------------

/// One operator line of an EXPLAIN ANALYZE profile.
#[derive(Debug, Clone)]
pub struct OpProfile {
    /// The operator itself, the word a reader groups by.
    pub kind: &'static str,
    /// What the operator is working on, rendered the way the statement
    /// wrote it: the tables a scan reads, the pattern an expand walks,
    /// the predicate a filter asks. Empty where the operator has no
    /// arguments to name.
    pub detail: String,
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
    /// source, the flattens, and the brackets themselves, which pass
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
    /// What the listing calls this operator, the kind and its detail
    /// joined the way EXPLAIN ANALYZE prints them.
    pub fn name(&self) -> String {
        match self.detail.is_empty() {
            true => self.kind.to_string(),
            false => format!("{} {}", self.kind, self.detail),
        }
    }

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
            let names: Vec<String> = stage.ops.iter().map(OpProfile::name).collect();
            let width = names.iter().map(String::len).max().unwrap_or(0);
            for (ix, op) in stage.ops.iter().enumerate().rev() {
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
                    names[ix],
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

/// The selector as the plan listing says it, the empty string for a
/// pattern that keeps every path. Both listings read it from here, so
/// what a profile prints and what an explain prints cannot drift.
pub(crate) fn selector_text(selector: Option<Selector>) -> String {
    match selector {
        None => String::new(),
        Some(Selector::Any(k)) => format!(" any {k}"),
        Some(Selector::AnyShortest) => " any shortest".into(),
        Some(Selector::AllShortest) => " all shortest".into(),
        Some(Selector::Shortest(k)) => format!(" shortest {k}"),
        Some(Selector::ShortestGroup(k)) => format!(" shortest {k} group"),
    }
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
    let (left, right) = direction.spelling();
    format!("({from}){left}[:{names}]{right}({to})")
}

/// What a profiled operator is and what it is doing, kept apart so a
/// reader of a profile can group by the operator without cutting the
/// printed label back up. [`OpProfile::name`] is the two joined, which
/// is what the listing shows.
fn op_label(
    desc: &OpDesc,
    stage: &StageDef,
    query: &BoundQuery,
    schema: &Schema,
) -> (&'static str, String) {
    let var = |slot: usize| query.variables[slot].name.as_str();
    match desc {
        OpDesc::Source => ("Source", String::new()),
        OpDesc::RowSource { chunk } => ("RowSource", slot_names(&stage.chunk_slots[*chunk], query)),
        OpDesc::ArgSource { chunk, .. } => {
            ("ArgSource", slot_names(&stage.chunk_slots[*chunk], query))
        }
        OpDesc::Scan { tables, chunk } => (
            "Scan",
            format!(
                "{}: {}",
                var(stage.chunk_slots[*chunk][0]),
                node_tables_text(tables, schema)
            ),
        ),
        OpDesc::IndexLookup { tables, key, chunk } => (
            "IndexLookup",
            format!(
                "{}: {} [id = {}]",
                var(stage.chunk_slots[*chunk][0]),
                node_tables_text(tables, schema),
                expr_text(key, query)
            ),
        ),
        OpDesc::Flatten { chunk } => ("Flatten", slot_names(&stage.chunk_slots[*chunk], query)),
        OpDesc::Expand {
            from,
            direction,
            rels,
            chunk,
            degrees,
            ..
        } => (
            if *degrees { "ExpandCount" } else { "Expand" },
            rel_text(
                var(*from),
                var(stage.chunk_slots[*chunk][0]),
                *direction,
                rels,
                schema,
            ),
        ),
        OpDesc::VarExpand {
            from,
            direction,
            rels,
            min,
            max,
            mode,
            selector,
            target,
            edge_filter,
            reach,
            counts,
            chunk,
            ..
        } => {
            let max = max.map_or(String::new(), |v| v.to_string());
            let mode = match mode {
                PathMode::Walk => " walk",
                PathMode::Trail => "",
                PathMode::Simple => " simple",
                PathMode::Acyclic => " acyclic",
            };
            let sel = selector_text(*selector);
            let pinned = target.as_ref().map_or(String::new(), |key| {
                format!(" [id = {}]", expr_text(key, query))
            });
            // The edge predicate reads as part of the walk, because
            // that is where it runs: an edge that fails it is not
            // followed, so it is not a filter over the result.
            let gate = edge_filter.as_ref().map_or(String::new(), |expr| {
                format!(" where {}", expr_text(expr, query))
            });
            // The walk that keeps endpoints rather than paths reads
            // as `reach`, because the row count it produces is the
            // reachable set and not the path count. The one that counts
            // the paths without building them reads as `count`.
            let kind = match (*reach, *counts) {
                (true, _) => " reach",
                (_, true) => " count",
                _ => "",
            };
            (
                "VarExpand",
                format!(
                    "*{min}..{max}{mode}{sel}{kind} {}{pinned}{gate}",
                    rel_text(
                        var(*from),
                        var(stage.chunk_slots[*chunk][0]),
                        *direction,
                        rels,
                        schema
                    )
                ),
            )
        }
        OpDesc::ExpandInto {
            from,
            to,
            direction,
            rels,
            ..
        } => (
            "ExpandInto",
            rel_text(var(*from), var(*to), *direction, rels, schema),
        ),
        OpDesc::AspJoin {
            from,
            to,
            direction,
            rels,
            retain,
            ..
        } => (
            "AspJoin",
            format!(
                "{}{}",
                if retain.is_some() { "(retain) " } else { "" },
                rel_text(var(*from), var(*to), *direction, rels, schema)
            ),
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
            (
                "MultiwayIntersect",
                format!(
                    "{} & {}",
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
                ),
            )
        }
        OpDesc::Filter { expr, .. } => ("Filter", expr_text(expr, query)),
        OpDesc::Unwind { expr, chunk, .. } => (
            "Unwind",
            format!(
                "{} AS {}",
                expr_text(expr, query),
                var(stage.chunk_slots[*chunk][0])
            ),
        ),
        OpDesc::TableFunction {
            func, rel, chunk, ..
        } => {
            let table = schema
                .rel_by_id(*rel)
                .map(|r| r.name.as_str())
                .unwrap_or("?");
            let cols: Vec<&str> = stage.chunk_slots[*chunk].iter().map(|&s| var(s)).collect();
            (
                "Call",
                format!("{}({table}) YIELD {}", func.name(), cols.join(", ")),
            )
        }
        OpDesc::BracketBegin => ("BracketBegin", String::new()),
        OpDesc::BracketEnd { chunks, kind, .. } => {
            let slots: Vec<usize> = chunks
                .iter()
                .flat_map(|&c| stage.chunk_slots[c].iter().copied())
                .collect();
            let name = match kind {
                BracketKind::Optional => "Optional",
                BracketKind::Semi => "Semi",
                BracketKind::Anti => "Anti",
                // The mark is the one kind that names something above
                // it, the variable it writes its answer into, so the
                // listing says which one that is.
                BracketKind::Mark { slot, negated } => {
                    let not = if *negated { "not " } else { "" };
                    return (
                        "Mark",
                        format!("{not}{} into {}", slot_names(&slots, query), var(*slot)),
                    );
                }
            };
            (name, slot_names(&slots, query))
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

/// Total order over values for grouping, DISTINCT, ORDER BY and the
/// inequality operators: nulls first, then booleans, numbers (int and
/// float compare numerically), strings, temporals, nodes, rels, lists,
/// records, paths.
///
/// The wrapper is the sort key; [`value_order`] is the order itself
/// and is what a predicate calls, so the two never drift apart.
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
        value_order(&self.0, &other.0)
    }
}

/// Which of two types comes first when a comparison meets both.
///
/// GA04 is universal comparison and ISO does not say what the order
/// between two types is, so this table is zu's answer to IV010 and it
/// is one answer for the whole engine: a sort key, a DISTINCT, a GROUP
/// BY and a `<` all read it. It is stated here rather than left to
/// whatever order the enum happened to be declared in, because a
/// documented choice can be relied on and a declaration order cannot.
fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 2,
        Value::Str(_) => 3,
        // A temporal value sorts after the strings and before the
        // references, and two of different kinds sort by kind, because
        // a date and a duration have no order between them and the
        // total order still owes an answer.
        Value::Temporal(_) => 4,
        Value::Node { .. } => 5,
        Value::Rel { .. } => 6,
        Value::List(_) | Value::Chain(_) => 7,
        // A record sorts after every list, and two records sort by
        // their fields, name first and then value.
        Value::Record(_) => 8,
        // A path sorts after every record, and two paths sort by their
        // elements, which is the list order over the same sequence.
        Value::Path(_) => 9,
        // GV60 and GV61 sort last, after everything the language can
        // write down. They are handles rather than data, so where they
        // go is a choice with nothing to recommend one place over
        // another, and the end is the place that moves no other type.
        Value::Graph(_) => 10,
        Value::BindingTable(_) => 11,
    }
}

/// The order between any two values, which is total: every pair has an
/// answer and the answer is the same one wherever it is asked.
///
/// Within a type it is the type's own order. Between two types it is
/// [`rank`]. The null is smaller than every value here, which is not
/// where a query sees it: an ORDER BY that names neither NULLS FIRST
/// nor NULLS LAST moves it to the end (IS001) and a comparison against
/// it is the unknown truth value rather than a true or a false, both of
/// which the callers handle before reaching this.
pub fn value_order(a: &Value, b: &Value) -> Ordering {
    if matches!(a, Value::Chain(_)) || matches!(b, Value::Chain(_)) {
        return value_order(&settle(a.clone()), &settle(b.clone()));
    }
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => float_order(*a, *b),
        (Value::Int(a), Value::Float(b)) => float_order(*a as f64, *b),
        (Value::Float(a), Value::Int(b)) => float_order(*a, *b as f64),
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
                ord: r1,
            },
            Value::Rel {
                table: t2,
                src: s2,
                dst: d2,
                ord: r2,
            },
            // The row is last and it is what separates two copies of
            // one pair, so an order over rels is the order over their
            // endpoints with the copies of a pair kept in load order
            // inside it.
        ) => (t1, s1, d1, r1).cmp(&(t2, s2, d2, r2)),
        (Value::List(a), Value::List(b)) | (Value::Path(a), Value::Path(b)) => {
            for (x, y) in a.iter().zip(b) {
                let ord = value_order(x, y);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            a.len().cmp(&b.len())
        }
        (Value::Record(a), Value::Record(b)) => {
            for ((na, va), (nb, vb)) in a.iter().zip(b) {
                let ord = na.cmp(nb).then_with(|| value_order(va, vb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            a.len().cmp(&b.len())
        }
        // Two graph references order by what identifies them, which is
        // the catalog id; the names ride along and are the same
        // whenever the id is, and the epoch is when the reference was
        // taken rather than which graph it names, so ordering on it
        // would put two references to one graph in two places and
        // disagree with the equality above.
        (Value::Graph(a), Value::Graph(b)) => a.id.cmp(&b.id),
        // Two binding table references order by handle number, which
        // is creation order. Ordering them by their rows would say two
        // tables holding the same rows are one, and they are two.
        (Value::BindingTable(a), Value::BindingTable(b)) => a.id().cmp(&b.id()),
        (a, b) => rank(a).cmp(&rank(b)),
    }
}

/// Two floats in the total order.
///
/// A NaN is neither less than, equal to, nor greater than any number,
/// and the order still owes an answer, so every NaN sorts after every
/// number and two NaNs sort equal. That is GA01's placement and it is
/// not [`f64::total_cmp`], which would read the sign bit of a NaN as
/// part of the value and put a negative one below every number. The
/// two zeroes sort equal for the same reason `=` says they are equal:
/// an order that disagreed with equality would make DISTINCT keep two
/// values a query cannot tell apart.
fn float_order(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).expect("neither operand is NaN"),
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
    /// Whether this table's edges have no direction (GH02), which is
    /// what [`RelDirection::resolve`] reads to turn the pattern's
    /// direction into the stored lists this step walks.
    undirected: bool,
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
    /// Feeds an argument's rows in as unflat batches: the same thing
    /// [`OpDesc::RowSource`] does for a stage boundary, for a row set
    /// that came from a run of its own. That is what the clauses after
    /// a write read.
    ArgSource {
        arg: usize,
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
        /// The key of the one endpoint this expansion is asked about,
        /// absorbed from the equality filter that followed it. Set only
        /// under ANY SHORTEST, where it turns the whole operator into a
        /// single-pair search that meets in the middle.
        target: Option<BoundExpr>,
        /// The step's own `WHERE`, asked of every edge before it is
        /// walked, and the slot it reads that edge out of. A path
        /// through an edge that fails it is never built.
        edge_filter: Option<BoundExpr>,
        edge_slot: Option<usize>,
        /// True when the stage throws the paths away and keeps the
        /// endpoints: nothing reads the rel slot and the answer is a
        /// set. The walk then visits each node once instead of once
        /// per path. See [`rewrite_reach_varlen`].
        reach: bool,
        /// Count mode, the PMR half of the count-to-degree rewrite
        /// (docs/07 §5): nothing reads this chunk but one bare count,
        /// so the walk counts the paths off the shortest-path DAG
        /// instead of building them. A node's paths are its
        /// predecessors' paths added up, which is one pass over the
        /// levelled graph where enumerating is one walk per path, and
        /// there can be exponentially many of those between the same
        /// two nodes. Set only under a shortest selector, where the
        /// levels make the graph acyclic and the sum is exact.
        counts: bool,
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
        /// Where in the chunk the counter of a `WITH ORDINALITY` or a
        /// `WITH OFFSET` goes, and what its first element is numbered.
        /// The chunk holds the value and the number side by side, so
        /// the operator writes two columns of the same length rather
        /// than the counter being an operator of its own.
        ordinal: Option<i64>,
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
    /// Bottom of a bracketed group: yields the current outer
    /// configuration exactly once per activation, so the group's
    /// operators exhaust per outer row and a miss is detectable.
    BracketBegin,
    /// Top of a bracketed group. What it does with a match and with a
    /// miss is the kind's business; the chunks are the group's own, and
    /// they are bound to a single null row wherever a row leaves here
    /// without one of the group's matches behind it.
    BracketEnd {
        /// Index of the matching `BracketBegin` in the stage.
        begin: usize,
        /// Chunks introduced inside the group.
        chunks: Vec<usize>,
        kind: BracketKind,
        /// The one-column chunk a mark writes its answer into, made
        /// below the group so it outlives the rearm and read above the
        /// group like any other column. `None` for the other kinds,
        /// which answer by keeping the row or dropping it.
        mark: Option<usize>,
    },
}

#[derive(Debug, Clone)]
enum PostOp {
    Distinct,
    Filter(BoundExpr),
    Sort(Vec<SortKey<BoundExpr>>),
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
    /// The one unflat chunk the argument reads, if any; the optimizer
    /// flattens the rest.
    arg_chunk: Option<usize>,
    /// The second argument of a binary set function, which is the
    /// percentiles and nothing else: the standard's independent value
    /// expression, the fraction of the way through the group to
    /// answer. It reads no slot, which the compiler checks, so it is
    /// the same value for every row and is evaluated once, when the
    /// group is finalized.
    fraction: Option<BoundExpr>,
}

#[derive(Debug, Clone)]
struct SinkDef {
    /// Projection items in clause order; for aggregations this
    /// interleaves keys and aggregates exactly as written.
    items: Vec<BoundItem>,
    aggregate: bool,
    /// One per aggregate item in clause order, then one per hidden sort
    /// aggregate. The two live in one list because a group accumulates
    /// them together; what tells them apart is that the second lot has
    /// no column, which is why the slots below say where they go.
    aggs: Vec<AggSpec>,
    /// GF20. The slot each hidden sort aggregate finalizes into, in the
    /// order those specs sit at the end of `aggs`.
    order_slots: Vec<usize>,
    post: Vec<PostOp>,
    /// Slots snapshotted per row for post-projection filters.
    extra_slots: Vec<usize>,
}

#[derive(Debug, Clone)]
struct StageDef {
    descs: Vec<OpDesc>,
    /// The optimizer's row estimate for each operator, `None` where no
    /// logical operator owns it (the source, the brackets,
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
        crate::binder::expr_slots(expr, &mut slots);
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

fn input_of(plan: &LogicalPlan) -> Option<&LogicalPlan> {
    match plan {
        // A conjoin has two inputs and no one of them is the input, so
        // it is not a link in a chain and the linearizer stops at it.
        // It never reaches here in a run: [`execute`] takes the
        // composite apart above the pipeline and runs each operand as
        // a query of its own. A fork is the same shape of thing, one
        // plan per way, and the session runs each of them as a part.
        LogicalPlan::Empty
        | LogicalPlan::Rows { .. }
        | LogicalPlan::Conjoin { .. }
        | LogicalPlan::Fork { .. } => None,
        LogicalPlan::ScanNodes { input, .. }
        | LogicalPlan::Expand { input, .. }
        | LogicalPlan::Filter { input, .. }
        | LogicalPlan::Unwind { input, .. }
        | LogicalPlan::Insert { input, .. }
        | LogicalPlan::Set { input, .. }
        | LogicalPlan::Delete { input, .. }
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
            crate::binder::expr_slots(other, &mut slots);
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
            undirected: rd.undirected,
        });
    }
    // An expand with no table to walk finds nothing, which is the
    // answer to a step whose type the graph has no table for.
    Ok(steps)
}

/// The bracket a plan operator sits in, `None` for a plain match and
/// for the operators that never carry one.
fn bracket_of(op: &LogicalPlan) -> Option<Bracket> {
    match op {
        LogicalPlan::ScanNodes { bracket, .. }
        | LogicalPlan::Expand { bracket, .. }
        | LogicalPlan::Filter { bracket, .. } => *bracket,
        _ => None,
    }
}

/// Whether the operator after an expand is an edge probe into that
/// expand's endpoint that the WCOJ fusion can absorb: a single-step
/// fixed-direction closing expand in the same group whose other end is
/// already bound, carrying the optimizer's mark unless
/// the mode forces the fusion. Returns the probe's rel slot, source
/// slot, direction, and rel step, and flattens the source's chunk so
/// the fused operator reads one configuration per pull.
fn wcoj_fusion(
    b: &mut StageBuilder,
    lookahead: Option<&LogicalPlan>,
    to: usize,
    bracket: Option<Bracket>,
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
        bracket: probe_bracket,
        ..
    }) = lookahead
    else {
        return Ok(None);
    };
    if *probe_bracket != bracket || !(*wcoj || b.wcoj == Wcoj::Force) {
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
        (*to2, direction.flip())
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
/// Fusion requires the filter to share the scan's group, otherwise a
/// bracketed `{id: k}` filter would fuse into a plain scan and take
/// the group's own predicate out of it.
fn compile_match_op(
    b: &mut StageBuilder,
    op: &LogicalPlan,
    lookahead: Option<&LogicalPlan>,
    query: &BoundQuery,
    schema: &Schema,
) -> Result<bool> {
    match op {
        LogicalPlan::ScanNodes { slot, bracket, .. } => {
            // No candidate table is a scan over nothing, not a fault.
            // The binder leaves the list empty when the pattern asked
            // for labels the graph holds nowhere, and the answer to
            // that is an empty binding table.
            let tables = query.variables[*slot].node_tables.clone();
            let fused = lookahead.and_then(|next| match next {
                LogicalPlan::Filter {
                    expr,
                    bracket: filter_bracket,
                    ..
                } if filter_bracket == bracket => index_key(expr, *slot),
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
            bracket,
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
                && let Some(fused) = wcoj_fusion(b, lookahead, *to, *bracket, query, schema)?
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
                // A shortest search whose far end is pinned by the very
                // next filter is a single-pair question, and answering
                // it by levelling the whole reachable component and
                // throwing all but one endpoint away is the wrong
                // search. Absorbing the key lets the operator meet in
                // the middle instead. Only ANY SHORTEST takes it: ALL
                // SHORTEST has to know the minimum hop count of every
                // node on both sides before it can enumerate, which the
                // meeting search deliberately never learns.
                // The lower bound has to be the one hop the meeting
                // search assumes as well: it answers with the path it
                // met on, which is a shortest one, and a pattern asking
                // for at least three hops would have that thrown away
                // rather than answered from a longer path.
                let target = if v.selector == Some(Selector::AnyShortest) && v.min.unwrap_or(1) <= 1
                {
                    lookahead.and_then(|next| match next {
                        LogicalPlan::Filter {
                            expr,
                            bracket: filter_bracket,
                            ..
                        } if filter_bracket == bracket => index_key(expr, *to),
                        _ => None,
                    })
                } else {
                    None
                };
                let consumed = target.is_some();
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
                    target,
                    edge_filter: v.edge_filter.clone(),
                    edge_slot: v.edge_slot,
                    reach: false,
                    counts: false,
                    chunk,
                });
                b.produced(chunk);
                return Ok(consumed);
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

/// Compiles one bracketed group: flattens for the outer chunks the
/// group reads, then `BracketBegin`, the group's operators, and the
/// `BracketEnd` that decides what an outer row is worth. Returns the
/// linear index just past the group.
fn compile_bracket(
    b: &mut StageBuilder,
    linear: &[&LogicalPlan],
    start: usize,
    query: &BoundQuery,
    schema: &Schema,
    est: &[crate::optimizer::Estimate],
) -> Result<usize> {
    let group = bracket_of(linear[start]);
    let mut end = start + 1;
    while end < linear.len() && bracket_of(linear[end]) == group {
        end += 1;
    }
    // Every outer chunk the group reads must flatten below the
    // boundary, so one `BracketBegin` activation is exactly one outer
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
            LogicalPlan::Filter { expr, .. } => crate::binder::expr_slots(expr, &mut read),
            _ => {}
        }
    }
    b.expand_shapes(&mut read);
    for slot in read {
        if let Some(&(c, _)) = b.slot_loc.get(&slot) {
            b.ensure_flat(c);
        }
    }
    // The mark's column is made before the boundary: the group's own
    // chunks are rebound to a null row every time the group is rearmed,
    // and the answer has to survive that to be read above.
    let mark = match group.map(|g| g.kind) {
        Some(BracketKind::Mark { slot, .. }) => Some(b.new_chunk(vec![slot], true)),
        _ => None,
    };
    let begin = b.descs.len();
    // The brackets belong to no logical operator, so they carry no
    // estimate and the profile leaves their column blank.
    b.cur_est = None;
    b.push(OpDesc::BracketBegin);
    // Nothing below the boundary may compact through the group, and
    // nothing above may compact through the `BracketEnd`.
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
    b.push(OpDesc::BracketEnd {
        begin,
        chunks: (first_chunk..b.chunk_slots.len()).collect(),
        kind: group.expect("a bracket group has a kind").kind,
        mark,
    });
    b.compactable = None;
    Ok(end)
}

/// Slots one operator reads.
fn desc_refs(desc: &OpDesc, out: &mut BTreeSet<usize>) {
    match desc {
        OpDesc::IndexLookup { key, .. } => crate::binder::expr_slots(key, out),
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
        OpDesc::Filter { expr, .. } | OpDesc::Unwind { expr, .. } => {
            crate::binder::expr_slots(expr, out)
        }
        OpDesc::TableFunction { args, .. } => {
            args.iter().for_each(|e| crate::binder::expr_slots(e, out))
        }
        OpDesc::Source
        | OpDesc::RowSource { .. }
        | OpDesc::ArgSource { .. }
        | OpDesc::Scan { .. }
        | OpDesc::Flatten { .. }
        | OpDesc::BracketBegin
        | OpDesc::BracketEnd { .. } => {}
    }
}

/// Slots the sink's post operators read.
fn post_refs(post: &[PostOp], out: &mut BTreeSet<usize>) {
    for op in post {
        match op {
            PostOp::Filter(e) | PostOp::Skip(e) | PostOp::Limit(e) => {
                crate::binder::expr_slots(e, out)
            }
            PostOp::Sort(keys) => keys
                .iter()
                .for_each(|k| crate::binder::expr_slots(&k.expr, out)),
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
///
/// A variable-length walk under a shortest selector takes the same
/// rewrite and answers it off the PMR (docs/07 §5): the levels make
/// the searched graph acyclic, so a node's shortest paths are its
/// predecessors' shortest paths added up and the whole count is one
/// breadth-first pass. Enumerating instead is one walk per path, and
/// two nodes can have exponentially many shortest paths between them,
/// so this is the difference between a number and a search that does
/// not finish. `ANY SHORTEST` keeps one path per endpoint by
/// definition and counts its endpoints.
fn rewrite_count_expand(
    b: &mut StageBuilder,
    items: &[BoundItem],
    aggs: &mut [AggSpec],
    post: &[PostOp],
    extra: &BTreeSet<usize>,
    aggregate: bool,
) {
    // A bracket holds absolute operator indices and decides its own
    // multiplicity; leave its stages alone.
    if b.descs
        .iter()
        .any(|d| matches!(d, OpDesc::BracketBegin | OpDesc::BracketEnd { .. }))
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
        crate::binder::expr_slots(&item.expr, &mut full_refs);
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
                crate::binder::expr_slots(&item.expr, &mut others);
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
    // The counting expand, and the walk that counts paths the same
    // way. A shortest walk is the one kind of walk whose paths can be
    // counted without being built: the levels make the graph acyclic,
    // so a node's paths are its predecessors' paths added up. A walk
    // that already answers one path per endpoint is pinned to a single
    // far node, or keeps endpoints rather than paths, has nothing to
    // count this way and is left alone.
    let (c, walk) = match &b.descs[target] {
        OpDesc::Expand { chunk, .. } => (*chunk, false),
        OpDesc::VarExpand {
            chunk,
            selector,
            min,
            target: None,
            reach: false,
            ..
        } if matches!(
            selector,
            Some(Selector::AnyShortest | Selector::AllShortest)
        ) && *min <= 1 =>
        {
            (*chunk, true)
        }
        _ => return,
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
        crate::binder::expr_slots(&item.expr, &mut refs);
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
        crate::binder::expr_slots(arg, &mut arg_refs);
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
    if walk {
        if let OpDesc::VarExpand { counts, .. } = &mut b.descs[target] {
            *counts = true;
        }
        return;
    }
    let OpDesc::Expand { from, .. } = b.descs[target] else {
        unreachable!("the fixed-length arm above matched an expand")
    };
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

/// The reachability rewrite over a finished stage: a variable-length
/// expand whose paths the stage throws away walks each node once
/// instead of once per path.
///
/// Two things have to hold. Nothing outside the expand reads its rel
/// slot, so no path is ever looked at; the edge predicate does not
/// count, since it reads the edge being walked rather than the answer.
/// And the stage answers a set: either a DISTINCT over the projected
/// rows, or a grouping every one of whose aggregates is a DISTINCT of
/// its own. Everything between the expand and the sink is a function
/// of the row, so a duplicate in makes a duplicate out and nothing
/// else, and the duplicate is what both of those throw away.
///
/// A minimum above one hop is refused. The walk visits a node at the
/// fewest hops that reach it and never again, so a node the query
/// wants only at three hops but that also sits at one would go
/// missing. The endpoint set is otherwise the same as the enumeration
/// would find, for every path mode: the fewest-hops walk to a node
/// repeats no node and so repeats no edge, which is a path all three
/// modes allow. The one node that needs saying twice is the start,
/// which the walk marks before it moves and which a cycle can still
/// reach, so it is emitted on being met again unless the mode forbids
/// a repeated node.
fn rewrite_reach_varlen(
    b: &mut StageBuilder,
    items: &[BoundItem],
    aggs: &[AggSpec],
    post: &[PostOp],
    extra: &BTreeSet<usize>,
    aggregate: bool,
) {
    let a_set = if aggregate {
        !aggs.is_empty() && aggs.iter().all(|spec| spec.distinct)
    } else {
        post.iter().any(|op| matches!(op, PostOp::Distinct))
            // A window over rows that are a set is still a window over
            // the order they arrive in, and this changes that order.
            && !post
                .iter()
                .any(|op| matches!(op, PostOp::Skip(_) | PostOp::Limit(_)))
    };
    if !a_set {
        return;
    }
    // A bracket decides its own multiplicity, and the DISTINCT this
    // read is outside it: leave those stages alone.
    if b.descs
        .iter()
        .any(|d| matches!(d, OpDesc::BracketBegin | OpDesc::BracketEnd { .. }))
    {
        return;
    }
    let mut refs = BTreeSet::new();
    for desc in &b.descs {
        desc_refs(desc, &mut refs);
    }
    for item in items {
        crate::binder::expr_slots(&item.expr, &mut refs);
    }
    for spec in aggs {
        if let Some(arg) = &spec.arg {
            crate::binder::expr_slots(arg, &mut refs);
        }
    }
    post_refs(post, &mut refs);
    refs.extend(extra.iter().copied());
    b.expand_shapes(&mut refs);
    for desc in &mut b.descs {
        if let OpDesc::VarExpand {
            min,
            selector: None,
            target: None,
            reach,
            chunk,
            ..
        } = desc
            && *min <= 1
            && !refs.contains(&b.chunk_slots[*chunk][1])
        {
            *reach = true;
        }
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
    // The leaf is not an operator that pulls, so it stays out of the
    // linearized run and is compiled below the rest of them.
    let leaf = cur;
    linear.reverse();

    let projections: Vec<(&Vec<BoundItem>, &Vec<BoundItem>)> = query
        .clauses
        .iter()
        .filter_map(|c| match c {
            crate::binder::BoundClause::Project {
                items, order_aggs, ..
            } => Some((items, order_aggs)),
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
    if let LogicalPlan::Rows { slots, arg } = leaf {
        let chunk = b.new_chunk(slots.clone(), false);
        b.push(OpDesc::ArgSource { arg: *arg, chunk });
        b.produced(chunk);
    }

    let mut i = 0;
    while i < linear.len() {
        b.cur_est = est.get(i).copied();
        if bracket_of(linear[i]).is_some() {
            i = compile_bracket(&mut b, &linear, i, query, schema, &est)?;
            continue;
        }
        match linear[i] {
            LogicalPlan::Empty
            | LogicalPlan::Rows { .. }
            | LogicalPlan::Conjoin { .. }
            | LogicalPlan::Fork { .. } => {
                unreachable!("a leaf never appears in the linearized ops")
            }
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
            LogicalPlan::Unwind {
                expr,
                slot,
                ordinal,
                ..
            } => {
                for c in b.unflat_of(expr)? {
                    b.ensure_flat(c);
                }
                let mut slots = vec![*slot];
                slots.extend(ordinal.map(|ordinal| ordinal.slot));
                let chunk = b.new_chunk(slots, false);
                b.push(OpDesc::Unwind {
                    expr: expr.clone(),
                    chunk,
                    ordinal: ordinal.map(|ordinal| ordinal.start),
                });
                b.produced(chunk);
            }
            LogicalPlan::Insert { .. } | LogicalPlan::Set { .. } | LogicalPlan::Delete { .. } => {
                // A write is not an operator here. The session splits
                // the statement at it, runs the clauses before it,
                // writes once for each row they answered, and runs the
                // clauses after it over those rows, because this
                // executor reads through a graph and a graph reads.
                // Nothing else runs a plan holding one of these.
                return Err(invalid(
                    "a write is run by the session that owns the log, not by the executor".into(),
                ));
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
                let (items, order_aggs) = projections
                    .get(proj_ix)
                    .ok_or_else(|| invalid("more sinks than projection clauses".into()))?;
                let items = items.to_vec();
                let order_aggs = order_aggs.to_vec();
                proj_ix += 1;
                let aggregate = matches!(linear[i], LogicalPlan::Aggregate { .. });

                let mut aggs = Vec::new();
                let mut order_slots = Vec::new();
                if aggregate {
                    // Grouping keys must be flat; each aggregate
                    // argument may keep exactly one unflat vector.
                    for item in items.iter().filter(|it| !it.aggregate) {
                        for c in b.unflat_of(&item.expr)? {
                            b.ensure_flat(c);
                        }
                    }
                    // The aggregates with a column first, in clause
                    // order, then the ones a sort key asked for. The
                    // second lot accumulates exactly like the first,
                    // which is why one loop reads both.
                    for item in items
                        .iter()
                        .filter(|it| it.aggregate)
                        .chain(order_aggs.iter())
                    {
                        let BoundExpr::Call {
                            func,
                            distinct,
                            star,
                            args,
                            ..
                        } = &item.expr
                        else {
                            return Err(invalid(format!(
                                "aggregate item '{}' must be a bare call for now",
                                item.name
                            )));
                        };
                        // A scalar function wrapping a set function, as
                        // in `size(collect(n))`, is a call and so gets
                        // this far, but it is a projection over the
                        // grouped rows rather than an aggregate of its
                        // own. Refuse it here: without this the spec
                        // below asks for an accumulator no scalar
                        // function has.
                        if !func.is_aggregate() {
                            return Err(invalid(format!(
                                "aggregate item '{}' applies a scalar function to a set function, which needs a projection after the grouping and is not implemented yet",
                                item.name
                            )));
                        }
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
                        // The percentiles' second argument. The
                        // standard says it is one value for the whole
                        // group and says nothing about what happens if
                        // a query writes something that is not, which
                        // is a question worth refusing rather than
                        // answering with whichever row arrived first.
                        // Reading no slot is the strict form of that
                        // and it is a compile time check, so a literal
                        // and a parameter both pass and a column does
                        // not.
                        let fraction = args.get(1).cloned();
                        if let Some(expr) = &fraction {
                            let mut slots = BTreeSet::new();
                            crate::binder::expr_slots(expr, &mut slots);
                            if !slots.is_empty() {
                                return Err(invalid(format!(
                                    "the second argument of {}() is the fraction to answer and has to be the same for the whole group, so it cannot read a column",
                                    crate::functions::name_of(*func)
                                )));
                            }
                        }
                        aggs.push(AggSpec {
                            func: *func,
                            distinct: *distinct,
                            star: *star,
                            arg,
                            arg_chunk,
                            fraction,
                        });
                    }
                    for item in &order_aggs {
                        order_slots.push(item.slot.ok_or_else(|| {
                            invalid("a sort key's aggregate lost its slot, this is a bug".into())
                        })?);
                    }
                }

                let mut post = Vec::new();
                let mut extra = BTreeSet::new();
                while let Some(op) = linear.get(i + 1) {
                    match op {
                        LogicalPlan::Distinct { .. } => post.push(PostOp::Distinct),
                        LogicalPlan::Filter { expr, .. } => {
                            crate::binder::expr_slots(expr, &mut extra);
                            post.push(PostOp::Filter(expr.clone()));
                        }
                        LogicalPlan::Sort { keys, .. } => post.push(PostOp::Sort(keys.clone())),
                        LogicalPlan::Skip { expr, .. } => post.push(PostOp::Skip(expr.clone())),
                        LogicalPlan::Limit { expr, .. } => post.push(PostOp::Limit(expr.clone())),
                        _ => break,
                    }
                    i += 1;
                }

                // A hidden sort aggregate reads its argument the way a
                // projected one does, and the two rewrites below decide
                // what a column may drop by what is read, so they are
                // handed the items and the hidden ones together. Left
                // out, the argument of `ORDER BY count(r.weight)` would
                // look like a slot nobody wants.
                let read: Vec<BoundItem> = items.iter().chain(&order_aggs).cloned().collect();
                rewrite_count_expand(&mut b, &read, &mut aggs, &post, &extra, aggregate);
                rewrite_reach_varlen(&mut b, &read, &aggs, &post, &extra, aggregate);

                let unflat = (0..b.chunk_flat.len())
                    .filter(|&c| !b.chunk_flat[c])
                    .collect();
                let sink = SinkDef {
                    items,
                    aggregate,
                    aggs,
                    order_slots,
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

    /// Keeps each position as many times as `times` says, which is a
    /// filter when every entry is zero or one and a filter that also
    /// duplicates when one is more.
    ///
    /// The fused semijoin needs the second: a pair joined by two edges
    /// matches twice, and the row it keeps stands for both. Duplicating
    /// costs a pass and a copy, so a mask that repeats nothing takes
    /// [`Chunk::retain`], which is every graph whose adjacency holds a
    /// pair once.
    fn repeat(&mut self, times: &[usize]) {
        if times.iter().all(|&n| n <= 1) {
            let keep: Vec<bool> = times.iter().map(|&n| n == 1).collect();
            self.retain(&keep);
            return;
        }
        let total: usize = times.iter().sum();
        for col in &mut self.cols {
            if col.is_empty() {
                continue;
            }
            let mut out = Vec::with_capacity(total);
            for (pos, &n) in times.iter().enumerate() {
                for _ in 0..n {
                    out.push(col[pos].clone());
                }
            }
            *col = out;
        }
        self.size = total;
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

/// The accumulated edges of one rel step of an ASP join: every pair,
/// each mapped to the run it names, the property row of its first edge
/// and how many edges it holds.
///
/// The sweep that fills this walks the forward lists source by source,
/// which is the load order, so the running count is the row, and a pair
/// that repeats has its copies next to each other in the list so the
/// count is one increment per copy. That is the same `(row, count)`
/// pair a storage probe answers with, which is what lets the ASP plan
/// and the storage-probe plan agree edge for edge.
type EdgeSet =
    std::collections::HashMap<(u64, u64), (u64, u64), std::hash::BuildHasherDefault<EdgeHasher>>;

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
    /// The value query expressions of this query, read here by
    /// [`BoundExpr::Scalar`] (GQ18): the ones that decorrelated as the
    /// values they answered before the first stage ran, and the ones
    /// that did not as the plans this run works out per row.
    scalars: &'a Scalars<'a>,
    counts: &'a BTreeMap<u32, u64>,
    /// The rows a delete took away, which a scan of a table's extent
    /// walks straight over and every source here filters out.
    gone: &'a DeletedRows,
    slot_loc: &'a BTreeMap<usize, (usize, usize)>,
    /// Path variable shapes from the binder, the assembly recipe
    /// behind [`assemble_path`]. No operator produces a path slot.
    path_shapes: &'a BTreeMap<usize, Vec<PathPart>>,
    /// The caller's handle on this run, read at every pull and written
    /// at every scan. A worker driving a morsel holds the same handle
    /// the calling thread does, so one ask stops all of them.
    stop: &'a Interrupt,
    chunks: Vec<Chunk>,
    states: Vec<OpState>,
    rows: Vec<Vec<Value>>,
    /// Row-context values that shadow the chunks: projection aliases
    /// during materialization, the row snapshot during post filters.
    overlay: BTreeMap<usize, Value>,
    scratch: Vec<u64>,
    /// The accumulated edge sets of each ASP join, keyed by operator
    /// index and built on the join's first pull, one set per rel step
    /// in storage orientation. Deliberately outside `states` so a
    /// bracket's rearm never throws the accumulate away, and
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
                "read of an unflattened vector, the optimizer missed a flatten".into(),
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
        let Some(direction) = direction.resolve(step.undirected) else {
            continue;
        };
        if direction.walks_out() && table == step.from_table {
            total += graph.degree(step.id, offset, false)?;
        }
        if direction.walks_in() && table == step.to_table {
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
            | OpDesc::ArgSource { chunk, .. }
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
            OpDesc::Source | OpDesc::Filter { .. } | OpDesc::BracketBegin => {}
            OpDesc::BracketEnd { .. } => {}
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
/// brackets pass rows through without producing any, so
/// their count belongs to whatever is under them.
fn counts_rows(desc: &OpDesc) -> bool {
    !matches!(
        desc,
        OpDesc::Source | OpDesc::Flatten { .. } | OpDesc::BracketBegin | OpDesc::BracketEnd { .. }
    )
}

/// How many values one successful pull produced: chunk producers
/// report their chunk size, a compacting filter the surviving size,
/// and pass-through operators one configuration.
fn produced_rows(descs: &[OpDesc], ctx: &StageCtx, i: usize) -> u64 {
    match &descs[i] {
        OpDesc::Source | OpDesc::Flatten { .. } => 1,
        OpDesc::Filter { compact: None, .. } => 1,
        OpDesc::BracketBegin | OpDesc::BracketEnd { .. } => 1,
        OpDesc::Filter {
            compact: Some(c), ..
        } => ctx.chunks[*c].size as u64,
        OpDesc::AspJoin {
            retain: Some(f), ..
        } => ctx.chunks[*f].size as u64,
        OpDesc::RowSource { chunk }
        | OpDesc::ArgSource { chunk, .. }
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
    /// The levelled graph a shortest walk runs down, `None` for a walk
    /// that reads the graph itself. Every hop of it is a hop of some
    /// shortest path, so the walk follows it without asking storage
    /// anything and without checking the mode's repeat rule: a
    /// shortest path repeats no node, so every mode allows it.
    steps: Option<&'a Steps>,
    gate: EdgeGate<'a>,
}

/// Every hop leaving `(table, offset)` across the pattern's rel steps
/// and direction: the rel value, the far table, and the far offset, in
/// storage order, which keeps enumeration deterministic.
fn hop_edges(
    ctx: &mut StageCtx,
    rels: &[RelStep],
    direction: RelDirection,
    gate: EdgeGate<'_>,
    table: u32,
    offset: u64,
) -> Result<Vec<(Value, u32, u64)>> {
    let mut hops: Vec<(Value, u32, u64)> = Vec::new();
    let mut nbrs = Vec::new();
    let mut ords = Vec::new();
    for step in rels {
        let Some(direction) = direction.resolve(step.undirected) else {
            continue;
        };
        if direction.walks_out() && table == step.from_table {
            ctx.graph.neighbors(step.id, offset, false, &mut nbrs)?;
            ctx.graph
                .neighbor_ordinals(step.id, offset, false, nbrs.len(), &mut ords)?;
            for (&dst, &ord) in nbrs.iter().zip(&ords) {
                hops.push((
                    Value::Rel {
                        table: step.id,
                        src: offset,
                        dst,
                        ord,
                    },
                    step.to_table,
                    dst,
                ));
            }
        }
        if direction.walks_in() && table == step.to_table {
            ctx.graph.neighbors(step.id, offset, true, &mut nbrs)?;
            ctx.graph
                .neighbor_ordinals(step.id, offset, true, nbrs.len(), &mut ords)?;
            for (&src, &ord) in nbrs.iter().zip(&ords) {
                hops.push((
                    Value::Rel {
                        table: step.id,
                        src,
                        dst: offset,
                        ord,
                    },
                    step.from_table,
                    src,
                ));
            }
        }
    }
    if gate.expr.is_some() {
        let mut kept = Vec::with_capacity(hops.len());
        for hop in hops {
            if edge_passes(ctx, gate, &hop.0)? {
                kept.push(hop);
            }
        }
        return Ok(kept);
    }
    Ok(hops)
}

/// The step's own `WHERE` and the slot it reads the edge out of. Every
/// search that walks edges takes one, so a predicate written on the
/// step prunes the walk itself and not the paths it has already built.
/// The default gate lets everything through.
#[derive(Clone, Copy)]
struct EdgeGate<'a> {
    expr: Option<&'a BoundExpr>,
    slot: Option<usize>,
}

impl EdgeGate<'_> {
    /// The gate a step with no `WHERE` walks through.
    const OPEN: EdgeGate<'static> = EdgeGate {
        expr: None,
        slot: None,
    };
}

/// Whether one edge satisfies the step's predicate. The edge goes into
/// the overlay under the slot the binder gave it for as long as the
/// predicate is being read, so an expression that names the step's
/// variable sees this edge and not the list of them. Null is a miss,
/// the same as it is in a WHERE.
fn edge_passes(ctx: &mut StageCtx, gate: EdgeGate<'_>, rel: &Value) -> Result<bool> {
    let (Some(expr), Some(slot)) = (gate.expr, gate.slot) else {
        return Ok(true);
    };
    let shadowed = ctx.overlay.insert(slot, rel.clone());
    let verdict = eval(ctx, expr);
    match shadowed {
        Some(previous) => ctx.overlay.insert(slot, previous),
        None => ctx.overlay.remove(&slot),
    };
    Ok(truth(&verdict?)? == Some(true))
}

/// How many of the paths a walk finds it keeps, per endpoint, which is
/// the path selector of ISO 16.6 as the walk sees it.
///
/// The selector names a pair of endpoints and one of the two is fixed
/// for the whole walk, since a `VarExpand` starts from one node per
/// row, so an endpoint here is the far one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// Every path, in the order the walk came to them.
    All,
    /// The first k the walk comes to, which is `ANY k`. Depth first, so
    /// they are not the shortest k and are not promised to be: the
    /// standard leaves which ones to the engine.
    First(u64),
    /// The k of least length, which is `SHORTEST k`, and `ANY SHORTEST`
    /// when k is one.
    Least(u64),
    /// Every path whose length is one of the k least, which is
    /// `SHORTEST k GROUP`, and `ALL SHORTEST` when k is one.
    LeastGroups(u64),
}

impl Keep {
    /// What a selector asks the walk for. `None` is `ALL PATHS`.
    ///
    /// Two of these are named twice by the standard, which is why the
    /// pairs land in one place here: `ANY SHORTEST` is one path of the
    /// least length and so is `SHORTEST 1`, and `ALL SHORTEST` is every
    /// path of the least length and so is `SHORTEST 1 GROUP`.
    fn of(selector: Option<Selector>) -> Keep {
        match selector {
            None => Keep::All,
            Some(Selector::Any(k)) => Keep::First(k),
            Some(Selector::AnyShortest) => Keep::Least(1),
            Some(Selector::Shortest(k)) => Keep::Least(k),
            Some(Selector::AllShortest) => Keep::LeastGroups(1),
            Some(Selector::ShortestGroup(k)) => Keep::LeastGroups(k),
        }
    }
}

/// The paths one endpoint is holding, by their length, shortest first.
type Lengths = BTreeMap<u64, Vec<Arc<PathLink>>>;

/// The paths a walk is keeping, and the rule it keeps them by.
///
/// `All` and `First` answer as the walk runs and hold nothing besides
/// the answer. The two that go by length cannot: a path of the least
/// length may turn up after a longer one, so they hold what they have
/// per endpoint and throw away as they go, which bounds what they hold
/// by the answer rather than by the graph.
struct Paths {
    keep: Keep,
    far: Vec<Value>,
    trails: Vec<Value>,
    /// How many paths each endpoint has taken, for `First`.
    counts: BTreeMap<(u32, u64), u64>,
    /// What each endpoint is holding, by path length, for the two that
    /// go by length.
    held: BTreeMap<(u32, u64), Lengths>,
}

impl Paths {
    fn new(keep: Keep) -> Paths {
        Paths {
            keep,
            far: Vec::new(),
            trails: Vec::new(),
            counts: BTreeMap::new(),
            held: BTreeMap::new(),
        }
    }

    /// Offers one path, ending at `(table, offset)`, to the rule.
    fn push(&mut self, table: u32, offset: u64, link: &Arc<PathLink>) {
        match self.keep {
            Keep::All => self.emit(table, offset, link.clone()),
            Keep::First(k) => {
                let taken = self.counts.entry((table, offset)).or_insert(0);
                if *taken >= k {
                    return;
                }
                *taken += 1;
                self.emit(table, offset, link.clone());
            }
            Keep::Least(k) => {
                let held = self.held.entry((table, offset)).or_default();
                let total: usize = held.values().map(Vec::len).sum();
                if total as u64 >= k {
                    // Full, so this path is worth holding only if it is
                    // shorter than the longest one held, and the longest
                    // is the last key because the map is ordered.
                    let (&longest, _) = held.iter().next_back().expect("a full endpoint holds one");
                    if link.hops >= longest {
                        return;
                    }
                    let bucket = held.get_mut(&longest).expect("the key was just read");
                    bucket.pop();
                    if bucket.is_empty() {
                        held.remove(&longest);
                    }
                }
                held.entry(link.hops).or_default().push(link.clone());
            }
            Keep::LeastGroups(k) => {
                let held = self.held.entry((table, offset)).or_default();
                if held.len() as u64 >= k && !held.contains_key(&link.hops) {
                    // This length is a new group, and the endpoint has
                    // all the groups it is allowed. It is worth having
                    // only in place of the longest one held, which then
                    // goes in full: a group is every path of its length
                    // or it is not a group.
                    let (&longest, _) = held.iter().next_back().expect("a full endpoint holds one");
                    if link.hops >= longest {
                        return;
                    }
                    held.remove(&longest);
                }
                held.entry(link.hops).or_default().push(link.clone());
            }
        }
    }

    fn emit(&mut self, table: u32, offset: u64, link: Arc<PathLink>) {
        self.far.push(Value::Node { table, offset });
        self.trails.push(Value::Chain(link));
    }

    /// The two columns the operator answers with: the far node of every
    /// path kept, and the path.
    ///
    /// The rules that held paths back give them up here, endpoint by
    /// endpoint and shortest first, so the order is the graph's order
    /// rather than the order the walk happened to stumble on.
    fn finish(mut self) -> (Vec<Value>, Vec<Value>) {
        let held = std::mem::take(&mut self.held);
        for ((table, offset), lengths) in held {
            for (_, links) in lengths {
                for link in links {
                    self.emit(table, offset, link);
                }
            }
        }
        (self.far, self.trails)
    }
}

/// Depth-first path enumeration for `VarExpand`: every path of
/// `min..=max` hops from the start node under the mode's repeat rule,
/// WALK unrestricted (the binder guarantees a bound), TRAIL with no
/// repeated edge, ACYCLIC with no repeated node, SIMPLE with no
/// repeated node but for the one the path started at. A path whose endpoint
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
    out: &mut Paths,
) -> Result<()> {
    let depth = link.hops;
    if depth >= spec.min && spec.to_tables.contains(&table) {
        out.push(table, offset, link);
    }
    if spec.max.is_some_and(|m| depth >= m) {
        return Ok(());
    }
    // Where the path began, which SIMPLE is the one mode to care about:
    // it is the node the path may come back to, and the node it may not
    // walk on from once it has.
    let began = match spec.mode {
        PathMode::Simple => Some(chain_start(link)),
        _ => None,
    };
    if began == Some((table, offset)) && depth > 0 {
        // A simple path standing where it began. Every node ahead of it
        // is one the path already holds, or the start a second time, so
        // there is nothing legal left to walk.
        return Ok(());
    }
    // The levelled walk reads its hops out of the DAG the prepass
    // built, which is the point of building one: a node is reached by
    // one path prefix per path through it, and asking storage again
    // for each of those is the read the DAG spends memory to remove.
    let read;
    let hops: &[(Value, u32, u64)] = match spec.steps {
        Some(steps) => steps.get(&(table, offset)).map_or(&[][..], Vec::as_slice),
        None => {
            read = hop_edges(ctx, spec.rels, spec.direction, spec.gate, table, offset)?;
            &read
        }
    };
    for (rel_val, next_table, next_offset) in hops {
        let (next_table, next_offset) = (*next_table, *next_offset);
        if spec.steps.is_none() {
            let repeats = match spec.mode {
                PathMode::Walk => false,
                PathMode::Trail => chain_has_rel(link, rel_val),
                PathMode::Acyclic => chain_has_node(link, next_table, next_offset),
                // The step back to the start is the one repeat SIMPLE
                // allows, and the guard above stops the path there.
                PathMode::Simple => {
                    began != Some((next_table, next_offset))
                        && chain_has_node(link, next_table, next_offset)
                }
            };
            if repeats {
                continue;
            }
        }
        let child = Arc::new(PathLink {
            prev: Some(link.clone()),
            rel: Some(rel_val.clone()),
            node: Value::Node {
                table: next_table,
                offset: next_offset,
            },
            hops: depth + 1,
        });
        enumerate_paths(ctx, spec, next_table, next_offset, &child, out)?;
    }
    Ok(())
}

/// The set of rows a walk has already been to, one bit per row per
/// table. Row ids are dense and the walk asks about every edge it
/// sees, so a direct index beats hashing two words that many times.
#[derive(Default)]
struct Seen(BTreeMap<u32, Vec<u64>>);

impl Seen {
    fn holds(&self, table: u32, offset: u64) -> bool {
        let ix = offset as usize;
        self.0
            .get(&table)
            .and_then(|bits| bits.get(ix / 64))
            .is_some_and(|w| w >> (ix % 64) & 1 == 1)
    }

    /// Marks a row, answering whether it was not already marked.
    fn mark(&mut self, table: u32, offset: u64) -> bool {
        let bits = self.0.entry(table).or_default();
        let ix = offset as usize;
        if bits.len() <= ix / 64 {
            bits.resize(ix / 64 + 1, 0);
        }
        let was = bits[ix / 64] >> (ix % 64) & 1 == 1;
        bits[ix / 64] |= 1 << (ix % 64);
        !was
    }
}

/// The endpoints a variable-length step reaches, each once, for a
/// stage that keeps the set and throws the paths away. A node enters
/// the walk at the fewest hops that reach it and is never walked to
/// again, so the cost is the reachable subgraph rather than every path
/// through it.
///
/// The step's predicate is asked only about an edge that would reach
/// somewhere new. An edge into a node the walk already holds changes
/// nothing whether it passes or not, and on a graph where the walk
/// covers its component quickly that is nearly every edge, so the
/// reads the predicate needs fall away with them.
///
/// The start is marked before the walk moves, so the only way it comes
/// back is as somebody's neighbor, and that is where it is emitted:
/// a cycle through it is a path that ends where it began, which WALK
/// and TRAIL both allow and ACYCLIC does not.
fn reach_nodes(ctx: &mut StageCtx, spec: &VarSpec, table: u32, offset: u64) -> Result<Vec<Value>> {
    let mut far = Vec::new();
    let mut seen = Seen::default();
    seen.mark(table, offset);
    if spec.min == 0 && spec.to_tables.contains(&table) {
        far.push(Value::Node { table, offset });
    }
    // Whether the start has been dealt with: it is not one of the far
    // ends, or the mode forbids a path that returns to it, or the zero
    // hop above already put it out. Until then it is the one node the
    // walk is allowed to arrive at a second time.
    let mut start_again =
        spec.mode == PathMode::Acyclic || !spec.to_tables.contains(&table) || spec.min == 0;
    let mut frontier = vec![(table, offset)];
    let mut nbrs = Vec::new();
    let mut ords = Vec::new();
    let mut depth = 0u64;
    while !frontier.is_empty() && spec.max.is_none_or(|m| depth < m) {
        depth += 1;
        let mut next = Vec::new();
        for (t, o) in frontier {
            for step in spec.rels {
                let Some(direction) = spec.direction.resolve(step.undirected) else {
                    continue;
                };
                for backwards in [false, true] {
                    let walks = if backwards {
                        direction.walks_in() && t == step.to_table
                    } else {
                        direction.walks_out() && t == step.from_table
                    };
                    if !walks {
                        continue;
                    }
                    let far_table = if backwards {
                        step.from_table
                    } else {
                        step.to_table
                    };
                    ctx.graph.neighbors(step.id, o, backwards, &mut nbrs)?;
                    ctx.graph
                        .neighbor_ordinals(step.id, o, backwards, nbrs.len(), &mut ords)?;
                    for i in 0..nbrs.len() {
                        let other = nbrs[i];
                        let ord = ords[i];
                        let back = (far_table, other) == (table, offset);
                        if back && start_again {
                            continue;
                        }
                        if !back && seen.holds(far_table, other) {
                            continue;
                        }
                        let rel = Value::Rel {
                            table: step.id,
                            src: if backwards { other } else { o },
                            dst: if backwards { o } else { other },
                            ord,
                        };
                        if !edge_passes(ctx, spec.gate, &rel)? {
                            continue;
                        }
                        if back {
                            if depth >= spec.min {
                                start_again = true;
                                far.push(Value::Node {
                                    table: far_table,
                                    offset: other,
                                });
                            }
                            continue;
                        }
                        seen.mark(far_table, other);
                        next.push((far_table, other));
                        if depth >= spec.min && spec.to_tables.contains(&far_table) {
                            far.push(Value::Node {
                                table: far_table,
                                offset: other,
                            });
                        }
                    }
                }
            }
        }
        frontier = next;
    }
    Ok(far)
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

/// The shortest-path DAG as a walk that is going to run over it reads
/// it: the hops leaving one node that land a level further out, in the
/// order storage handed them back.
type Steps = BTreeMap<(u32, u64), Vec<(Value, u32, u64)>>;

/// The breadth-first prepass behind the SHORTEST selectors: minimum
/// hop counts from the start node within the hop window, the first
/// discovered parent hop per node, and nodes in discovery order.
/// Frontiers expand in discovery order and neighbors in storage order,
/// so levels, parents, and order are all deterministic. ANY SHORTEST
/// walks the parent chain for one canonical path per endpoint; ALL
/// SHORTEST reads the DAG below.
struct HopLevels {
    levels: BTreeMap<(u32, u64), u64>,
    parents: BTreeMap<(u32, u64), (Value, u32, u64)>,
    order: Vec<(u32, u64)>,
    /// The levelled graph itself, kept when the caller asks for it:
    /// every hop of it is a hop of some shortest path, and every
    /// shortest path is a walk down it. This is the PMR of docs/07 §5,
    /// the multiset of paths held as the graph that spells them rather
    /// than as the paths. Empty when the caller did not ask.
    steps: Steps,
    /// How many shortest paths reach each node, which is its
    /// predecessors' counts added up. Empty when the caller did not
    /// ask for the DAG, since this is read off it.
    paths: BTreeMap<(u32, u64), u64>,
}

/// `dag` asks for the levelled graph and the path counts over it. A
/// walk that is about to enumerate wants them: the alternative is
/// reading each node's neighbors again for every path prefix that
/// reaches it, and there is one prefix per path. A walk that only
/// wants one canonical path per endpoint does not, and pays neither.
fn hop_levels(
    ctx: &mut StageCtx,
    rels: &[RelStep],
    direction: RelDirection,
    gate: EdgeGate<'_>,
    max: Option<u64>,
    from: NodeAt,
    dag: bool,
) -> Result<HopLevels> {
    let mut bfs = HopLevels {
        levels: BTreeMap::new(),
        parents: BTreeMap::new(),
        order: vec![from],
        steps: BTreeMap::new(),
        paths: BTreeMap::new(),
    };
    bfs.levels.insert(from, 0);
    if dag {
        // The start is reached by the one path of no hops.
        bfs.paths.insert(from, 1);
    }
    let mut frontier = vec![from];
    let mut depth = 0u64;
    while !frontier.is_empty() && max.is_none_or(|m| depth < m) {
        depth += 1;
        let mut next = Vec::new();
        for (t, o) in frontier {
            for (rel_val, nt, no) in hop_edges(ctx, rels, direction, gate, t, o)? {
                let seen = bfs.levels.get(&(nt, no)).copied();
                if dag && seen.unwrap_or(depth) == depth {
                    // A hop of the levelled graph, whether or not it is
                    // the one that discovered the far node: every way
                    // in from the level behind is another path, and
                    // parallel edges between the same two nodes are
                    // different paths for the same reason.
                    let reaching = bfs.paths.get(&(t, o)).copied().unwrap_or(0);
                    let at = bfs.paths.entry((nt, no)).or_insert(0);
                    *at = at.saturating_add(reaching);
                    bfs.steps
                        .entry((t, o))
                        .or_default()
                        .push((rel_val.clone(), nt, no));
                }
                if seen.is_some() {
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

/// Whether one row of one table is a row a `DELETE` took away.
fn deleted(gone: &DeletedRows, table: u32, offset: u64) -> bool {
    gone.holds(table, offset)
}

/// The nodes a key expression names, one per candidate table it exists
/// in, the same resolution `IndexLookup` does. An empty answer is a
/// miss, not an error: a key of the wrong type, or one nobody carries,
/// names no node.
fn key_nodes(ctx: &mut StageCtx, key: &BoundExpr, tables: &[u32]) -> Result<Vec<(u32, u64)>> {
    let Value::Int(k) = eval(ctx, key)? else {
        return Ok(Vec::new());
    };
    let Ok(k) = u64::try_from(k) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for &table in tables {
        let Some(offset) = ctx.graph.lookup_key(table, k)? else {
            continue;
        };
        if offset < *ctx.counts.get(&table).unwrap_or(&0) && !deleted(ctx.gone, table, offset) {
            out.push((table, offset));
        }
    }
    Ok(out)
}

/// One side of the meeting search: the level every node was reached at
/// and the hop that reached it, plus the frontier the next round grows
/// from. `step` is the rel walked and the node on the other side of it,
/// which is the parent going forwards and the successor coming back.
/// A node the search stands on: its table and its row in that table.
type NodeAt = (u32, u64);

/// How a node was reached: the rel value walked and the node on the
/// other side of it. The source was reached by nothing, which is the
/// `None` that ends a path when it is read back.
type Step = Option<(Value, NodeAt)>;

struct HalfSearch {
    seen: BTreeMap<NodeAt, (u64, Step)>,
    frontier: Vec<(u32, u64)>,
    depth: u64,
}

impl HalfSearch {
    fn start(at: (u32, u64)) -> Self {
        let mut seen = BTreeMap::new();
        seen.insert(at, (0, None));
        HalfSearch {
            seen,
            frontier: vec![at],
            depth: 0,
        }
    }
}

/// The minimum-hop path between two bound nodes, as a PMR chain rooted
/// at `src` and ending at `dst`, or `None` when no path within the hop
/// window joins them.
///
/// Two frontiers grow towards each other and the smaller one moves each
/// round, so the search visits about two balls of radius d/2 where a
/// one-sided BFS visits one ball of radius d. On a graph where the
/// neighbourhood grows by a branching factor that is the difference
/// between b^d and 2*b^(d/2), which is the whole reason a single-pair
/// question is not answered by the same search that answers "how far is
/// everything from here".
///
/// A round stops growing a side when the other side is empty, when the
/// hop window is used up, or when the best meeting found so far is no
/// longer than the two depths together: every path still undiscovered
/// has to leave both explored balls, so it is at least one hop longer
/// than their radii add up to, and nothing shorter can turn up later.
fn pair_shortest(
    ctx: &mut StageCtx,
    rels: &[RelStep],
    direction: RelDirection,
    gate: EdgeGate<'_>,
    max: Option<u64>,
    src: (u32, u64),
    dst: (u32, u64),
) -> Result<Option<Arc<PathLink>>> {
    if src == dst {
        return Ok(Some(chain_root(src.0, src.1)));
    }
    let back = direction.flip();
    let mut fwd = HalfSearch::start(src);
    let mut bwd = HalfSearch::start(dst);
    let mut best: Option<(u64, (u32, u64))> = None;
    loop {
        if best.is_some_and(|(len, _)| len <= fwd.depth + bwd.depth) {
            break;
        }
        if fwd.frontier.is_empty() || bwd.frontier.is_empty() {
            break;
        }
        if max.is_some_and(|m| fwd.depth + bwd.depth + 1 > m) {
            break;
        }
        let forwards = fwd.frontier.len() <= bwd.frontier.len();
        let (near, far, dir) = if forwards {
            (&mut fwd, &bwd, direction)
        } else {
            (&mut bwd, &fwd, back)
        };
        let depth = near.depth + 1;
        let mut next = Vec::new();
        for &(t, o) in &std::mem::take(&mut near.frontier) {
            for (rel_val, nt, no) in hop_edges(ctx, rels, dir, gate, t, o)? {
                if near.seen.contains_key(&(nt, no)) {
                    continue;
                }
                near.seen.insert((nt, no), (depth, Some((rel_val, (t, o)))));
                next.push((nt, no));
                if let Some(&(other, _)) = far.seen.get(&(nt, no)) {
                    let len = depth + other;
                    if max.is_none_or(|m| len <= m) && best.is_none_or(|(b, _)| len < b) {
                        best = Some((len, (nt, no)));
                    }
                }
            }
        }
        near.frontier = next;
        near.depth = depth;
    }
    let Some((_, meet)) = best else {
        return Ok(None);
    };
    // Forwards from the source to the meeting node, then onwards to the
    // target through the hops the backward side recorded.
    let mut up = Vec::new();
    let mut cur = meet;
    while let Some((rel, prev)) = fwd.seen[&cur].1.clone() {
        up.push((rel, cur));
        cur = prev;
    }
    let mut chain = chain_root(src.0, src.1);
    for (rel, node) in up.into_iter().rev() {
        chain = link(chain, rel, node);
    }
    let mut cur = meet;
    while let Some((rel, next)) = bwd.seen[&cur].1.clone() {
        chain = link(chain, rel, next);
        cur = next;
    }
    Ok(Some(chain))
}

/// One more hop on a PMR chain.
fn link(prev: Arc<PathLink>, rel: Value, node: (u32, u64)) -> Arc<PathLink> {
    let hops = prev.hops + 1;
    Arc::new(PathLink {
        prev: Some(prev),
        rel: Some(rel),
        node: Value::Node {
            table: node.0,
            offset: node.1,
        },
        hops,
    })
}

/// Every probe against the accumulated edge sets of an ASP join that
/// answers, mirroring the direction and endpoint table checks of the
/// storage probe in `ExpandInto`.
/// A hit is the whole run the pair names, table and endpoints and
/// `(row, count)`, because a pattern with both endpoints bound matches
/// once per edge and a pair can hold several.
/// There can be more than one hit for one bound pair: a step naming no
/// direction may join through the forward edge and the backward one
/// both, and an alternation of rel types may join through any of them.
/// Each of those is a different edge, so each is emitted, which is the
/// count the expand this join replaces produces.
fn asp_hits(
    sets: &[EdgeSet],
    rels: &[RelStep],
    direction: RelDirection,
    (ft, fo): (u32, u64),
    (tt, to_off): (u32, u64),
    mut emit: impl FnMut(u32, u64, u64, (u64, u64)),
) {
    for (step, set) in rels.iter().zip(sets) {
        let Some(direction) = direction.resolve(step.undirected) else {
            continue;
        };
        if direction.walks_out()
            && ft == step.from_table
            && tt == step.to_table
            && let Some(&run) = set.get(&(fo, to_off))
        {
            emit(step.id, fo, to_off, run);
        }
        if direction.walks_in()
            && ft == step.to_table
            && tt == step.from_table
            && let Some(&run) = set.get(&(to_off, fo))
        {
            emit(step.id, to_off, fo, run);
        }
    }
}

/// Appends the rel values of one run, in load order: `count` copies of
/// the same pair, each carrying its own property row.
fn push_run_values(
    table: u32,
    src: u64,
    dst: u64,
    (base, count): (u64, u64),
    out: &mut Vec<Value>,
) {
    out.extend((0..count).map(|k| Value::Rel {
        table,
        src,
        dst,
        ord: base + k,
    }));
}

/// Whether one end of the intersection has stored lists to gallop for
/// this direction. A fixed direction reads one list and always does; an
/// undirected end reads both, which only lines up when the rel is self
/// referencing and the two lists hold the same node table.
fn usable_side(step: &RelStep, direction: RelDirection) -> bool {
    match direction.resolve(step.undirected) {
        Some(direction) => !direction.both_ways() || step.from_table == step.to_table,
        None => false,
    }
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
    match direction.resolve(step.undirected)? {
        RelDirection::Out if table == step.from_table => Some((&[false], step.to_table)),
        RelDirection::In if table == step.to_table => Some((&[true], step.from_table)),
        RelDirection::Any if step.from_table == step.to_table && table == step.from_table => {
            Some((&[false, true], step.to_table))
        }
        _ => None,
    }
}

/// The probe side of one intersection, cached per probe node.
///
/// `all` is what the leapfrog walks: the stored list for a fixed
/// direction, both lists merged for an undirected end, keeping every
/// copy, because a value a list holds twice is two edges rather than
/// two mentions of one. `ords` is the property row of each entry and
/// `fwd` says which stored list it came from, so an emitted rel can be
/// oriented the way the binary probe would have oriented it. Both are
/// filled only when the plan reads a rel.
///
/// `back` is scratch the undirected merge fills with the backward list;
/// it lives here so a cache miss reuses the allocation.
#[derive(Default)]
struct ProbeSide {
    key: (u32, u64),
    all: Vec<u64>,
    ords: Vec<u64>,
    fwd: Vec<bool>,
    out: Vec<u64>,
    out_ords: Vec<u64>,
    back: Vec<u64>,
    back_ords: Vec<u64>,
}

impl ProbeSide {
    /// A cache no probe node can match, so the first pull reads.
    fn empty() -> Self {
        ProbeSide {
            key: (u32::MAX, u64::MAX),
            ..ProbeSide::default()
        }
    }

    /// Merges the two stored lists of an undirected end into `all`,
    /// ascending, recording for each entry the list it came from.
    /// `ords` and `fwd` are filled only when `with_rows` is set, which
    /// is when something reads the rel.
    ///
    /// Every copy of both lists stays. A neighbor both lists hold is a
    /// pair joined in both directions, and those are two edges rather
    /// than two readings of one, so an undirected end matches through
    /// each of them. Copies inside one list are parallel edges and stay
    /// for the same reason. That is exactly what the expand this close
    /// replaces produces, which walks the out list and then the back one
    /// and keeps whatever each of them holds, so a fused close and an
    /// unfused one still count the same rows.
    fn merge(&mut self, with_rows: bool) {
        self.all.clear();
        self.all.reserve(self.out.len() + self.back.len());
        self.ords.clear();
        self.fwd.clear();
        let (mut i, mut j) = (0, 0);
        loop {
            let fwd = match (self.out.get(i), self.back.get(j)) {
                (Some(a), Some(b)) => a <= b,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            let (at, ord) = if fwd {
                let at = self.out[i];
                let ord = if with_rows { self.out_ords[i] } else { 0 };
                i += 1;
                (at, ord)
            } else {
                let at = self.back[j];
                let ord = if with_rows { self.back_ords[j] } else { 0 };
                j += 1;
                (at, ord)
            };
            self.all.push(at);
            if with_rows {
                self.ords.push(ord);
                self.fwd.push(fwd);
            }
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
/// Emits the index into each list of every matching pair.
///
/// A value both lists hold once is one hit, the common case and the one
/// the galloping is for. A value either list holds more than once is a
/// pair of edges that repeats, and every copy on the seed side joins
/// every copy on the probe side, because the two lists are two rel
/// steps of the pattern and each of them matched that many times. That
/// is the same row count the pair of expands this replaces produces, so
/// a fused close and an unfused one still agree.
fn leapfrog(seed: &[u64], probe: &[u64], mut emit: impl FnMut(usize, usize)) {
    let (mut si, mut pi) = (0, 0);
    while si < seed.len() && pi < probe.len() {
        let (sv, pv) = (seed[si], probe[pi]);
        if sv < pv {
            si = gallop(seed, pv, si);
        } else if pv < sv {
            pi = gallop(probe, sv, pi);
        } else {
            let se = si + seed[si..].partition_point(|&v| v == sv);
            let pe = pi + probe[pi..].partition_point(|&v| v == sv);
            for s in si..se {
                for p in pi..pe {
                    emit(s, p);
                }
            }
            si = se;
            pi = pe;
        }
    }
}

/// The pull entry point: a plain `step` normally, a timed and counted
/// one when the context carries stats. Recursive pulls inside `step`
/// come back through here, so every operator gets counted.
///
/// It is also where a cancellation is answered. Every operator's inner
/// loop pulls from the one below it through here, so a check on the way
/// in covers the whole pipeline at chunk granularity, from the top of
/// the stage down to a filter that has rejected a million rows without
/// producing one. A run nobody armed a handle for pays a load of a null
/// pointer for that, next to a chunk of work.
fn next(descs: &[OpDesc], ctx: &mut StageCtx, i: usize) -> Result<bool> {
    ctx.stop.check()?;
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
        OpDesc::ArgSource { arg, chunk } => {
            let rows = match ctx.params.get(*arg) {
                Some(Value::List(rows)) => rows,
                other => {
                    return Err(invalid(format!(
                        "the rows a write carried across it arrive as a list, got {other:?}"
                    )));
                }
            };
            let start = ctx.states[i].pos;
            if start >= rows.len() {
                return Ok(false);
            }
            let end = (start + VECTOR_SIZE).min(rows.len());
            let ncols = ctx.chunks[*chunk].cols.len();
            let mut cols = vec![Vec::with_capacity(end - start); ncols];
            for row in &rows[start..end] {
                let Value::List(row) = row else {
                    return Err(invalid(format!(
                        "each row a write carried across it is a list, got {row:?}"
                    )));
                };
                if row.len() != ncols {
                    return Err(invalid(format!(
                        "a row a write carried across it holds {} columns and the plan reads {ncols}",
                        row.len()
                    )));
                }
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
            let gone = ctx.gone;
            if let Some(m) = morsel {
                let st = &mut ctx.states[i];
                while vals.len() < VECTOR_SIZE && st.offset < m.end {
                    let offset = st.offset;
                    st.offset += 1;
                    // A deleted row keeps its offset, so the extent
                    // still covers it and the scan steps over it here
                    // rather than the table having grown shorter.
                    if deleted(gone, m.table, offset) {
                        continue;
                    }
                    vals.push(Value::Node {
                        table: m.table,
                        offset,
                    });
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
                    let offset = st.offset;
                    st.offset += 1;
                    if deleted(gone, table, offset) {
                        continue;
                    }
                    vals.push(Value::Node { table, offset });
                }
            }
            if vals.is_empty() {
                ctx.states[i].active = false;
                continue;
            }
            // What the caller watching this statement sees move. One
            // add per chunk of rows, on the operator every long query
            // ultimately pulls through.
            ctx.stop.read(vals.len() as u64);
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
                if offset < *ctx.counts.get(&table).unwrap_or(&0)
                    && !deleted(ctx.gone, table, offset)
                {
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
            let mut ords = Vec::new();
            for step in rels {
                let Some(direction) = direction.resolve(step.undirected) else {
                    continue;
                };
                if direction.walks_out() && table == step.from_table {
                    ctx.scratch.clear();
                    let mut scratch = std::mem::take(&mut ctx.scratch);
                    ctx.graph.neighbors(step.id, offset, false, &mut scratch)?;
                    // The rows only when something reads them: an
                    // expand that emits no rel value has no property to
                    // address and the second storage call would be for
                    // nothing.
                    if *emit_rels {
                        ctx.graph.neighbor_ordinals(
                            step.id,
                            offset,
                            false,
                            scratch.len(),
                            &mut ords,
                        )?;
                    }
                    for (i, &dst) in scratch.iter().enumerate() {
                        far.push(Value::Node {
                            table: step.to_table,
                            offset: dst,
                        });
                        if *emit_rels {
                            rel_vals.push(Value::Rel {
                                table: step.id,
                                src: offset,
                                dst,
                                ord: ords[i],
                            });
                        }
                    }
                    ctx.scratch = scratch;
                }
                if direction.walks_in() && table == step.to_table {
                    ctx.scratch.clear();
                    let mut scratch = std::mem::take(&mut ctx.scratch);
                    ctx.graph.neighbors(step.id, offset, true, &mut scratch)?;
                    if *emit_rels {
                        ctx.graph.neighbor_ordinals(
                            step.id,
                            offset,
                            true,
                            scratch.len(),
                            &mut ords,
                        )?;
                    }
                    for (i, &src) in scratch.iter().enumerate() {
                        far.push(Value::Node {
                            table: step.from_table,
                            offset: src,
                        });
                        if *emit_rels {
                            rel_vals.push(Value::Rel {
                                table: step.id,
                                src,
                                dst: offset,
                                ord: ords[i],
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
                        let Some(direction) = direction.resolve(step.undirected) else {
                            continue;
                        };
                        let sides = [
                            (direction.walks_out(), step.from_table, false),
                            (direction.walks_in(), step.to_table, true),
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
        // Count mode: nothing reads this chunk's values but one bare
        // count, so the walk counts the shortest paths off the levelled
        // graph and materializes none of them. A node's count is its
        // predecessors' counts added up, which the prepass does as it
        // levels, so this is one breadth-first pass where enumerating
        // is one walk per path and there can be exponentially many
        // paths between two nodes.
        OpDesc::VarExpand {
            from,
            direction,
            rels,
            min,
            max,
            selector,
            to_tables,
            edge_filter,
            edge_slot,
            counts: true,
            chunk,
            ..
        } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let v = value_of(ctx, *from)?;
            if matches!(v, Value::Null) {
                continue;
            }
            let (table, offset) = node_value(v, "var expand")?;
            let gate = match (edge_filter.as_ref(), *edge_slot) {
                (Some(expr), Some(slot)) => EdgeGate {
                    expr: Some(expr),
                    slot: Some(slot),
                },
                _ => EdgeGate::OPEN,
            };
            // ANY SHORTEST keeps one path per endpoint however many
            // there are, so its count is the endpoints and the DAG is
            // not worth building.
            let all = *selector == Some(Selector::AllShortest);
            let bfs = hop_levels(ctx, rels, *direction, gate, *max, (table, offset), all)?;
            let mut total = 0u64;
            for (at, level) in &bfs.levels {
                if *level < *min || !to_tables.contains(&at.0) {
                    continue;
                }
                let paths = match all {
                    true => bfs.paths.get(at).copied().unwrap_or(0),
                    false => 1,
                };
                total = total.saturating_add(paths);
            }
            if total == 0 {
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = usize::try_from(total).unwrap_or(usize::MAX);
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
            target,
            edge_filter,
            edge_slot,
            reach,
            counts: false,
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
            // A step with no predicate, or one whose predicate reads no
            // edge, walks through an open gate and pays nothing for it.
            let gate = match (edge_filter.as_ref(), *edge_slot) {
                (Some(expr), Some(slot)) => EdgeGate {
                    expr: Some(expr),
                    slot: Some(slot),
                },
                _ => EdgeGate::OPEN,
            };
            let mut far = Vec::new();
            let mut trails = Vec::new();
            if let Some(key) = target {
                // The far end is one node, so the search is a meeting
                // one and the answer is one row. A key that is not an
                // integer, or names nobody, matches nothing, which is
                // what the filter this absorbed would have said.
                let dsts = key_nodes(ctx, key, to_tables)?;
                for dst in dsts {
                    let Some(chain) =
                        pair_shortest(ctx, rels, *direction, gate, *max, (table, offset), dst)?
                    else {
                        continue;
                    };
                    if chain.hops < *min {
                        continue;
                    }
                    far.push(Value::Node {
                        table: dst.0,
                        offset: dst.1,
                    });
                    trails.push(Value::Chain(chain));
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
            }
            // The two searches that level the graph read a hop count as
            // the distance from the start, so they answer a pattern
            // whose lower bound is one hop and not one whose lower bound
            // is higher: a node's least length is not its least length
            // of at least three hops. A pattern like that goes to the
            // walk below, which reads lengths off the paths it built and
            // so can answer either. The binder leans on this split for
            // what it lets an unbounded WALK carry.
            let levelled = matches!(
                selector,
                Some(Selector::AnyShortest) | Some(Selector::AllShortest)
            ) && *min <= 1;
            if levelled && *selector == Some(Selector::AnyShortest) {
                // Chains build in discovery order, so a node's parent
                // chain always exists before its own and every
                // endpoint's path is one `Arc` clone.
                let bfs = hop_levels(ctx, rels, *direction, gate, *max, (table, offset), false)?;
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
            } else if levelled {
                // ALL SHORTEST. The level map cuts the walk down to the
                // shortest-path DAG, so every path it finds is one of
                // the shortest and the walk keeps the lot.
                let bfs = hop_levels(ctx, rels, *direction, gate, *max, (table, offset), true)?;
                let spec = VarSpec {
                    rels,
                    direction: *direction,
                    to_tables,
                    min: *min,
                    max: *max,
                    mode: *mode,
                    steps: Some(&bfs.steps),
                    gate,
                };
                let root = chain_root(table, offset);
                let mut out = Paths::new(Keep::All);
                enumerate_paths(ctx, &spec, table, offset, &root, &mut out)?;
                (far, trails) = out.finish();
            } else {
                let spec = VarSpec {
                    rels,
                    direction: *direction,
                    to_tables,
                    min: *min,
                    max: *max,
                    mode: *mode,
                    steps: None,
                    gate,
                };
                if *reach {
                    // The rel column stays empty: nothing reads the
                    // slot, which is what let the paths go. The rewrite
                    // that sets this asks for no selector, since how
                    // many paths reach a node is the question a selector
                    // answers and this walk deliberately forgets it.
                    far = reach_nodes(ctx, &spec, table, offset)?;
                } else {
                    let root = chain_root(table, offset);
                    let mut out = Paths::new(Keep::of(*selector));
                    enumerate_paths(ctx, &spec, table, offset, &root, &mut out)?;
                    (far, trails) = out.finish();
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
        } => {
            // A pair that repeats is that many edges and matches that
            // many times, so the run is walked here rather than handed
            // to a flatten: the operator is the flat one either side of
            // it expects, and one configuration in still gives one
            // configuration out per pull.
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
                let fv = value_of(ctx, *from)?;
                let tv = value_of(ctx, *to)?;
                if matches!(fv, Value::Null) || matches!(tv, Value::Null) {
                    continue;
                }
                let (ft, fo) = node_value(fv, "expand into")?;
                let (tt, to_off) = node_value(tv, "expand into")?;
                // Every run the bound pair joins through, not the first
                // one: a step naming no direction may hold a forward
                // edge and a backward one, and those are two edges, so
                // the pattern matches through each of them. An
                // alternation of rel types is the same story a step at a
                // time.
                let mut vals = Vec::new();
                for step in rels {
                    let Some(direction) = direction.resolve(step.undirected) else {
                        continue;
                    };
                    // The run rather than the probe: this needs the rows
                    // anyway to build the values, and a lookup that
                    // answers `None` for a missing edge answers the
                    // probe too.
                    if direction.walks_out()
                        && ft == step.from_table
                        && tt == step.to_table
                        && let Some(run) = ctx.graph.edge_run(step.id, fo, to_off)?
                    {
                        push_run_values(step.id, fo, to_off, run, &mut vals);
                    }
                    if direction.walks_in()
                        && ft == step.to_table
                        && tt == step.from_table
                        && let Some(run) = ctx.graph.edge_run(step.id, to_off, fo)?
                    {
                        push_run_values(step.id, to_off, fo, run, &mut vals);
                    }
                }
                if vals.is_empty() {
                    continue;
                }
                let c = &mut ctx.chunks[c];
                c.size = vals.len();
                c.cols[0] = vals;
                c.cur = Some(0);
                ctx.states[i] = OpState {
                    active: true,
                    ..OpState::default()
                };
                return Ok(true);
            }
        }
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
                    // The sweep is the load order, so the running count
                    // is each edge's row: the first copy of a pair
                    // records the row and every later one bumps the
                    // length of the run.
                    let mut ord = 0;
                    for src in 0..count {
                        graph.neighbors(step.id, src, false, scratch)?;
                        for &dst in scratch.iter() {
                            let run = set.entry((src, dst)).or_insert((ord, 0));
                            run.1 += 1;
                            ord += 1;
                        }
                    }
                    sets.push(set);
                }
                ctx.edge_sets.insert(i, sets);
            }
            match retain {
                // Flat probe, one configuration at a time like
                // `ExpandInto`, and like it walking the run a repeated
                // pair names rather than reporting one edge for it.
                None => {
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
                        let fv = value_of(ctx, *from)?;
                        let tv = value_of(ctx, *to)?;
                        if matches!(fv, Value::Null) || matches!(tv, Value::Null) {
                            continue;
                        }
                        let (ft, fo) = node_value(fv, "asp join")?;
                        let (tt, to_off) = node_value(tv, "asp join")?;
                        let sets = ctx.edge_sets.get(&i).expect("accumulated above");
                        let mut vals = Vec::new();
                        asp_hits(
                            sets,
                            rels,
                            *direction,
                            (ft, fo),
                            (tt, to_off),
                            |table, src, dst, run| {
                                push_run_values(table, src, dst, run, &mut vals);
                            },
                        );
                        if vals.is_empty() {
                            continue;
                        }
                        let c = &mut ctx.chunks[c];
                        c.size = vals.len();
                        c.cols[0] = vals;
                        c.cur = Some(0);
                        ctx.states[i] = OpState {
                            active: true,
                            ..OpState::default()
                        };
                        return Ok(true);
                    }
                }
                // The fused semijoin: probe the whole unflat neighbor
                // vector and keep the survivors in place, each one as
                // many times as the closing pair holds edges.
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
                    let mut times = Vec::with_capacity(source.size);
                    for pos in 0..source.size {
                        times.push(match &source.cols[col][pos] {
                            Value::Null => 0,
                            &Value::Node { table, offset } => {
                                let mut n = 0;
                                asp_hits(
                                    sets,
                                    rels,
                                    *direction,
                                    (ft, fo),
                                    (table, offset),
                                    |_, _, _, (_, count)| n += count as usize,
                                );
                                n
                            }
                            other => {
                                return Err(invalid(format!(
                                    "asp join expects a node, got {other:?}"
                                )));
                            }
                        });
                    }
                    if times.iter().all(|n| *n == 0) {
                        continue;
                    }
                    ctx.chunks[*f].repeat(&times);
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
            let mut seed_ords = Vec::new();
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
                            if *emit_rels {
                                let len = cache.all.len();
                                graph.neighbor_ordinals(
                                    probe_step.id,
                                    po,
                                    *prev,
                                    len,
                                    &mut cache.ords,
                                )?;
                                cache.fwd.clear();
                                cache.fwd.resize(len, !*prev);
                            }
                        }
                        _ => {
                            graph.neighbors(probe_step.id, po, false, &mut cache.out)?;
                            graph.neighbors(probe_step.id, po, true, &mut cache.back)?;
                            if *emit_rels {
                                let (fl, bl) = (cache.out.len(), cache.back.len());
                                graph.neighbor_ordinals(
                                    probe_step.id,
                                    po,
                                    false,
                                    fl,
                                    &mut cache.out_ords,
                                )?;
                                graph.neighbor_ordinals(
                                    probe_step.id,
                                    po,
                                    true,
                                    bl,
                                    &mut cache.back_ords,
                                )?;
                            }
                            cache.merge(*emit_rels);
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
                    if *emit_rels {
                        graph.neighbor_ordinals(
                            seed_step.id,
                            so,
                            srev,
                            scratch.len(),
                            &mut seed_ords,
                        )?;
                    }
                    leapfrog(scratch, &cache.all, |s, p| {
                        let v = scratch[s];
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
                                ord: seed_ords[s],
                            });
                            // Each entry of the merged list is one
                            // stored edge and the list says which of the
                            // two stored lists it came from, so the rel
                            // is oriented the way storage holds it.
                            let (ps, pd) = if cache.fwd[p] { (po, v) } else { (v, po) };
                            probe_rels.push(Value::Rel {
                                table: probe_step.id,
                                src: ps,
                                dst: pd,
                                ord: cache.ords[p],
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
                let keep = match vector_filter(ctx, expr, *chunk)? {
                    Some(keep) => keep,
                    None => {
                        let size = ctx.chunks[*chunk].size;
                        let mut keep = Vec::with_capacity(size);
                        for pos in 0..size {
                            ctx.chunks[*chunk].cur = Some(pos);
                            keep.push(truthy(&eval(ctx, expr)?));
                        }
                        ctx.chunks[*chunk].cur = None;
                        keep
                    }
                };
                if keep.iter().any(|k| *k) {
                    ctx.chunks[*chunk].retain(&keep);
                    return Ok(true);
                }
            },
        },
        OpDesc::Unwind {
            expr,
            chunk,
            ordinal,
        } => loop {
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            let items = match settle(eval(ctx, expr)?) {
                Value::List(items) => items,
                // GQ23. A binding table is the other thing a FOR may
                // run over, and a row of one is a record: the columns
                // are the field names, which is what the table already
                // says a row is. Nothing else changes, so the counter
                // of WITH ORDINALITY numbers the rows the same way it
                // numbers a list's elements.
                Value::BindingTable(table) => (0..table.rows().len())
                    .map(|row| table.record(row).unwrap_or(Value::Null))
                    .collect(),
                Value::Null => continue,
                other => {
                    return Err(invalid(format!(
                        "UNWIND expects a list or a binding table, got {other:?}"
                    )));
                }
            };
            if items.is_empty() {
                continue;
            }
            let c = &mut ctx.chunks[*chunk];
            c.size = items.len();
            if let Some(start) = ordinal {
                // The counter is the position in this list, so it runs
                // from the start over the elements this row's list has
                // and begins again at the next row's.
                c.cols[1] = (0..items.len())
                    .map(|i| Value::Int(start + i as i64))
                    .collect();
            }
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
            // A traversal source arrives as a user-facing id; resolve
            // it to the dense offset the kernel walks.
            if matches!(
                func,
                TableFunc::Bfs | TableFunc::Sssp | TableFunc::SsspWeighted
            ) {
                let name = func.name();
                let Some(Value::Int(key)) = vals.first() else {
                    return Err(invalid(format!(
                        "{name}'s source must be a node id, got {:?}",
                        vals.first()
                    )));
                };
                let offset = u64::try_from(*key)
                    .ok()
                    .and_then(|k| ctx.graph.lookup_key(*table, k).transpose())
                    .transpose()?
                    .ok_or_else(|| invalid(format!("{name} source {key} names no node")))?;
                vals[0] = Value::Int(offset as i64);
            }
            // The same resolution over a list, with one difference: a
            // sample drawn against a bigger graph names rows this table
            // does not have, and the kernel counts nothing for those
            // rather than failing the query over them.
            if matches!(func, TableFunc::Betweenness) {
                let Some(Value::List(items)) = vals.first() else {
                    return Err(invalid(format!(
                        "betweenness's sources must be a list of node ids, got {:?}",
                        vals.first()
                    )));
                };
                let mut offsets = Vec::with_capacity(items.len());
                for item in items {
                    let Value::Int(key) = item else {
                        return Err(invalid(format!(
                            "betweenness's sources must be node ids, got {item:?}"
                        )));
                    };
                    let found = u64::try_from(*key)
                        .ok()
                        .and_then(|k| ctx.graph.lookup_key(*table, k).transpose())
                        .transpose()?;
                    if let Some(offset) = found {
                        offsets.push(Value::Int(offset as i64));
                    }
                }
                vals[0] = Value::List(offsets);
            }
            let rows = ctx.graph.table_function(func.name(), *rel, &vals)?;
            // The kernel answers for every row of the table's extent,
            // deleted rows included, because it walks an adjacency
            // whose offsets do not move. Their answers are dropped
            // here, so a CALL yields what a MATCH over the same table
            // would.
            let offsets: Vec<u64> = (0..rows.len() as u64)
                .filter(|&offset| !deleted(ctx.gone, *table, offset))
                .collect();
            if offsets.is_empty() {
                return Ok(false);
            }
            let c = &mut ctx.chunks[*chunk];
            if rows.iter().any(|r| r.len() + 1 != c.cols.len()) {
                return Err(invalid(format!(
                    "{} returned a row with the wrong arity",
                    func.name()
                )));
            }
            c.size = offsets.len();
            c.cols[0] = offsets
                .iter()
                .map(|&offset| Value::Node {
                    table: *table,
                    offset,
                })
                .collect();
            for (j, col) in c.cols.iter_mut().enumerate().skip(1) {
                *col = offsets
                    .iter()
                    .map(|&offset| rows[offset as usize][j - 1].clone())
                    .collect();
            }
            c.cur = None;
            Ok(true)
        }
        OpDesc::BracketBegin => {
            if ctx.states[i].active {
                return Ok(false);
            }
            if !next(descs, ctx, i - 1)? {
                return Ok(false);
            }
            ctx.states[i].active = true;
            Ok(true)
        }
        OpDesc::BracketEnd {
            begin,
            chunks,
            kind,
            mark,
        } => loop {
            // Every kind but the optional is a question about the outer
            // row and one match answers it, so once it has answered,
            // the group is rearmed on the spot and the rest of its
            // matches are never drawn. An optional wants them all.
            if ctx.states[i].pos == 1 && *kind != BracketKind::Optional {
                rearm_bracket(ctx, *begin, i);
            }
            if next(descs, ctx, i - 1)? {
                // `pos` doubles as the emitted flag for the current
                // outer configuration.
                ctx.states[i].pos = 1;
                match kind {
                    BracketKind::Optional => return Ok(true),
                    BracketKind::Semi => {
                        // Nothing above reads the group's slots, so the
                        // match is worth exactly one row and its vectors
                        // collapse to one before they are counted.
                        null_chunks(ctx, chunks);
                        return Ok(true);
                    }
                    // The mark keeps the row either way and writes the
                    // answer down, so a hit is one row carrying true.
                    BracketKind::Mark { negated, .. } => {
                        null_chunks(ctx, chunks);
                        write_mark(ctx, *mark, !negated);
                        return Ok(true);
                    }
                    // A hit is what an anti bracket rejects, and the
                    // group's remaining matches would say the same.
                    BracketKind::Anti => continue,
                }
            }
            if !ctx.states[*begin].active {
                // The begin never yielded: the outer input is done.
                return Ok(false);
            }
            let missed = ctx.states[i].pos == 0;
            // Rearm the group for the next outer configuration.
            rearm_bracket(ctx, *begin, i);
            if missed && *kind != BracketKind::Semi {
                null_chunks(ctx, chunks);
                if let BracketKind::Mark { negated, .. } = kind {
                    write_mark(ctx, *mark, *negated);
                }
                return Ok(true);
            }
        },
    }
}

/// Puts a bracket's operators back where they started so the next pull
/// through the begin draws the next outer configuration.
fn rearm_bracket(ctx: &mut StageCtx, begin: usize, end: usize) {
    for s in &mut ctx.states[begin..end] {
        *s = OpState::default();
    }
    ctx.states[end].pos = 0;
}

/// Writes a mark bracket's answer into the column made for it, which is
/// what the predicate around the block reads instead of the block.
fn write_mark(ctx: &mut StageCtx, chunk: Option<usize>, hit: bool) {
    let Some(c) = chunk else {
        debug_assert!(false, "a mark bracket without a column to write into");
        return;
    };
    let chunk = &mut ctx.chunks[c];
    chunk.cols[0] = vec![Value::Bool(hit)];
    chunk.size = 1;
    chunk.cur = Some(0);
}

/// Binds a bracket's chunks to a single null row, which is both what an
/// outer row that matched nothing carries above and the collapse a semi
/// bracket does to a match nothing above is allowed to read.
fn null_chunks(ctx: &mut StageCtx, chunks: &[usize]) {
    for &c in chunks {
        let chunk = &mut ctx.chunks[c];
        for col in &mut chunk.cols {
            *col = vec![Value::Null];
        }
        chunk.size = 1;
        chunk.cur = Some(0);
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

/// What one of the six comparison operators answers for a pair of
/// settled values.
///
/// Its own function because two callers ask it: the expression
/// evaluator, one row at a time, and the vector filter, which reads a
/// column for a whole vector and then compares it. Two spellings of a
/// comparison would be two chances for `WHERE p.age = 30` to mean
/// something slightly different depending on which one ran.
fn compare(op: BinaryOp, l: &Value, r: &Value) -> Result<Value> {
    use BinaryOp::*;
    match op {
        Eq | Ne => Ok(match cmp_eq(l, r)? {
            Some(b) => Value::Bool(if op == Eq { b } else { !b }),
            None => Value::Null,
        }),
        Lt | Le | Gt | Ge => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                return Ok(Value::Null);
            }
            let ord = cmp_ord(l, r);
            Ok(Value::Bool(match op {
                Lt => ord == Ordering::Less,
                Le => ord != Ordering::Greater,
                Gt => ord == Ordering::Greater,
                Ge => ord != Ordering::Less,
                _ => unreachable!("the outer match is the six comparisons"),
            }))
        }
        other => Err(invalid(format!("{other:?} is not a comparison"))),
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
                ord: r1,
            },
            Value::Rel {
                table: t2,
                src: s2,
                dst: d2,
                ord: r2,
            },
            // Two copies of one pair are two edges, and the row is what
            // says which copy, so it is part of being the same edge.
        ) => Some(t1 == t2 && s1 == s2 && d1 == d2 && r1 == r2),
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
        // GV60. Two graph references are equal when they name the same
        // graph. The epoch each was taken at says when it was asked
        // for and not what it names, so it is no part of this, which
        // is what lets a handle a caller has held since yesterday
        // equal one a statement wrote this morning.
        (Value::Graph(x), Value::Graph(y)) => Some(x.id == y.id),
        // GV61, the other way round. A binding table reference is an
        // identity and not its rows, so two tables over the same rows
        // are two tables and comparing the rows would say otherwise.
        (Value::BindingTable(x), Value::BindingTable(y)) => Some(x.id() == y.id()),
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

/// The order `<`, `<=`, `>` and `>=` read, which is the order ORDER BY
/// reads and not a second one.
///
/// GA04 is universal comparison and zu reports it supported, so every
/// pair of values has an answer here: within a type the type's own
/// order, and between two types the precedence in [`value_order`],
/// which is ISO's IV010 and a choice zu documents rather than one it
/// derives. A number is therefore less than a string, and a date is
/// less than a duration, and neither comparison is the unknown truth
/// value: an engine whose `x < y` said unknown while its `ORDER BY x`
/// put x first would have two orders and be wrong in one of them.
///
/// The null does not reach this. A comparison with one is unknown,
/// which is a rule about null and not a gap in the order, and the
/// caller answers it before calling.
fn cmp_ord(a: &Value, b: &Value) -> Ordering {
    debug_assert!(
        !matches!(a, Value::Null) && !matches!(b, Value::Null),
        "the caller answers a comparison against null"
    );
    value_order(a, b)
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
        // Two instants of one shape subtract to the length between
        // them, which is a day-time duration because the calendar is
        // not involved in counting it. It is `DURATION_BETWEEN` with
        // the arguments the other way round, and it is the same code,
        // so the operator and the function cannot answer differently.
        (Sub, left, right) => match Temporal::between(*right, *left, DurationKind::DayTime) {
            Some(length) => Ok(Value::Temporal(length)),
            None => refuse(),
        },
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

/// A whole vector's worth of a filter, when the filter is one property
/// of one bound node compared with something the vector does not
/// depend on, and `None` when it is anything else.
///
/// `WHERE p.age = 30` under a scan is that shape, and so is most of
/// what a workload filters on. Read a row at a time it costs a name
/// resolved, a table's reader found and a column's chunk located per
/// row, all of it the same answer every time, and on a hundred
/// thousand row table that is the statement. Read this way it is one
/// column read for the vector and a comparison per row.
///
/// Every bail here is a correctness one rather than a judgement about
/// what is worth batching. A row a DELETE took away has to be the
/// error the row at a time path raises, an overlay value shadows the
/// vector, and a column of nodes of two tables is two columns.
fn vector_filter(ctx: &mut StageCtx, expr: &BoundExpr, chunk: usize) -> Result<Option<Vec<bool>>> {
    let BoundExpr::Binary { op, lhs, rhs } = expr else {
        return Ok(None);
    };
    if !matches!(
        op,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
    ) {
        return Ok(None);
    }
    // The property can be on either side, and swapping the sides of an
    // ordering swaps the operator with them.
    let (prop, other, op) = match lhs.as_ref() {
        BoundExpr::Property { .. } => (lhs.as_ref(), rhs.as_ref(), *op),
        _ => (rhs.as_ref(), lhs.as_ref(), flip(*op)),
    };
    let BoundExpr::Property { base, key } = prop else {
        return Ok(None);
    };
    let BoundExpr::Var(slot) = base.as_ref() else {
        return Ok(None);
    };
    if ctx.overlay.contains_key(slot) {
        return Ok(None);
    }
    let Some(&(c, col)) = ctx.slot_loc.get(slot) else {
        return Ok(None);
    };
    if c != chunk {
        return Ok(None);
    }
    // The other side is evaluated once for the vector, so it must not
    // read anything the vector holds.
    let mut slots = BTreeSet::new();
    crate::binder::expr_slots(other, &mut slots);
    if slots
        .iter()
        .any(|s| ctx.slot_loc.get(s).is_some_and(|&(cc, _)| cc == chunk))
    {
        return Ok(None);
    }

    let size = ctx.chunks[chunk].size;
    let mut rows = Vec::with_capacity(size);
    let mut table = None;
    for pos in 0..size {
        let Value::Node { table: t, offset } = ctx.chunks[chunk].cols[col][pos] else {
            return Ok(None);
        };
        if *table.get_or_insert(t) != t {
            return Ok(None);
        }
        if deleted(ctx.gone, t, offset) {
            return Ok(None);
        }
        rows.push(offset);
    }
    let Some(table) = table else {
        return Ok(Some(Vec::new()));
    };

    let other = settle(eval(ctx, other)?);
    let mut values = Vec::with_capacity(size);
    ctx.graph.properties(table, &rows, key, &mut values)?;
    let mut keep = Vec::with_capacity(size);
    for value in values {
        keep.push(truthy(&compare(op, &settle(value), &other)?));
    }
    Ok(Some(keep))
}

/// The operator that means the same thing with its sides swapped.
fn flip(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::Le => BinaryOp::Ge,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::Ge => BinaryOp::Le,
        other => other,
    }
}

/// The value of one value query expression on the row the evaluator is
/// standing on (GQ18).
///
/// One that decorrelated was answered before the plan started and is
/// read straight out. One that did not is run here, on this row, with
/// the values it captured written into the parameters it reads them
/// at. That is the cost the statement was warned about: the query
/// inside runs once for each row the query outside has.
fn scalar_value(ctx: &mut StageCtx, ix: usize) -> Result<Value> {
    let query = &ctx.scalars.queries[ix];
    if query.captures.is_empty() {
        return Ok(ctx.scalars.once[ix].clone());
    }
    let mut params = ctx.params.to_vec();
    for capture in &query.captures {
        params[capture.param] = value_of(ctx, capture.slot)?;
    }
    let schema = ctx.scalars.schema;
    let options = ctx.scalars.options;
    let plan = ctx.scalars.plans[ix]
        .as_ref()
        .expect("a query that captures was planned as one that runs per row");
    answer(plan, query, schema, &mut *ctx.graph, &params, options)
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
        // ISO 20.27, the instant behind the five datetime value
        // functions. It was read once before the first stage and is
        // handed out here as many times as the rows ask for it, so a
        // scan of ten million rows makes no system call at all and
        // every row of it agrees about what time the statement ran.
        BoundExpr::Clock => Ok(Value::Temporal(
            ctx.scalars.options.clock.unwrap_or_default().instant(),
        )),
        // GE01. The binder already asked the catalog which graph this
        // is, so the row's work is a clone of the handle and nothing
        // else.
        BoundExpr::Graph(handle) => Ok(Value::Graph(handle.clone())),
        // GE03. Each name is worked out once and put in the overlay
        // for as long as the body is being read, which is where a
        // step's edge variable already lives: a slot that is in no
        // chunk and that `value_of` finds before it looks in one. The
        // binder made these slots for this expression alone, so there
        // is nothing under them to shadow and nothing to put back.
        BoundExpr::Let { values, body } => {
            let mut failed = None;
            for (slot, value) in values {
                match eval(ctx, value) {
                    Ok(v) => {
                        ctx.overlay.insert(*slot, v);
                    }
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            let answer = match failed {
                Some(e) => Err(e),
                None => eval(ctx, body),
            };
            for (slot, _) in values {
                ctx.overlay.remove(slot);
            }
            answer
        }
        BoundExpr::Scalar { ix, .. } => scalar_value(ctx, *ix),
        BoundExpr::Var(slot) => value_of(ctx, *slot),
        BoundExpr::HasLabels { slot, test } => match value_of(ctx, *slot)? {
            Value::Node { table, offset } => {
                let word = ctx.graph.labels(table, offset)?;
                Ok(Value::Bool(test.matches(word)))
            }
            // An unmatched optional row has no labels to test, and a
            // null predicate is what keeps its row.
            Value::Null => Ok(Value::Null),
            other => Err(invalid(format!("label test on {other:?}, expected a node"))),
        },
        // G110. Every edge of a table has a direction or none of them
        // does, so the table is the whole of the answer.
        BoundExpr::IsDirected {
            expr,
            undirected,
            negated,
        } => match eval(ctx, expr)? {
            Value::Rel { table, .. } => Ok(Value::Bool(
                undirected.binary_search(&table).is_err() != *negated,
            )),
            Value::Null => Ok(Value::Null),
            other => Err(invalid(format!(
                "IS DIRECTED asks about an edge, got {other:?}"
            ))),
        },
        // G111. A node is asked with the bit test the pattern would
        // have used, an edge by the table it is in.
        BoundExpr::IsLabeled {
            expr,
            node,
            rels,
            negated,
        } => match eval(ctx, expr)? {
            Value::Node { table, offset } => {
                let word = ctx.graph.labels(table, offset)?;
                Ok(Value::Bool(node.matches(word) != *negated))
            }
            Value::Rel { table, .. } => {
                Ok(Value::Bool(rels.binary_search(&table).is_ok() != *negated))
            }
            Value::Null => Ok(Value::Null),
            other => Err(invalid(format!(
                "IS LABELED asks about a node or an edge, got {other:?}"
            ))),
        },
        // G112. The edge already holds both of its ends, so this is a
        // comparison and never a lookup.
        BoundExpr::IsEndpoint {
            node,
            rel,
            end,
            ends,
            negated,
        } => {
            let (node, rel) = (eval(ctx, node)?, eval(ctx, rel)?);
            match (node, rel) {
                (
                    Value::Node { table, offset },
                    Value::Rel {
                        table: rel_table,
                        src,
                        dst,
                        ..
                    },
                ) => {
                    let Ok(at) = ends.binary_search_by_key(&rel_table, |&(id, _, _)| id) else {
                        return Err(invalid(format!("edge from unknown table {rel_table}")));
                    };
                    let (_, from, to) = ends[at];
                    let (side, row) = match end {
                        EdgeEnd::Source => (from, src),
                        EdgeEnd::Destination => (to, dst),
                    };
                    Ok(Value::Bool((side == table && row == offset) != *negated))
                }
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (node, rel) => Err(invalid(format!(
                    "IS {} OF relates a node to an edge, got {node:?} and {rel:?}",
                    end.text()
                ))),
            }
        }
        // G115. Whether the element's table carries the property, which
        // is why a stored null answers true: the question is about the
        // element and not about the value.
        BoundExpr::PropertyExists { expr, key } => match eval(ctx, expr)? {
            Value::Node { table, offset } => {
                Ok(Value::Bool(ctx.graph.has_property(table, offset, key)?))
            }
            Value::Rel { table, ord, .. } => {
                Ok(Value::Bool(ctx.graph.has_rel_property(table, ord, key)?))
            }
            Value::Null => Ok(Value::Null),
            other => Err(invalid(format!(
                "PROPERTY_EXISTS asks about a node or an edge, got {other:?}"
            ))),
        },
        BoundExpr::Property { base, key } => match eval(ctx, base)? {
            // A delete leaves the element bound, so a clause after one
            // can hold a reference to a row that is no longer there.
            // Reading it is 22G11 rather than the value the row used to
            // hold: a scan never hands one out, so a node that is in
            // the deleted set here arrived across a write of this
            // statement and nothing else.
            Value::Node { table, offset } if deleted(ctx.gone, table, offset) => Err(gql(
                codes::C22G11,
                format!(
                    "'{key}' is being read off an element that a DELETE in this statement took away, row {offset} of table {table}"
                ),
            )),
            Value::Node { table, offset } => ctx.graph.property(table, offset, key),
            // A field the record does not have is null rather than an
            // error, which is what a property a node does not have
            // already answers. A record whose shape a query can rely
            // on is one the query declared, and that is what a cast to
            // a record type is for.
            ref record @ Value::Record(_) => Ok(record.field(key).cloned().unwrap_or(Value::Null)),
            // An edge with no stored row reads null here rather than in
            // every engine, so an engine's `rel_property` only ever
            // sees a row it holds.
            Value::Rel { ord, .. } if ord == Value::NO_REL_ROW => Ok(Value::Null),
            Value::Rel { table, ord, .. } => ctx.graph.rel_property(table, ord, key),
            Value::Null => Ok(Value::Null),
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
                Eq | Ne | Lt | Le | Gt | Ge => {
                    let l = settle(eval(ctx, lhs)?);
                    let r = settle(eval(ctx, rhs)?);
                    compare(*op, &l, &r)
                }
                Add | Sub | Mul | Div | Mod => {
                    let l = settle(eval(ctx, lhs)?);
                    let r = settle(eval(ctx, rhs)?);
                    arith(*op, l, r)
                }
                // ISO 20.23. Strings and nothing else: a number here is
                // refused rather than written out as its digits, since
                // a query that meant the digits says so with a CAST and
                // one that meant an addition wrote a plus.
                Concat => {
                    let l = settle(eval(ctx, lhs)?);
                    let r = settle(eval(ctx, rhs)?);
                    match (l, r) {
                        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                        (Value::Str(a), Value::Str(b)) => Ok(Value::Str(a + &b)),
                        (a, b) => Err(invalid(format!("|| joins strings, got {a:?} and {b:?}"))),
                    }
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
        // The binder settled which kernel this call runs, so the row
        // spends nothing on deciding what function it is looking at:
        // the arguments are evaluated in written order and the kernel
        // is called through the pointer the registry holds. An
        // aggregate has no kernel, because what answers one is the
        // accumulator the grouping keeps, and reaching one here is a
        // projection that was compiled wrong.
        BoundExpr::Call {
            func,
            sig,
            star,
            args,
            ..
        } => {
            let kernel = crate::functions::row(*sig)
                .and_then(|sig| sig.kernel)
                .filter(|_| !*star)
                .ok_or_else(|| {
                    invalid("aggregate call outside a projection, this is a bug".into())
                })?;
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(eval(ctx, arg)?);
            }
            kernel(*func, &values)
        }
        // GE09. An aggregate over a group variable, which folds what one
        // row bound rather than what the rows held. The accumulator is
        // the one the grouped aggregates use, so a null element is
        // dropped here the way a null row is dropped there, and the
        // elements are read one at a time rather than gathered into a
        // list to walk down.
        BoundExpr::Fold {
            func,
            distinct,
            args,
        } => {
            let mut state = AggState::new(&AggSpec {
                func: *func,
                distinct: *distinct,
                star: false,
                arg: None,
                arg_chunk: None,
                fraction: None,
            });
            for arg in args {
                state.add(eval(ctx, arg)?, 1)?;
            }
            // The binder sends only the one argument set functions
            // down this path, so there is no fraction to hand over.
            state.finalize(None)
        }
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
        // GE01. The branches are asked in the order they were written
        // and the walk stops at the first that says yes, so a branch
        // below one that matched is never evaluated and neither is a
        // THEN whose WHEN said no. That is what lets a CASE guard a
        // division: the branch that would divide by zero is the branch
        // the walk never reaches.
        BoundExpr::Case {
            subject,
            branches,
            otherwise,
        } => {
            let subject = match subject {
                Some(expr) => Some(settle(eval(ctx, expr)?)),
                None => None,
            };
            for (when, then) in branches {
                let hit = match &subject {
                    // The simple form compares, and a null on either
                    // side is not a match, the way `=` answers null
                    // rather than true.
                    Some(value) => cmp_eq(value, &settle(eval(ctx, when)?))? == Some(true),
                    None => truth(&eval(ctx, when)?)? == Some(true),
                };
                if hit {
                    return eval(ctx, then);
                }
            }
            match otherwise {
                Some(expr) => eval(ctx, expr),
                None => Ok(Value::Null),
            }
        }
        BoundExpr::Coalesce(args) => {
            for arg in args {
                match settle(eval(ctx, arg)?) {
                    Value::Null => {}
                    value => return Ok(value),
                }
            }
            Ok(Value::Null)
        }
        BoundExpr::NullIf { value, compared } => {
            let value = settle(eval(ctx, value)?);
            let compared = settle(eval(ctx, compared)?);
            Ok(match cmp_eq(&value, &compared)? {
                Some(true) => Value::Null,
                _ => value,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Acc {
    Count(i64),
    Sum(Option<Value>),
    Avg {
        sum: f64,
        n: i64,
    },
    Min(Option<Value>),
    Max(Option<Value>),
    Collect(Vec<Value>),
    /// GF10's two standard deviations, kept as a running count, mean
    /// and sum of squared deviations from that mean.
    ///
    /// This is Welford's, and it is here rather than the sum of squares
    /// because the sum of squares is what a one pass deviation is
    /// usually written as and it is wrong: over values that are large
    /// and close together the sum of squares and the square of the sum
    /// are two big numbers whose difference is small, and subtracting
    /// them loses the answer's leading digits. Welford's subtracts
    /// nothing large from anything large, and it costs one more
    /// multiply per row.
    ///
    /// The fields are the ones the merge needs too, so a morsel partial
    /// is this and a group is these folded together.
    Spread {
        /// Which of the two divisors the answer takes, which is the
        /// whole of what the two functions differ by.
        which: Deviation,
        /// How many values arrived, counting multiplicities.
        n: f64,
        /// The mean of them so far.
        mean: f64,
        /// The sum of squared deviations from that mean.
        m2: f64,
    },
    /// GF11's two percentiles, kept as the values that arrived and how
    /// many rows each of them stood for.
    ///
    /// A percentile is holistic: no fixed amount of state answers it,
    /// because the answer can be any of the values and which one it is
    /// is not known until the last row has arrived. So the values are
    /// kept, which is what `COLLECT_LIST` does and costs what it costs.
    /// They are kept against their multiplicities rather than expanded,
    /// so an unflat chunk standing for a thousand rows is one entry
    /// weighing a thousand, and they are kept unsorted, the sort being
    /// one pass at the end rather than an insertion per row.
    Quantile {
        /// Whether a fraction that lands between two values is
        /// interpolated or answered with the value above it.
        which: Percentile,
        /// The values and their weights, in arrival order.
        values: Vec<(Value, i64)>,
    },
}

/// The percentile of a group: what arrived, weighted, and the fraction
/// of the way through it to answer.
///
/// The two functions read the same values and part on one question,
/// what to say when the fraction falls between two of them. The
/// discrete one answers the first value whose running share of the
/// group reaches the fraction, so its answer is a value that was there.
/// The continuous one draws a line through the values and answers the
/// point on it, so its answer usually was not.
fn percentile(which: Percentile, mut values: Vec<(Value, i64)>, fraction: &Value) -> Result<Value> {
    let name = match which {
        Percentile::Continuous => "percentile_cont",
        Percentile::Discrete => "percentile_disc",
    };
    let p = match fraction {
        // A null fraction is a question with nothing asked, which is
        // null, the same answer a null argument gets everywhere else.
        Value::Null => return Ok(Value::Null),
        other => as_f64(other).ok_or_else(|| {
            gql(
                codes::C22G03,
                format!("{name}() needs a number for its fraction, got {other:?}"),
            )
        })?,
    };
    if !(0.0..=1.0).contains(&p) {
        return Err(gql(
            codes::C22003,
            format!("{name}() needs a fraction from 0 to 1, got {p}"),
        ));
    }
    let n: i64 = values.iter().map(|(_, weight)| *weight).sum();
    if n <= 0 {
        return Ok(Value::Null);
    }
    // Every value that got in here is a number, which `apply` checked,
    // so the order is the numeric one and a value that is somehow not a
    // number sorts to the end rather than making the comparison
    // inconsistent and the sort panic.
    let key = |value: &Value| as_f64(value).unwrap_or(f64::NAN);
    values.sort_by(|left, right| key(&left.0).total_cmp(&key(&right.0)));
    if let Percentile::Discrete = which {
        let mut seen = 0_i64;
        for (value, weight) in &values {
            seen += weight;
            if seen as f64 / n as f64 >= p {
                return Ok(value.clone());
            }
        }
        // Unreachable while the fraction is at most one, since the last
        // share is exactly one, but the answer if the rounding ever
        // says otherwise is the last value and not a null.
        return Ok(values
            .last()
            .map_or(Value::Null, |(value, _)| value.clone()));
    }
    // The value at a place in the group counted as though the weights
    // had been expanded, which is what the standard's rule is written
    // over. Two walks rather than one because the two places are
    // adjacent and the second is usually in the same entry.
    let at = |place: i64| -> f64 {
        let mut seen = 0_i64;
        for (value, weight) in &values {
            seen += weight;
            if place < seen {
                return key(value);
            }
        }
        values.last().map_or(f64::NAN, |(value, _)| key(value))
    };
    let exact = p * (n - 1) as f64;
    let below = exact.floor();
    let above = exact.ceil();
    let low = at(below as i64);
    if below == above {
        return Ok(Value::Float(low));
    }
    // The standard writes this as the two ends weighted by their
    // distances and summed. It is written here as the low end plus the
    // step, which is the same number and does not subtract two nearly
    // equal products when the two ends are nearly equal.
    Ok(Value::Float(
        low + (at(above as i64) - low) * (exact - below),
    ))
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
            Func::Stddev(which) => Acc::Spread {
                which,
                n: 0.0,
                mean: 0.0,
                m2: 0.0,
            },
            Func::Percentile(which) => Acc::Quantile {
                which,
                values: Vec::new(),
            },
            Func::Id
            | Func::ElementId
            | Func::Size
            | Func::Cardinality
            | Func::PathLength
            | Func::Elements
            | Func::AllDifferent
            | Func::Same
            | Func::CharLength
            | Func::OctetLength
            | Func::Upper
            | Func::Lower
            | Func::Trim(_)
            | Func::Cut(_)
            | Func::Temporal(_)
            | Func::DurationBetween(_)
            | Func::Normalize(_)
            | Func::IsNormalized(_)
            | Func::Math(_) => {
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
            // A multiplicity is `mult` copies of one value, so this is
            // the merge below with a second set of `mult` values whose
            // mean is the value and whose spread is nought. Written out
            // rather than looped, since a chunk may stand for a great
            // many rows and they all fold in the same step.
            Acc::Spread { n, mean, m2, .. } => {
                let x = as_f64(&v).ok_or_else(|| {
                    gql(codes::C22G03, format!("stddev() needs numbers, got {v:?}"))
                })?;
                let w = mult as f64;
                let total = *n + w;
                let delta = x - *mean;
                *mean += delta * w / total;
                *m2 += delta * delta * w * *n / total;
                *n = total;
            }
            // The type is checked here rather than at the end so that a
            // string in the column is refused where it is, and so that
            // the sort below can order what it holds by number.
            Acc::Quantile { values, .. } => {
                if as_f64(&v).is_none() {
                    return Err(gql(
                        codes::C22G03,
                        format!("percentile() needs numbers, got {v:?}"),
                    ));
                }
                values.push((v, mult));
            }
        }
        Ok(())
    }

    /// The group's answer. `fraction` is the second argument of a
    /// binary set function, already evaluated, and is `None` for every
    /// other set function because they have no second argument.
    fn finalize(mut self, fraction: Option<Value>) -> Result<Value> {
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
            // No values is no deviation, and one value is no sample of
            // one, so both answer null rather than nought. A population
            // of one has a deviation and it is nought, the one value
            // being the whole of the population and sitting on its own
            // mean.
            Acc::Spread { which, n, m2, .. } => match which {
                _ if n < 1.0 => Value::Null,
                Deviation::Sample if n < 2.0 => Value::Null,
                Deviation::Sample => Value::Float((m2 / (n - 1.0)).sqrt()),
                Deviation::Population => Value::Float((m2 / n).sqrt()),
            },
            Acc::Quantile { which, values } => {
                let Some(fraction) = fraction else {
                    return Err(invalid(
                        "a percentile needs the fraction to answer as its second argument".into(),
                    ));
                };
                percentile(which, values, &fraction)?
            }
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
            // Chan's merge, which is what makes the deviation one pass
            // and parallel at once: two partials fold into one without
            // either of them keeping the values it saw.
            (
                Acc::Spread { n, mean, m2, .. },
                Acc::Spread {
                    n: n2,
                    mean: mean2,
                    m2: m22,
                    ..
                },
            ) => {
                let total = *n + n2;
                if total > 0.0 {
                    let delta = mean2 - *mean;
                    *mean += delta * n2 / total;
                    *m2 += m22 + delta * delta * *n * n2 / total;
                    *n = total;
                }
            }
            // Nothing to fold: the values are sorted once, at the end,
            // over whatever the morsels between them gathered.
            (Acc::Quantile { values, .. }, Acc::Quantile { values: more, .. }) => {
                values.extend(more);
            }
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

/// Two values compared the way one ORDER BY key reads them.
///
/// A null sits outside the direction. `NULLS FIRST` is the head of the
/// result and not the small end of the order, so reversing a descending
/// key would put the nulls at the wrong end; the null placement is read
/// off the key and the direction covers only what is left.
fn sort_cmp(key: &SortKey<BoundExpr>, a: &OrdValue, b: &OrdValue) -> Ordering {
    let null = |v: &OrdValue| matches!(v.0, Value::Null);
    match (null(a), null(b)) {
        (true, true) => return Ordering::Equal,
        (true, false) => {
            return if key.nulls_first() {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        (false, true) => {
            return if key.nulls_first() {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        (false, false) => {}
    }
    let ord = a.cmp(b);
    if key.ascending { ord } else { ord.reverse() }
}

fn sort_exprs(sink: &SinkDef) -> &[SortKey<BoundExpr>] {
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
    for SortKey { expr, .. } in sort_exprs(sink) {
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
    let mut specs = sink.aggs.iter();
    let mut values = Vec::with_capacity(sink.items.len());
    for item in &sink.items {
        let v = if item.aggregate {
            let state = sit.next().expect("one state per aggregate item");
            let spec = specs.next().expect("one spec per aggregate item");
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
            // The fraction of a percentile, which the compiler made
            // sure reads no slot, so it is the same value wherever it
            // is asked for and here is the one place it is needed.
            let fraction = match &spec.fraction {
                Some(expr) => Some(eval(ctx, expr)?),
                None => None,
            };
            state.finalize(fraction)?
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
    // GF20. What is left in the two iterators is the aggregates a sort
    // key asked for and no column carries. They finalize like the rest
    // and go into the slot each one was given, which is what the key
    // reads a line below, so a sort by a count costs the sort an
    // accumulator and the answer nothing.
    for slot in &sink.order_slots {
        let state = sit.next().expect("one state per hidden sort aggregate");
        let spec = specs.next().expect("one spec per hidden sort aggregate");
        if state.nulls_eliminated {
            ctx.notices.push(DiagnosticRecord::new(
                codes::C01G11,
                "a set function ignored one or more null arguments",
            ));
        }
        let fraction = match &spec.fraction {
            Some(expr) => Some(eval(ctx, expr)?),
            None => None,
        };
        let v = state.finalize(fraction)?;
        ctx.overlay.insert(*slot, v);
    }
    let mut keys = Vec::new();
    for SortKey { expr, .. } in sort_exprs(sink) {
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
                rows.sort_by(|a, b| {
                    for (ix, key) in keys.iter().enumerate() {
                        let ord = sort_cmp(key, &a.keys[ix], &b.keys[ix]);
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
// Streaming
// ---------------------------------------------------------------------------

/// The state a streamed statement carries across batches: where the
/// rows go, how many go at a time, and what SKIP and LIMIT have spent
/// so far.
///
/// SKIP and LIMIT live here rather than in [`apply_post`] because they
/// are prefix operations on the whole answer and a batch is a piece of
/// it. Spending them across batches is what makes streaming give the
/// same rows as buffering, one batch at a time.
///
/// Both executors hand rows over through this one object, so a caller
/// cannot tell which of them ran its statement from the batches it
/// sees, which is the only way the two engines can stay one API.
pub struct Streaming<'s> {
    sink: &'s mut dyn FnMut(Batch<'_>) -> Result<Flow>,
    columns: &'s [String],
    batch: usize,
    skip: usize,
    limit: Option<usize>,
    rows: u64,
    stopped: bool,
    /// Whether the rows were handed over as they were made. A statement
    /// that had to see every row before it could give one arrives whole
    /// and is handed over afterwards, and this is what tells them apart.
    streamed: bool,
}

impl<'s> Streaming<'s> {
    /// A handoff of `batch` rows at a time into `sink`, under the column
    /// names the statement projected.
    pub fn new(
        sink: &'s mut dyn FnMut(Batch<'_>) -> Result<Flow>,
        columns: &'s [String],
        batch: usize,
    ) -> Streaming<'s> {
        Streaming {
            sink,
            columns,
            batch: batch.max(1),
            skip: 0,
            limit: None,
            rows: 0,
            stopped: false,
            streamed: false,
        }
    }

    /// The window the statement asked for, once, before any of it is
    /// spent. An executor that has already evaluated SKIP and LIMIT to
    /// counts hands them over here instead of applying them per batch,
    /// which would make every batch its own answer.
    pub fn window(&mut self, skip: usize, limit: Option<usize>) {
        self.skip = skip;
        self.limit = limit;
    }

    /// Rows the caller asked to be handed at a time, which is also how
    /// far ahead of the consumer an executor should let itself run.
    pub fn batch(&self) -> usize {
        self.batch
    }

    /// Whether there is any row left to want: false once LIMIT is spent
    /// or the caller has stopped, which is what ends a scan early.
    pub fn wants_more(&self) -> bool {
        !self.stopped && self.limit != Some(0)
    }

    /// Records that these rows were made and handed over rather than
    /// collected first.
    pub fn made_them(&mut self) {
        self.streamed = true;
    }

    /// Spends the window on one piece of the answer and hands what
    /// survives over, in batches of the size the caller asked for.
    /// Answers [`Streaming::wants_more`] afterwards, so a driver stops
    /// on `false` without asking twice.
    pub fn feed(&mut self, mut rows: Vec<Vec<Value>>) -> Result<bool> {
        let n = self.skip.min(rows.len());
        rows.drain(..n);
        self.skip -= n;
        if let Some(left) = &mut self.limit {
            let n = (*left).min(rows.len());
            rows.truncate(n);
            *left -= n;
        }
        for chunk in rows.chunks(self.batch) {
            if matches!(self.hand_over(chunk)?, Flow::Stop) {
                break;
            }
        }
        Ok(self.wants_more())
    }

    /// Hands one batch over, or nothing when the batch is empty: a
    /// caller counting batches must not have to ignore empty ones,
    /// which a filter that rejected a whole batch would otherwise
    /// produce.
    fn hand_over(&mut self, values: &[Vec<Value>]) -> Result<Flow> {
        if values.is_empty() {
            return Ok(Flow::More);
        }
        self.rows += values.len() as u64;
        let columns = self.columns;
        let flow = (self.sink)(Batch::new(columns, values))?;
        if matches!(flow, Flow::Stop) {
            self.stopped = true;
        }
        Ok(flow)
    }

    /// Hands a whole result over in batches of the size the caller
    /// asked for, for the statements that could not be streamed. The
    /// window is spent already, by whatever produced the rows.
    pub fn hand_over_all(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        for chunk in rows.chunks(self.batch) {
            if matches!(self.hand_over(chunk)?, Flow::Stop) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// What the run did, for the caller that asked for it.
    pub fn done(self, columns: Vec<String>, notices: Vec<DiagnosticRecord>) -> Streamed {
        Streamed {
            columns,
            rows: self.rows,
            stopped: self.stopped,
            streamed: self.streamed,
            notices,
        }
    }
}

/// Whether a stage can hand its rows over as it makes them.
///
/// Sorting cannot: the first row of an ordered answer is not known
/// until the last row has been read, so a statement with ORDER BY
/// buffers whatever this does. Neither can DISTINCT, which is the same
/// argument with a set instead of a sort, or an aggregate, whose rows
/// do not exist until its groups are closed. Everything else is row at
/// a time work over a pull pipeline that was already producing rows
/// one at a time, so streaming it is a matter of not collecting them.
fn streamable(sink: &SinkDef) -> bool {
    if sink.aggregate {
        return false;
    }
    // A filter after a window would have to be applied before the
    // window that precedes it, and the handoff spends the window last,
    // so that order is refused here rather than answered wrongly there.
    // No clause produces it today: WHERE binds before SKIP and LIMIT.
    let mut windowed = false;
    for op in &sink.post {
        match op {
            PostOp::Filter(_) if windowed => return false,
            PostOp::Filter(_) => {}
            PostOp::Skip(_) | PostOp::Limit(_) => windowed = true,
            PostOp::Sort(_) | PostOp::Distinct => return false,
        }
    }
    true
}

/// Applies the post operators a streamable stage may have to one batch
/// and hands the survivors over. Answers whether to keep pulling, which
/// is false once the caller has said stop or LIMIT has been spent.
fn stream_batch(
    sink: &SinkDef,
    ctx: &mut StageCtx,
    mut rows: Vec<Row>,
    st: &mut Streaming,
) -> Result<bool> {
    for op in &sink.post {
        match op {
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
            // The window is spent by the handoff, across batches
            // rather than inside each one.
            PostOp::Skip(_) | PostOp::Limit(_) => {}
            PostOp::Sort(_) | PostOp::Distinct => {
                unreachable!("streamable refuses sorting and DISTINCT")
            }
        }
    }
    st.feed(rows.into_iter().map(|r| r.values).collect())
}

/// Drives a streamable stage, handing rows over as they are made.
///
/// The buffer never holds more than a batch and the chunk being read
/// into it, which is the whole point: a scan of ten million rows costs
/// the caller that much memory instead of ten million rows of it, and a
/// caller that stops after the first batch stops the scan with it.
fn run_stage_stream(
    stage: &StageDef,
    query: &BoundQuery,
    ctx: &mut StageCtx,
    st: &mut Streaming,
) -> Result<()> {
    let top = stage.descs.len() - 1;
    let sink = &stage.sink;
    // Counts rather than per row expressions, so they are evaluated
    // once here and spent across the batches below.
    let mut skip = 0usize;
    let mut limit = None;
    for op in &sink.post {
        match op {
            PostOp::Skip(expr) => skip = count_expr(ctx, expr, "SKIP")?,
            PostOp::Limit(expr) => limit = Some(count_expr(ctx, expr, "LIMIT")?),
            _ => {}
        }
    }
    st.window(skip, limit);
    st.made_them();
    if !st.wants_more() {
        return Ok(());
    }
    let batch = st.batch;
    let mut buf: Vec<Row> = Vec::with_capacity(batch);
    while next(&stage.descs, ctx, top)? {
        // One pull fills a whole chunk, which is a vector of rows and
        // not a batch of them, so the buffer is drained a batch at a
        // time here rather than once per pull: the caller asked for a
        // batch size and gets it, whatever the chunk size is.
        each_config(ctx, &stage.unflat, &mut |ctx| {
            buf.push(materialize(sink, query, ctx)?);
            Ok(())
        })?;
        while buf.len() >= batch {
            let piece = buf.drain(..batch).collect();
            if !stream_batch(sink, ctx, piece, st)? {
                return Ok(());
            }
        }
    }
    if !buf.is_empty() {
        stream_batch(sink, ctx, buf, st)?;
    }
    Ok(())
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
    gone: &'a DeletedRows,
    params: &'a [Value],
    /// What each value query expression answered, worked out once for
    /// the whole statement and read the same way by every worker. A
    /// statement holding one that is answered per row does not reach
    /// this path: it runs a query inside the expression evaluator, and
    /// a worker's graph reader is not somewhere to start one.
    scalars: &'a Scalars<'a>,
    stop: &'a Interrupt,
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
        scalars: job.scalars,
        counts: job.counts,
        gone: job.gone,
        slot_loc: &stage.slot_loc,
        path_shapes: &job.query.path_shapes,
        stop: job.stop,
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
        scalars: job.scalars,
        counts: job.counts,
        gone: job.gone,
        slot_loc: &stage.slot_loc,
        path_shapes: &job.query.path_shapes,
        stop: job.stop,
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
        let (kind, detail) = op_label(&stage.descs[i], stage, query, schema);
        ops.push(OpProfile {
            kind,
            detail,
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

/// What a run owes its caller besides the rows: the per operator
/// counters EXPLAIN ANALYZE asked for, and the handoff a streaming
/// caller is reading through. Both are absent on the ordinary path and
/// they travel together because each of them is a reason this run is
/// not the ordinary one.
#[derive(Default)]
struct Extras<'a, 's> {
    profile: Option<&'a mut Profile>,
    stream: Option<&'a mut Streaming<'s>>,
}

/// The value query expressions of a statement, ready to be read
/// (GQ18), in the order [`BoundExpr::Scalar`] indexes them.
///
/// There are two kinds and the difference is the whole of the
/// decorrelation. One that reads nothing from the query around it
/// answers the same value for every row, so it is run once before the
/// plan that reads it starts and what is left where it was written is
/// a constant. One that reads a name cannot be: the value it stands
/// for is the row's, so it is run again for each of them, with the
/// row's values written into the parameters its captures name.
///
/// The plan is built here rather than carried in beside the
/// statement's because that keeps every caller that holds a bound
/// query and a plan working without knowing this exists. It is built
/// once per run of the statement either way, never once per row.
struct Scalars<'a> {
    /// The value of each one that was worked out once. A correlated
    /// one holds null here and never reads it.
    once: Vec<Value>,
    /// The plan of each correlated one, `None` for the rest.
    plans: Vec<Option<LogicalPlan>>,
    queries: &'a [BoundQuery],
    schema: &'a Schema,
    options: &'a Options,
}

impl<'a> Scalars<'a> {
    /// Answers the ones that read nothing and plans the ones that do.
    fn prepare(
        query: &'a BoundQuery,
        schema: &'a Schema,
        graph: &mut dyn Graph,
        params: &[Value],
        options: &'a Options,
    ) -> Result<Self> {
        let mut once = Vec::with_capacity(query.scalars.len());
        let mut plans = Vec::with_capacity(query.scalars.len());
        for scalar in &query.scalars {
            let built = crate::plan::build(scalar)?;
            let plan = crate::optimizer::optimize(built, scalar, schema)?;
            if scalar.captures.is_empty() {
                once.push(answer(&plan, scalar, schema, graph, params, options)?);
                plans.push(None);
            } else {
                once.push(Value::Null);
                plans.push(Some(plan));
            }
        }
        Ok(Scalars {
            once,
            plans,
            queries: &query.scalars,
            schema,
            options,
        })
    }

    /// Which of them are answered per row, which is what the statement
    /// carries a warning for.
    fn correlated(&self) -> impl Iterator<Item = &BoundQuery> {
        self.queries.iter().filter(|q| !q.captures.is_empty())
    }

    /// None at all, for a plan run with no query written inside it.
    #[cfg(test)]
    fn none(schema: &'a Schema, options: &'a Options) -> Self {
        Scalars {
            once: Vec::new(),
            plans: Vec::new(),
            queries: &[],
            schema,
            options,
        }
    }
}

/// Runs one query written inside an expression and reads out of it
/// whatever the word around it asked for.
///
/// An `EXISTS` asked whether it answered a row and a `VALUE` asked for
/// the value in it, and the mark on the query is what says which.
fn answer(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
) -> Result<Value> {
    match query.exists {
        true => any_row(plan, query, schema, graph, params, options),
        false => one_value(plan, query, schema, graph, params, options),
    }
}

/// Runs one existence predicate written around a query (ISO 19.4) and
/// answers whether it had a row.
///
/// The limit is the point. What was asked is whether there is a row
/// and one row settles that, so the run stops at the first: a query
/// that would have matched a million times costs the first match and
/// nothing else. It is never null, unlike the value form, because a
/// query that answered nothing answers false rather than not knowing.
fn any_row(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
) -> Result<Value> {
    let mut found = false;
    let mut sink = |batch: Batch<'_>| -> Result<Flow> {
        found |= !batch.is_empty();
        Ok(Flow::Stop)
    };
    let mut stream = Streaming::new(&mut sink, &query.columns, 1);
    run_stages(
        plan,
        query,
        schema,
        graph,
        params,
        options,
        Extras {
            profile: None,
            stream: Some(&mut stream),
        },
    )?;
    Ok(Value::Bool(found))
}

/// Runs one value query expression and reads the value out of it.
///
/// A query that answers no row stands for a null, one row for the
/// value in it, and more than one row is an error: what was written
/// stands for one value and there are several.
fn one_value(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
) -> Result<Value> {
    let result = run_stages(
        plan,
        query,
        schema,
        graph,
        params,
        options,
        Extras::default(),
    )?;
    match result.rows.len() {
        0 => Ok(Value::Null),
        1 => Ok(result.rows[0].first().cloned().unwrap_or(Value::Null)),
        n => Err(ZuError::gql(
            codes::C22000,
            format!(
                "a VALUE query stands for one value, and this one answered {n} rows: cut it down with LIMIT 1 or aggregate it"
            ),
        )),
    }
}

/// The binding variables of a statement, worked out (ISO 13.3, GP05
/// through GP13 and GP17).
///
/// Each one is a query and a parameter position: the query is run
/// here, once, and its answer is written into the position, so
/// everything that reads the name below this reads a parameter. They
/// are done in written order because a definition may read the ones in
/// front of it, and each is run against the parameters the ones before
/// it filled.
///
/// What is read out of the run is the one thing the kind asks for. A
/// value and a graph stand for one value, so they take the value the
/// query answered the way a `VALUE { ... }` does. A binding table
/// stands for the whole result, so it takes every row, and that is the
/// one place in the engine where a query's rows become a value.
fn fill_bindings(
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
) -> Result<Vec<Value>> {
    let mut filled = params.to_vec();
    if filled.len() < query.params.len() {
        filled.resize(query.params.len(), Value::Null);
    }
    for binding in &query.bindings {
        let built = crate::plan::build(&binding.query)?;
        let plan = crate::optimizer::optimize(built, &binding.query, schema)?;
        let value = match binding.kind {
            crate::ast::BindingKind::Table => {
                let result = run_stages(
                    &plan,
                    &binding.query,
                    schema,
                    graph,
                    &filled,
                    options,
                    Extras::default(),
                )?;
                // Epoch nought, which is the one a table that outlives
                // nothing is given. A handle records the epoch it was
                // read at so that a session can say the snapshot under
                // it has moved, and this table is made and read inside
                // one statement, so there is no later for it to be
                // stale in. A table the caller passed in is the one
                // that needs a real epoch, and it arrives with one.
                Value::BindingTable(crate::refs::BindingTable::new(
                    result.columns.clone(),
                    result.rows.into_vec(),
                    0,
                ))
            }
            _ => one_value(&plan, &binding.query, schema, graph, &filled, options)?,
        };
        // What it was written as and what it turned out to be have to
        // agree, because the name is going to be read as one of the
        // three and a reader that finds another thing there has no way
        // to say so later.
        let wrong = match binding.kind {
            crate::ast::BindingKind::Graph => !matches!(value, Value::Graph(_) | Value::Null),
            crate::ast::BindingKind::Value => {
                matches!(value, Value::Graph(_) | Value::BindingTable(_))
            }
            crate::ast::BindingKind::Table => false,
        };
        if wrong {
            return Err(ZuError::gql(
                codes::C22G03,
                format!(
                    "{} '{}' was defined with something that is not one: what it answered is {}",
                    binding.kind.word(),
                    binding.name,
                    crate::cast::value_type(&value)
                ),
            ));
        }
        // A definition written with a type is a statement about what
        // the query is going to answer, and it is checked here because
        // here is where the answer is. `IS TYPED` decides it, so a
        // declared type means the same thing in a definition as it
        // means in a predicate and there is one place that meaning
        // lives.
        if let Some(ty) = &binding.ty
            && !crate::typed::is_of(&value, ty)
        {
            return Err(ZuError::gql(
                codes::C22G03,
                format!(
                    "'{}' was defined as {} and what defines it answered {}",
                    binding.name,
                    ty,
                    crate::cast::value_type(&value)
                ),
            ));
        }
        filled[binding.param] = value;
    }
    Ok(filled)
}

/// The warning a statement carries for every value query expression it
/// could not decorrelate.
///
/// It rides with the answer rather than replacing it, because the
/// statement is answerable and what is wrong with it is what it costs:
/// one run of the query inside per row of the query around it. The
/// text names what the query read, since that is the thing to take out
/// of it to get the cost back.
fn correlated_warning(query: &BoundQuery) -> DiagnosticRecord {
    let read: Vec<&str> = query.captures.iter().map(|c| c.name.as_str()).collect();
    let word = match query.exists {
        true => "EXISTS",
        false => "VALUE",
    };
    DiagnosticRecord::new(
        codes::C01000,
        format!(
            "a {word} query reading {} from the query around it is answered once per row rather than once: lift it out and join it if the row count is large",
            read.join(", ")
        ),
    )
}

fn run_stages(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
    extras: Extras<'_, '_>,
) -> Result<QueryResult> {
    let Extras {
        mut profile,
        mut stream,
    } = extras;
    // The instant this statement answers the datetime value functions
    // with, read here and nowhere else. A run that arrives with one
    // already keeps it, which is what makes a query written inside an
    // expression agree with the query around it, and what lets a test
    // say what time it is.
    let read;
    let options = match options.clock {
        Some(_) => options,
        None => {
            read = Options {
                clock: Some(Clock::read()),
                ..options.clone()
            };
            &read
        }
    };
    if matches!(plan, LogicalPlan::Conjoin { .. }) {
        return run_conjoin(plan, query, schema, graph, params, options, profile);
    }
    // The binding variables written at the head of this statement and
    // at the head of every block in it (GP17). They come first because
    // everything after this may read one, and they are worked out
    // before the first row exists because a definition cannot read a
    // row.
    let filled;
    let params = if query.bindings.is_empty() {
        params
    } else {
        filled = fill_bindings(query, schema, graph, params, options)?;
        &filled
    };
    // Every value query expression this query holds (GQ18). The ones
    // that read nothing from the rows around them are answered here,
    // once, which is the whole cost of one however many rows read it.
    // The ones that do are planned here and run per row.
    let scalars = Scalars::prepare(query, schema, graph, params, options)?;
    let stages = build_stages(plan, query, schema, graph, params, options)?;
    // The extent of every table, which is what the schema says plus
    // whatever the engine has committed and not yet folded back into
    // it. A schema is built when the store publishes one and a write
    // that publishes no new schema still adds rows, so the count alone
    // would stop a scan short of the rows the statement before it
    // wrote.
    let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
    for node in schema.nodes() {
        counts.insert(node.id, node.node_count + graph.appended(node.id)?);
    }
    // Read once for the whole query, beside the counts, because the
    // extent and what is missing out of it are the same fact: a table
    // holds `counts` rows and every source below skips the ones here.
    let gone = graph.deleted()?;
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
    // The warning for every value query expression that did not
    // decorrelate, raised here rather than where one is run so that a
    // statement answering no row is warned about as loudly as one
    // answering a million. A run that reaches the parallel path has
    // none of these, which is why the two facts are settled together.
    let mut notices: Vec<DiagnosticRecord> = scalars.correlated().map(correlated_warning).collect();
    let per_row = !notices.is_empty();
    let last = stages.len() - 1;
    for (ix, stage) in stages.iter().enumerate() {
        // Only the last stage can stream, because every earlier one is
        // read by the stage above it rather than by the caller, and a
        // stage that streams cannot be split into morsels: the rows go
        // out in the order they are made, and workers make them in
        // whatever order they finish.
        let streaming = stream.is_some() && ix == last && streamable(&stage.sink);
        if !streaming
            && threads > 1
            && !per_row
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
                    gone: &gone,
                    params,
                    scalars: &scalars,
                    stop: &options.interrupt,
                };
                rows = run_stage_parallel(&job, graph, forks, morsels, &mut notices)?;
                continue;
            }
        }
        let mut ctx = StageCtx {
            graph: &mut *graph,
            params,
            scalars: &scalars,
            counts: &counts,
            gone: &gone,
            slot_loc: &stage.slot_loc,
            path_shapes: &query.path_shapes,
            stop: &options.interrupt,
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
        if streaming {
            let st = stream.as_deref_mut().expect("streaming was checked above");
            run_stage_stream(stage, query, &mut ctx, st)?;
            rows = Vec::new();
        } else {
            rows = run_stage(stage, query, &mut ctx)?;
        }
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

/// Runs a composite query: each operand as a query of its own, and the
/// conjunction over the pair of result tables (ISO 12.1).
///
/// The operands are run rather than fused because they share nothing.
/// A variable one of them matched is not a variable the other has, so
/// there is no row that both could be part of and nothing to push
/// through: what meets here is two tables of values.
///
/// `OTHERWISE` is the exception that proves it: the right operand is
/// not run at all unless the left answered nothing, which is a thing
/// only an operator standing above both of them can decide.
fn run_conjoin(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
    mut profile: Option<&mut Profile>,
) -> Result<QueryResult> {
    let LogicalPlan::Conjoin {
        left,
        right,
        how,
        build,
    } = plan
    else {
        unreachable!("a conjoin was matched before this was called");
    };
    // The operands were joined on left to right, so the conjoins below
    // this one count off the operand it joins.
    let nth = conjoin_depth(left);
    let operand = query
        .conjoined
        .get(nth)
        .map_or(query, |joined| joined.query.as_ref());
    let run = |plan: &LogicalPlan,
               query: &BoundQuery,
               graph: &mut dyn Graph,
               profile: Option<&mut Profile>|
     -> Result<QueryResult> {
        run_stages(
            plan,
            query,
            schema,
            graph,
            params,
            options,
            Extras {
                profile,
                stream: None,
            },
        )
    };
    let mut result = run(left, query, graph, profile.as_deref_mut())?;
    let (op, all) = match how {
        Conjunction::Otherwise => {
            // The left answered, so the right is not a thing that
            // happened: no scan of it runs and no row of it is read.
            if !result.rows.is_empty() {
                return Ok(result);
            }
            let mut right = run(right, operand, graph, profile)?;
            right.columns = query.columns.clone();
            return Ok(right);
        }
        Conjunction::Set { op, all } => (*op, *all),
    };
    let other = run(right, operand, graph, profile)?;
    let notices = {
        let mut notices = result.notices;
        notices.extend(other.notices);
        notices
    };
    let rows = conjoin_rows(
        result.rows.into_vec(),
        other.rows.into_vec(),
        op,
        all,
        *build,
    );
    result = QueryResult::new(query.columns.clone(), rows);
    for record in notices {
        result.notice(record);
    }
    Ok(result)
}

/// How many conjoins stand on the left spine from here down, this one
/// counted.
fn conjoin_depth(plan: &LogicalPlan) -> usize {
    match plan {
        LogicalPlan::Conjoin { left, .. } => 1 + conjoin_depth(left),
        _ => 0,
    }
}

/// The rows a set operator makes out of two result tables.
///
/// Every one of the six is a statement about how many times a row
/// appears, so all six are counted rather than tested: the table built
/// over one operand holds a count per distinct row, and the other
/// operand is read against it. `ALL` spends those counts one at a
/// time and `DISTINCT` spends each of them once.
///
/// The table is the ordered one this engine's `DISTINCT` and `GROUP
/// BY` already use, so a value that groups here groups there.
fn conjoin_rows(
    left: Vec<Vec<Value>>,
    right: Vec<Vec<Value>>,
    op: SetOp,
    all: bool,
    build: Side,
) -> Vec<Vec<Value>> {
    let key = |row: &[Value]| -> Vec<OrdValue> { row.iter().cloned().map(OrdValue).collect() };
    let counted = |rows: &[Vec<Value>]| -> BTreeMap<Vec<OrdValue>, usize> {
        let mut counts = BTreeMap::new();
        for row in rows {
            *counts.entry(key(row)).or_insert(0) += 1;
        }
        counts
    };
    match op {
        SetOp::Union if all => {
            // Nothing is built and nothing is held: the rows of one
            // operand are the answer, and then the rows of the other.
            let mut out = left;
            out.extend(right);
            out
        }
        SetOp::Union => {
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            for row in left.into_iter().chain(right) {
                if seen.insert(key(&row)) {
                    out.push(row);
                }
            }
            out
        }
        SetOp::Intersect => {
            // Symmetric, so the table goes over whichever operand the
            // optimizer expects to be smaller and the other is read
            // against it. The answer's rows come from the probing side
            // either way, which is a choice about which copy of an
            // equal row is returned and not about which rows are.
            let (built, probe) = match build {
                Side::Left => (left, right),
                Side::Right => (right, left),
            };
            let mut counts = counted(&built);
            let mut out = Vec::new();
            for row in probe {
                let Some(left_over) = counts.get_mut(&key(&row)) else {
                    continue;
                };
                if *left_over == 0 {
                    continue;
                }
                if all {
                    *left_over -= 1;
                } else {
                    *left_over = 0;
                }
                out.push(row);
            }
            out
        }
        SetOp::Except => {
            // Subtracting is not symmetric, but which side is held is:
            // hold the right and read the left against it, or hold the
            // left and take the right's rows out of what is held. The
            // two spend the counts in a different order and arrive at
            // the same multiset.
            let mut counts = match build {
                Side::Right => counted(&right),
                Side::Left => {
                    let mut counts = counted(&left);
                    for row in &right {
                        if let Some(held) = counts.get_mut(&key(row)) {
                            *held = if all { held.saturating_sub(1) } else { 0 };
                        }
                    }
                    counts
                }
            };
            let mut out = Vec::new();
            for row in left {
                let k = key(&row);
                let keep = match build {
                    // The table holds what is being taken away, so a
                    // row survives when nothing is left to take.
                    Side::Right => match counts.get_mut(&k) {
                        Some(subtract) if *subtract > 0 => {
                            *subtract -= usize::from(all);
                            false
                        }
                        Some(_) => all,
                        None => true,
                    },
                    // The table holds what survived, so a row is
                    // written out as many times as the table says.
                    Side::Left => match counts.get_mut(&k) {
                        Some(left_over) if *left_over > 0 => {
                            *left_over -= 1;
                            true
                        }
                        _ => false,
                    },
                };
                if !keep {
                    continue;
                }
                // DISTINCT answers once however many times the left
                // wrote a row, so what has been answered is struck out.
                if !all {
                    counts.insert(k, 0);
                }
                out.push(row);
            }
            out
        }
    }
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
    run_stages(
        plan,
        query,
        schema,
        graph,
        params,
        options,
        Extras::default(),
    )
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
        Extras {
            profile: Some(&mut profile),
            ..Extras::default()
        },
    )?;
    Ok((result, profile))
}

/// Runs an optimized plan and hands the rows to `st` in batches as they
/// are made, rather than returning them all (`dx/04` §4).
///
/// The point is memory and the first row. A caller reading ten million
/// rows to write them somewhere else holds one batch instead of the
/// answer, and a caller that has seen enough returns [`Flow::Stop`] and
/// the scan under it stops with it, which is the same boundary an
/// interrupt is answered at and costs the same nothing.
///
/// A statement that cannot answer its first row before it has read its
/// last one is run whole and handed over in batches anyway, so a
/// caller's loop is the same either way and the difference is what it
/// costs. [`Streamed::streamed`] says which happened, and ORDER BY,
/// DISTINCT and the aggregates are the three that cannot.
pub fn execute_streaming(
    plan: &LogicalPlan,
    query: &BoundQuery,
    schema: &Schema,
    graph: &mut dyn Graph,
    params: &[Value],
    options: &Options,
    st: &mut Streaming<'_>,
) -> Result<Streamed> {
    let result = run_stages(
        plan,
        query,
        schema,
        graph,
        params,
        options,
        Extras {
            stream: Some(st),
            ..Extras::default()
        },
    )?;
    if !st.streamed && !st.stopped {
        st.hand_over_all(&result.rows)?;
    }
    Ok(Streamed {
        columns: result.columns,
        rows: st.rows,
        stopped: st.stopped,
        streamed: st.streamed,
        notices: result.notices,
    })
}

/// The batch size a caller gets when it does not pick one: one vector,
/// the unit the executor already works in, which is large enough that
/// the per batch call is nothing beside the rows in it and small enough
/// that holding one is not holding the answer.
pub const STREAM_BATCH: usize = VECTOR_SIZE;

/// Hands a result that was produced whole over to a streaming sink, in
/// batches, for the statements a session runs some other way and a
/// streaming caller asked for anyway. It reports `streamed: false`,
/// because it did not.
pub fn stream_result(
    result: QueryResult,
    batch_rows: usize,
    sink: &mut dyn FnMut(Batch<'_>) -> Result<Flow>,
) -> Result<Streamed> {
    let mut st = Streaming::new(sink, &result.columns, batch_rows);
    st.hand_over_all(&result.rows)?;
    let (rows, stopped) = (st.rows, st.stopped);
    Ok(Streamed {
        columns: result.columns,
        rows,
        stopped,
        streamed: false,
        notices: result.notices,
    })
}

/// What a streamed statement did, once its rows have all been handed
/// over. The rows themselves are gone by now, which is the point.
#[derive(Debug, Clone, PartialEq)]
pub struct Streamed {
    /// The column names, the one part of a result a streaming caller
    /// still wants after the fact.
    pub columns: Vec<String>,
    /// How many rows were handed over, which is fewer than the
    /// statement would have returned when the caller stopped early.
    pub rows: u64,
    /// Whether the caller stopped it.
    pub stopped: bool,
    /// Whether the rows were made and handed over in batches, rather
    /// than buffered whole and cut into batches afterwards.
    pub streamed: bool,
    /// Conditions raised without stopping the statement, as on any
    /// other result.
    pub notices: Vec<DiagnosticRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{NodeDef, RelDef};

    /// The handoff is what both executors stream through, so what it
    /// does with a window and with an empty piece is checked here once
    /// rather than through either of them.
    #[test]
    fn the_handoff_spends_one_window_across_the_pieces_it_is_fed() {
        let columns = vec!["id".to_string()];
        let piece = |from: i64, to: i64| -> Vec<Vec<Value>> {
            (from..to).map(|i| vec![Value::Int(i)]).collect()
        };
        let mut seen: Vec<Vec<i64>> = Vec::new();
        let mut sink = |batch: Batch<'_>| {
            seen.push(
                batch
                    .rows()
                    .iter()
                    .map(|row| match row[0] {
                        Value::Int(i) => i,
                        _ => unreachable!("the test feeds integers"),
                    })
                    .collect(),
            );
            Ok(Flow::More)
        };
        let mut st = Streaming::new(&mut sink, &columns, 4);
        st.window(3, Some(9));
        // The skip is longer than the first piece, so it carries into
        // the second one instead of restarting there.
        assert!(st.feed(piece(0, 2)).expect("fed"));
        assert!(st.feed(piece(2, 10)).expect("fed"));
        // The limit runs out inside this piece, which is what ends the
        // scan: nothing after it is wanted.
        assert!(!st.feed(piece(10, 20)).expect("fed"));
        assert!(!st.wants_more());
        let out = st.done(columns.clone(), Vec::new());
        assert_eq!(seen, vec![vec![3, 4, 5, 6], vec![7, 8, 9], vec![10, 11]]);
        assert_eq!(out.rows, 9);
        assert!(!out.stopped, "the limit ended it, not the caller");
        assert!(!out.streamed, "nothing said it made these as it went");

        // An empty piece is not a batch. A caller that counts batches
        // would otherwise have to know which of its filters rejected a
        // whole morsel.
        let mut calls = 0;
        let mut sink = |_: Batch<'_>| {
            calls += 1;
            Ok(Flow::Stop)
        };
        let mut st = Streaming::new(&mut sink, &columns, 4);
        assert!(st.feed(Vec::new()).expect("fed"));
        // And a caller that stops is a caller nothing else is handed:
        // one batch of the hundred rows fed here, not twenty five.
        assert!(!st.feed(piece(0, 100)).expect("fed"));
        assert_eq!(st.done(columns.clone(), Vec::new()).rows, 4);
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_ceiling_is_violated_by_a_factor_and_never_by_a_rounding() {
        let op = |flat, bnd| OpProfile {
            kind: "Expand",
            detail: String::new(),
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
                    labels: Vec::new(),
                },
                NodeDef {
                    id: 1,
                    name: "Place".into(),
                    node_count: 3,
                    labels: Vec::new(),
                },
            ],
            vec![
                RelDef {
                    id: 2,
                    name: "KNOWS".into(),
                    from: 0,
                    to: 0,
                    edge_count: 8,
                    undirected: false,
                },
                RelDef {
                    id: 3,
                    name: "IS_LOCATED_IN".into(),
                    from: 0,
                    to: 1,
                    edge_count: 6,
                    undirected: false,
                },
            ],
        )
        .expect("schema")
    }

    struct MockGraph {
        edges: BTreeMap<u32, Vec<(u64, u64)>>,
        gone: DeletedRows,
    }

    /// The KNOWS edges of the fixture, in load order, which is sorted
    /// by source and then destination the way a bulk load leaves them.
    /// An edge's place here is the row its properties would sit in, so
    /// this is also what `knows` reads to build an expected value.
    const KNOWS: [(u64, u64); 8] = [
        (0, 1),
        (0, 2),
        (1, 2),
        (1, 3),
        (2, 4),
        (3, 4),
        (4, 5),
        (5, 0),
    ];

    fn mock() -> MockGraph {
        let mut edges = BTreeMap::new();
        edges.insert(2, KNOWS.to_vec());
        edges.insert(3, vec![(0, 0), (1, 1), (2, 2), (3, 0), (4, 1), (5, 2)]);
        MockGraph {
            edges,
            gone: DeletedRows::new(),
        }
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

        /// The mock's edge lists are written in load order, so an
        /// edge's row is where it sits in one. Answering this honestly
        /// rather than taking the trait's default is what lets the
        /// tests that run one query down two plans compare the rel
        /// values whole: a fused close and the expand pair it replaces
        /// have to name the same edge, and a row every path calls zero
        /// would not tell them apart.
        fn edge_ordinal(&mut self, rel: u32, src: u64, dst: u64) -> Result<Option<u64>> {
            Ok(self.edges[&rel]
                .iter()
                .position(|&e| e == (src, dst))
                .map(|i| i as u64))
        }

        /// Load order keeps a pair's copies together, so the run is the
        /// stretch of equal pairs from the first one.
        fn edge_run(&mut self, rel: u32, src: u64, dst: u64) -> Result<Option<(u64, u64)>> {
            let list = &self.edges[&rel];
            let Some(at) = list.iter().position(|&e| e == (src, dst)) else {
                return Ok(None);
            };
            let count = list[at..].iter().take_while(|&&e| e == (src, dst)).count();
            Ok(Some((at as u64, count as u64)))
        }

        fn neighbor_ordinals(
            &mut self,
            rel: u32,
            node: u64,
            reversed: bool,
            _len: usize,
            out: &mut Vec<u64>,
        ) -> Result<()> {
            out.clear();
            // The same walk `neighbors` takes over the same list, so
            // the two line up slot for slot.
            for (i, &(src, dst)) in self.edges[&rel].iter().enumerate() {
                if (!reversed && src == node) || (reversed && dst == node) {
                    out.push(i as u64);
                }
            }
            Ok(())
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

        fn deleted(&mut self) -> Result<DeletedRows> {
            Ok(self.gone.clone())
        }

        fn fork(&self) -> Option<Box<dyn Graph + Send>> {
            Some(Box::new(MockGraph {
                edges: self.edges.clone(),
                gone: self.gone.clone(),
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
                // The round count is optional, so the stub reports what
                // reached it rather than a label: that is the only part
                // of cdlp the plumbing decides.
                "cdlp" => {
                    let rounds = match args.first() {
                        Some(Value::Int(rounds)) => *rounds,
                        _ => -1,
                    };
                    Ok((0..n).map(|_| vec![Value::Int(rounds)]).collect())
                }
                "lcc" => Ok((0..n)
                    .map(|o| vec![Value::Float(o as f64 / 100.0)])
                    .collect()),
                "triangle_count" => Ok((0..n).map(|o| vec![Value::Int(o + 1)]).collect()),
                // The stub scores the sources themselves, which is
                // enough to show the list arrived and resolved.
                "betweenness" => {
                    let Some(Value::List(sources)) = args.first() else {
                        return Err(invalid("mock betweenness needs a source list".into()));
                    };
                    Ok((0..n)
                        .map(|o| {
                            vec![Value::Float(if sources.contains(&Value::Int(o)) {
                                1.0
                            } else {
                                0.0
                            })]
                        })
                        .collect())
                }
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
                // The stub answers the source's own distance and
                // nothing else, which is enough to show the source and
                // the column name arrived.
                "sssp_weighted" => {
                    let (Some(Value::Int(source)), Some(Value::Str(column))) =
                        (args.first(), args.get(1))
                    else {
                        return Err(invalid(
                            "mock sssp_weighted needs a source and a weight column".into(),
                        ));
                    };
                    let width = column.len() as i64;
                    Ok((0..n)
                        .map(|o| {
                            vec![if o == *source {
                                Value::Int(width)
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
        run_graph(source, params, options, mock())
    }

    /// The same run over a graph the caller built, for the shapes whose
    /// answer depends on edges the shared fixture does not have.
    fn run_graph(
        source: &str,
        params: &[(&str, Value)],
        options: Options,
        mut graph: MockGraph,
    ) -> QueryResult {
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
        execute(&optimized, &query, &schema, &mut graph, &args, &options).expect("execute")
    }

    /// The same run over a graph that has lost rows: `dead` names the
    /// rows a delete took away, by table.
    fn run_deleted(source: &str, dead: &[(u32, &[u64])]) -> QueryResult {
        let schema = schema();
        let parsed = crate::parser::parse(source).expect("parse");
        let query = crate::binder::bind(&parsed, &schema).expect("bind");
        let built = crate::plan::build(&query).expect("plan");
        let optimized = crate::optimizer::optimize(built, &query, &schema).expect("optimize");
        let mut graph = mock();
        graph.gone = dead
            .iter()
            .map(|&(table, rows)| (table, rows.into()))
            .collect();
        let options = Options {
            threads: 1,
            ..Options::default()
        };
        execute(&optimized, &query, &schema, &mut graph, &[], &options).expect("execute")
    }

    #[test]
    fn a_deleted_row_is_not_scanned() {
        let r = run_deleted(
            "MATCH (p:Person) RETURN p.id AS id ORDER BY id",
            &[(0, &[2, 4])],
        );
        assert_eq!(int_rows(&r), [[0], [1], [3], [5]]);
    }

    #[test]
    fn a_deleted_row_is_not_found_by_its_key() {
        let r = run_deleted("MATCH (p:Person {id: 2}) RETURN p.id AS id", &[(0, &[2])]);
        assert!(r.rows.is_empty(), "the key names a row that is gone");
        let still = run_deleted("MATCH (p:Person {id: 3}) RETURN p.id AS id", &[(0, &[2])]);
        assert_eq!(int_rows(&still), [[3]]);
    }

    #[test]
    fn a_call_does_not_yield_a_deleted_row() {
        // The kernel answers for every row of the table, deleted rows
        // included, because it walks an adjacency whose offsets do not
        // move; what comes back out of the CALL is the rows that are
        // still there.
        let r = run_deleted(
            "CALL wcc('KNOWS') YIELD node, component RETURN node.id AS id ORDER BY id",
            &[(0, &[0, 5])],
        );
        assert_eq!(int_rows(&r), [[1], [2], [3], [4]]);
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
        let names: Vec<String> = p.stages[0].ops.iter().map(OpProfile::name).collect();
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
                labels: Vec::new(),
            }],
            vec![RelDef {
                id: 2,
                name: "KNOWS".into(),
                from: 0,
                to: 0,
                edge_count: 6,
                undirected: false,
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
        let LogicalPlan::Aggregate {
            input,
            keys,
            aggs,
            order_aggs,
        } = built
        else {
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
            bracket: None,
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
            bracket: None,
        };
        let b_from_c = expand(scan, hop2, c, b, RelDirection::In, false, false);
        let a_from_c = expand(b_from_c, close, c, a, RelDirection::In, false, false);
        let close_a_b = expand(a_from_c, hop1, a, b, RelDirection::Out, true, true);
        let plan = LogicalPlan::Aggregate {
            input: Box::new(close_a_b),
            keys,
            aggs,
            order_aggs,
        };
        // Everyone points at 0, plus 1 knows 3 and 2 knows 4: the
        // triangles are (1, 3, 0) and (2, 4, 0).
        let mut graph = MockGraph {
            edges: BTreeMap::from([(2u32, vec![(1, 0), (2, 0), (3, 0), (4, 0), (1, 3), (2, 4)])]),
            gone: DeletedRows::new(),
        };
        let options = Options {
            threads: 1,
            ..Options::default()
        };
        let (r, p) =
            execute_profiled(&plan, &query, &schema, &mut graph, &[], &options).expect("execute");
        assert_eq!(int_rows(&r), [[2]]);
        let names: Vec<String> = p.stages[0].ops.iter().map(OpProfile::name).collect();
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
        let names: Vec<String> = p.stages[0].ops.iter().map(OpProfile::name).collect();
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

    fn op_names(p: &Profile) -> Vec<String> {
        p.stages[0].ops.iter().map(OpProfile::name).collect()
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
    fn leapfrog_pairs_every_copy_with_every_copy() {
        // A duplicate on either side is another edge, so a value the
        // seed holds twice and the probe once is two hits, and one both
        // hold twice is four.
        let (seed, probe) = ([2u64, 2, 3, 7, 9, 12], [1u64, 2, 7, 7, 8, 12]);
        let mut hits = Vec::new();
        leapfrog(&seed, &probe, |s, p| hits.push((seed[s], s, p)));
        assert_eq!(
            hits,
            [(2, 0, 1), (2, 1, 1), (7, 3, 2), (7, 3, 3), (12, 5, 5),]
        );
        hits.clear();
        leapfrog(&[5], &[], |s, p| hits.push((5, s, p)));
        leapfrog(&[], &[5], |s, p| hits.push((5, s, p)));
        assert_eq!(hits, []);
    }

    /// A pair drawn twice is two edges, so a pattern that closes onto
    /// it matches twice, and the three plans that can close it have to
    /// say so alike: the fused intersection, the binary pair, and the
    /// ASP probe that reads its edges out of a swept set instead of out
    /// of storage.
    #[test]
    fn a_close_on_a_repeated_pair_reports_every_copy() {
        let fixture = || {
            let mut edges = BTreeMap::new();
            // The closing leg 0 -> 2 is drawn twice, the other two legs
            // once, so the triangle matches twice and no other shape in
            // the graph can account for the second row.
            edges.insert(2, vec![(0u64, 1u64), (0, 2), (0, 2), (1, 2)]);
            edges.insert(3, Vec::new());
            MockGraph {
                edges,
                gone: DeletedRows::new(),
            }
        };
        let count = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[:KNOWS]->(c) \
                     RETURN count(*) AS n";
        assert_eq!(int_rows(&run_graph(count, &[], wcoj(), fixture())), [[2]]);
        assert_eq!(
            int_rows(&run_graph(count, &[], no_wcoj(), fixture())),
            [[2]]
        );

        // And the rows name the two copies rather than the first one
        // twice, which is what the property read downstream depends on.
        let rels = "MATCH (a:Person)-[:KNOWS]->(b)-[:KNOWS]->(c), (a)-[r:KNOWS]->(c) RETURN r";
        let fused = run_graph(rels, &[], wcoj(), fixture());
        let binary = run_graph(rels, &[], no_wcoj(), fixture());
        assert_eq!(
            fused.rows,
            [
                vec![Value::Rel {
                    table: 2,
                    src: 0,
                    dst: 2,
                    ord: 1
                }],
                vec![Value::Rel {
                    table: 2,
                    src: 0,
                    dst: 2,
                    ord: 2
                }],
            ]
        );
        assert_eq!(fused.rows, binary.rows);
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
    fn multiway_intersect_inside_an_optional_bracket() {
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
    fn call_passes_the_cdlp_round_count_or_leaves_it_to_the_kernel() {
        let r = run(
            "CALL cdlp('KNOWS', 4) YIELD node, community RETURN DISTINCT community",
            &[],
        );
        assert_eq!(int_rows(&r), [[4]]);
        let r = run(
            "CALL cdlp('KNOWS') YIELD node, community RETURN DISTINCT community",
            &[],
        );
        assert_eq!(int_rows(&r), [[-1]]);
    }

    #[test]
    fn call_yields_the_lcc_coefficient_column() {
        let r = run(
            "CALL lcc('KNOWS') YIELD node, coefficient \
             RETURN node.id AS id, coefficient ORDER BY id DESC LIMIT 1",
            &[],
        );
        assert_eq!(r.columns, ["id", "coefficient"]);
        assert_eq!(r.rows, [[Value::Int(5), Value::Float(0.05)]]);
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
                "CALL cdlp('KNOWS', 2, 3) YIELD node, community RETURN community",
                "optional round count",
            ),
            (
                "CALL cdlp('KNOWS', 'ten') YIELD node, community RETURN community",
                "round count must be an integer",
            ),
            (
                "CALL lcc('KNOWS', 1) YIELD node, coefficient RETURN coefficient",
                "takes only the rel table",
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
        // And a label or a relationship type the graph does not hold is
        // neither: nothing carries it, so the pattern matches nothing
        // and the statement answers with no rows.
        assert!(
            run("MATCH (a:Nonexistent) RETURN a.id AS x", &[])
                .rows
                .is_empty()
        );
        assert!(
            run("MATCH (a:Person)-[:NOPE]->(b) RETURN a.id AS x", &[])
                .rows
                .is_empty()
        );
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
    fn shortest_paths_are_counted_without_being_built() {
        // The walk above, counted rather than built. Six shortest
        // paths reach the five endpoints, node 2 holding two of them,
        // and ANY SHORTEST keeps one per endpoint however many there
        // are, so the two selectors count differently over the same
        // graph.
        for (selector, want) in [("ALL", 6), ("ANY", 5)] {
            let text = format!(
                "MATCH {selector} SHORTEST (a:Person {{id: 3}})-[r:KNOWS*]-(b) RETURN count(*) AS n"
            );
            assert_eq!(int_rows(&run(&text, &[])), [[want]], "{selector} SHORTEST");
            let (_, p) = profiled(&text, &[]);
            let counted = p
                .stages
                .iter()
                .flat_map(|s| &s.ops)
                .any(|o| o.kind == "VarExpand" && o.detail.contains(" count "));
            assert!(counted, "{selector} SHORTEST did not count off the DAG");
        }
        // A bare count of the endpoint is the same count, and it is the
        // rewrite that has to say so: the walk it counts materializes
        // no endpoint to count.
        let r = run(
            "MATCH ALL SHORTEST (a:Person {id: 3})-[r:KNOWS*]-(b) RETURN count(b) AS n",
            &[],
        );
        assert_eq!(int_rows(&r), [[6]]);
    }

    #[test]
    fn counting_paths_agrees_with_building_them() {
        // Flat mode enumerates every path and counts the rows, which is
        // the oracle the DAG sum is checked against: every seed, both
        // selectors, both directions, and the hop windows the levels
        // are allowed to answer.
        for id in 0..6 {
            for selector in ["ALL", "ANY"] {
                for arrow in ["-[r:KNOWS*]->", "-[r:KNOWS*]-", "-[r:KNOWS*1..2]->"] {
                    let text = format!(
                        "MATCH {selector} SHORTEST (a:Person {{id: {id}}}){arrow}(b)                          RETURN count(*) AS n"
                    );
                    let counted = run(&text, &[]);
                    let built = run_opts(
                        &text,
                        &[],
                        Options {
                            flat: true,
                            ..Options::default()
                        },
                    );
                    assert_eq!(int_rows(&counted), int_rows(&built), "{text}");
                }
            }
        }
    }

    #[test]
    fn a_counted_walk_that_reads_its_paths_still_builds_them() {
        // Anything read off the walk besides the one count blocks the
        // rewrite, because a count off the DAG is a number and not a
        // set of paths.
        let r = run(
            "MATCH ALL SHORTEST (a:Person {id: 3})-[r:KNOWS*]-(b) \
             RETURN b.id AS b, count(*) AS n ORDER BY b",
            &[],
        );
        assert_eq!(int_rows(&r), [[0, 1], [1, 1], [2, 2], [4, 1], [5, 1]]);
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

    #[test]
    fn a_pinned_endpoint_turns_the_search_into_a_meeting_one() {
        let (r, p) = profiled(
            "MATCH ANY SHORTEST (a:Person {id: 0})-[r:KNOWS*]->(b:Person {id: 5}) \
             RETURN size(r) AS hops",
            &[],
        );
        assert_eq!(int_rows(&r), [[3]]);
        let names = op_names(&p);
        assert!(
            names
                .iter()
                .any(|n| n.contains("any shortest") && n.contains("[id = 5]")),
            "the endpoint filter was not absorbed: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("Filter")),
            "the absorbed filter is still being evaluated: {names:?}"
        );
    }

    #[test]
    fn the_meeting_search_answers_every_pair_the_one_sided_one_does() {
        // The one-sided search over all endpoints is the reference: for
        // every source it says how far each node is, and the pinned
        // query has to say the same thing one pair at a time. Both
        // directed and undirected, so the backward frontier is walked
        // both ways round.
        for arrow in ["->", "-"] {
            for src in 0..6u64 {
                let all = run(
                    &format!(
                        "MATCH ANY SHORTEST (a:Person {{id: {src}}})-[r:KNOWS*]{arrow}(b) \
                         RETURN b.id AS b, size(r) AS hops ORDER BY b"
                    ),
                    &[],
                );
                let want: BTreeMap<i64, i64> = int_rows(&all)
                    .into_iter()
                    .map(|row| (row[0], row[1]))
                    .collect();
                for dst in 0..6u64 {
                    let one = run(
                        &format!(
                            "MATCH ANY SHORTEST (a:Person {{id: {src}}})-[r:KNOWS*]{arrow}\
                             (b:Person {{id: {dst}}}) RETURN size(r) AS hops"
                        ),
                        &[],
                    );
                    let got: Vec<i64> = int_rows(&one).into_iter().map(|row| row[0]).collect();
                    let want: Vec<i64> = want.get(&(dst as i64)).copied().into_iter().collect();
                    assert_eq!(got, want, "{src} to {dst} over '{arrow}'");
                }
            }
        }
    }

    #[test]
    fn the_meeting_search_keeps_the_hop_window() {
        // 5 sits three hops from 0, so a ceiling under three answers
        // nothing and one at or over it answers three, exactly as the
        // unpinned search does. A selector only ever carries a lower
        // bound of one: the binder refuses to force a minimum-hop path
        // to be longer than it is.
        for hops in ["*1..2", "*1..3", "*1..4", "*1.."] {
            let one = run(
                &format!(
                    "MATCH ANY SHORTEST (a:Person {{id: 0}})-[r:KNOWS{hops}]->\
                     (b:Person {{id: 5}}) RETURN size(r) AS hops"
                ),
                &[],
            );
            let all = run(
                &format!(
                    "MATCH ANY SHORTEST (a:Person {{id: 0}})-[r:KNOWS{hops}]->(b) \
                     WHERE b.id = 5 AND true RETURN size(r) AS hops"
                ),
                &[],
            );
            assert_eq!(int_rows(&one), int_rows(&all), "window {hops}");
        }
    }

    #[test]
    fn a_pinned_endpoint_nobody_carries_matches_nothing() {
        let r = run(
            "MATCH ANY SHORTEST (a:Person {id: 0})-[r:KNOWS*]->(b:Person {id: 99}) \
             RETURN size(r) AS hops",
            &[],
        );
        assert!(r.rows.is_empty());
        // The start again: a shortest path never comes back to it,
        // because its zero-hop path is under the lower bound of one.
        let r = run(
            "MATCH ANY SHORTEST (a:Person {id: 0})-[r:KNOWS*]->(b:Person {id: 0}) \
             RETURN size(r) AS hops",
            &[],
        );
        assert!(r.rows.is_empty());
    }

    #[test]
    fn the_meeting_search_returns_a_path_somebody_can_walk() {
        // 0 to 5 has one shortest path, 0-2-4-5, and the halves the two
        // frontiers found have to come back as one chain in order.
        let r = run(
            "MATCH p = ANY SHORTEST (a:Person {id: 0})-[r:KNOWS*]->(b:Person {id: 5}) \
             RETURN p",
            &[],
        );
        assert_eq!(
            r.rows,
            [[path(vec![
                node(0),
                knows(0, 2),
                node(2),
                knows(2, 4),
                node(4),
                knows(4, 5),
                node(5),
            ])]]
        );
    }

    fn node(offset: u64) -> Value {
        Value::Node { table: 0, offset }
    }

    fn knows(src: u64, dst: u64) -> Value {
        Value::Rel {
            table: 2,
            src,
            dst,
            ord: KNOWS
                .iter()
                .position(|&e| e == (src, dst))
                .expect("the fixture has this edge") as u64,
        }
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
        let gone = DeletedRows::new();
        let stop = Interrupt::default();
        let options = Options::default();
        let scalars = Scalars::none(&schema, &options);
        let mut graph = mock();
        let mut ctx = StageCtx {
            graph: &mut graph,
            params: &[],
            scalars: &scalars,
            counts: &counts,
            gone: &gone,
            slot_loc: &slot_loc,
            path_shapes: &shapes,
            stop: &stop,
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
            undirected: false,
        }];
        let spec = VarSpec {
            rels: &rels,
            direction: RelDirection::Out,
            to_tables: &[0],
            min: 1,
            max: Some(6),
            mode: PathMode::Trail,
            steps: None,
            gate: EdgeGate::OPEN,
        };
        let root = chain_root(0, 0);
        let mut out = Paths::new(Keep::All);
        enumerate_paths(&mut ctx, &spec, 0, 0, &root, &mut out).expect("enumerate");
        let (_far, trails) = out.finish();
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
        let names: Vec<String> = stage.ops.iter().map(OpProfile::name).collect();
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
        assert_eq!(expand_c.name(), "ExpandCount (b)-[:KNOWS]->(c)");
        // One pull for the whole absorbed neighbor vector of node 0,
        // reporting three counted paths without materializing a list:
        // the count-to-degree rewrite at work.
        assert_eq!((expand_c.pulls, expand_c.rows), (1, 3));
        // No flatten sits between the two expands anymore.
        assert!(
            !stage.ops.iter().any(|o| o.name() == "Flatten b"),
            "absorbed source still flattens: {:?}",
            stage.ops.iter().map(OpProfile::name).collect::<Vec<_>>()
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
            assert_eq!(
                sorted(fac.rows.into_vec()),
                sorted(flat.rows.into_vec()),
                "query: {q}"
            );
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
        assert_eq!(rows, vec![vec![knows(0, 1)], vec![knows(0, 2)]]);
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
                .any(|o| o.kind == "ExpandCount");
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
            .find(|o| o.kind == "Scan")
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
    fn exists_keeps_the_people_with_a_far_friend() {
        // Out-neighbors over 3: 2 -> 4, 3 -> 4, 4 -> 5. The block's own
        // WHERE runs inside the bracket, so it decides the match rather
        // than the row.
        let r = run(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 } \
             RETURN a.id AS id ORDER BY id",
            &[],
        );
        assert_eq!(int_rows(&r), [[2], [3], [4]]);
    }

    #[test]
    fn not_exists_keeps_the_rest() {
        let r = run(
            "MATCH (a:Person) WHERE NOT EXISTS { MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 } \
             RETURN a.id AS id ORDER BY id",
            &[],
        );
        assert_eq!(int_rows(&r), [[0], [1], [5]]);
    }

    #[test]
    fn exists_hands_the_row_up_once_however_many_matched() {
        // Everyone knows someone and 0 and 1 know two people each, so a
        // bracket that passed its matches through the way an optional
        // does would count eight rows here instead of six.
        let r = run(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) } RETURN count(*) AS n",
            &[],
        );
        assert_eq!(int_rows(&r), [[6]]);
    }

    #[test]
    fn exists_ties_its_block_to_the_row_being_tested() {
        // b is the outer variable, c is the block's own, and the answer
        // is per edge: only the four edges whose far end has an
        // out-neighbor other than 4 survive, plus (2, 4) and (3, 4)
        // where 4's only friend is 5.
        let r = run(
            "MATCH (a:Person)-[:KNOWS]->(b) \
             WHERE NOT EXISTS { MATCH (b)-[:KNOWS]->(c) WHERE c.id = 4 } \
             RETURN a.id AS a, b.id AS b ORDER BY a, b",
            &[],
        );
        assert_eq!(int_rows(&r), [[0, 1], [2, 4], [3, 4], [4, 5], [5, 0]]);
    }

    #[test]
    fn exists_runs_a_two_hop_block() {
        // Two-hop trails from a that end at 0: 4 -> 5 -> 0 alone, and
        // the block rearms per outer row the same way an optional does.
        let r = run(
            "MATCH (a:Person) \
             WHERE EXISTS { MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c) WHERE c.id = 0 } \
             RETURN a.id AS id ORDER BY id",
            &[],
        );
        assert_eq!(int_rows(&r), [[4]]);
    }

    #[test]
    fn exists_sits_next_to_an_ordinary_predicate() {
        // The conjunct that is not a block stays an ordinary filter and
        // the two are anded, whichever order they are written in.
        let r = run(
            "MATCH (a:Person) WHERE a.id < 4 AND EXISTS { MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 } \
             RETURN a.id AS id ORDER BY id",
            &[],
        );
        assert_eq!(int_rows(&r), [[2], [3]]);
        let r = run(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 } AND a.id < 4 \
             RETURN a.id AS id ORDER BY id",
            &[],
        );
        assert_eq!(int_rows(&r), [[2], [3]]);
    }

    #[test]
    fn exists_and_not_exists_stack() {
        // Two blocks off one WHERE, each its own bracket: people with a
        // friend over 3 and no friend under 2. 2 -> 4, 3 -> 4 and
        // 4 -> 5 pass the first, and 4 keeps nothing under 2 either.
        let r = run(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) WHERE b.id > 3 } \
             AND NOT EXISTS { MATCH (a)-[:KNOWS]->(c) WHERE c.id < 2 } \
             RETURN a.id AS id ORDER BY id",
            &[],
        );
        assert_eq!(int_rows(&r), [[2], [3], [4]]);
    }

    #[test]
    fn exists_reads_the_pattern_alone() {
        // No block WHERE at all: the pattern is the whole question, and
        // an inline property in it is a filter inside the bracket.
        let r = run(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->({id: 4}) } \
             RETURN a.id AS id ORDER BY id",
            &[],
        );
        assert_eq!(int_rows(&r), [[2], [3]]);
    }

    #[test]
    fn exists_after_a_with_tests_the_projected_row() {
        // Degree over 1 leaves 0 and 1, and of those only 1 knows
        // someone over 2, so the block runs on the grouped rows.
        let r = run(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg WHERE deg > 1 \
             AND EXISTS { MATCH (a)-[:KNOWS]->(c) WHERE c.id > 2 } \
             RETURN a.id AS id, deg ORDER BY id",
            &[],
        );
        assert_eq!(int_rows(&r), [[1, 2]]);
    }

    #[test]
    fn exists_counts_the_outer_row_and_not_the_block() {
        // count(a) is over the outer rows the semi bracket kept, and
        // nothing the block bound is alive to be counted.
        let r = run(
            "MATCH (a:Person)-[:KNOWS]->(f) \
             WHERE EXISTS { MATCH (a)-[:IS_LOCATED_IN]->(p) WHERE p.id = 0 } \
             RETURN count(*) AS edges, count(DISTINCT a.id) AS people",
            &[],
        );
        // Places: 0 -> 0 and 3 -> 0. Edges out of them: 0 -> 1, 0 -> 2,
        // 3 -> 4.
        assert_eq!(int_rows(&r), [[3, 2]]);
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
                sorted(fac.rows.to_vec()),
                sorted(flat.rows.to_vec()),
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
