//! Binder: turns the parsed AST into a bound query with every variable
//! resolved to a slot, every label and relationship type resolved to a
//! catalog table, and every expression typed (docs/07 §2).
//!
//! The binder works against a `Schema`, a plain description of the node
//! and rel tables, rather than a storage engine catalog, so it binds
//! identically over zu1, SQLite, and S3 and tests need no file. The zu
//! facade adapts the engine catalog into a `Schema`.
//!
//! Property columns are not in the catalog yet, so property access
//! types as `Any` once the base is a node, rel, or map; the typed
//! column catalog tightens this later without changing the shape here.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use zu_common::gqlstatus::codes;
use zu_common::unicode::NormalForm;
use zu_common::{DurationKind, LogicalType, Result, ZuError};

use crate::ast::{
    self, BinaryOp, Clause, Conjunction, DeleteTarget, Expr, GraphRef, LabelExpr, Literal,
    NodePattern, PathMode, Projection, RelDirection, RelPattern, RemoveItem, Removed, Selector,
    SetInto, SetItem, SortKey, TemporalFn, TrimSide, UnaryOp,
};
use crate::functions;
use crate::refs::GraphHandle;

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// A small count written the way a sentence writes it, since a refusal
/// is read by a person and a person reading about two arguments does
/// not want the digit.
fn spelled(n: usize) -> String {
    match n {
        2 => "two".to_owned(),
        3 => "three".to_owned(),
        other => other.to_string(),
    }
}

/// A statement the surface accepts and this build does not answer yet,
/// named rather than described, in the shape the parser uses for the
/// clauses it refuses by keyword: a reader who wrote one should be told
/// which milestone they are waiting on, not sent looking for a typo.
fn not_yet(what: &str) -> ZuError {
    ZuError::gql(codes::C42001, format!("{what} is not implemented yet"))
}

/// The label bits every row of every one of these node tables carries,
/// which is what a label test may take as read. A table's own name is
/// one such bit and is usually the only one, so this is empty as soon
/// as two tables are in play.
fn guaranteed(schema: &Schema, tables: &[u32]) -> u64 {
    tables
        .iter()
        .filter_map(|id| schema.node_by_id(*id))
        .fold(u64::MAX, |mask, n| mask & 1 << n.primary_label())
}

/// `42002 syntax error or access rule violation, invalid reference`: a
/// name in the statement does not resolve, or resolves to something
/// already taken. The statement parses, so this is not 42001; it just
/// mentions something that is not there.
fn bad_reference(detail: String) -> ZuError {
    ZuError::gql(codes::C42002, detail)
}

/// A group variable a match written several ways bound, read behind
/// that match.
///
/// The ways walk different numbers of steps, so the elements the name
/// stands for are in different places in each of them, and the row that
/// leaves the fork holds one column per name rather than one per step.
/// Binding it costs nothing and reading it is what has nowhere to come
/// from, so the refusal is here and not where the pattern was written.
fn out_of_reach(name: &str) -> ZuError {
    bad_reference(format!(
        "'{name}' stands for what a repeated stretch bound, and the stretch is \
         written a number of lengths rather than one, so the elements it stands \
         for are in a different place in each of them; write the lengths as \
         statements of their own, joined with UNION, where each of them reads a \
         stretch of one length"
    ))
}

/// `22G03 data exception, invalid value type`: the expression is well
/// formed and every name in it resolves, but its type is not one this
/// position accepts.
///
/// Every type check in this file uses this, and so does the evaluator,
/// on purpose. zu decides some of these statically and some at run time
/// depending on whether the type is known from the catalog or only from
/// the value, and which side catches it is an implementation detail of
/// the plan cache. A statement has to answer with the same code either
/// way or a harness is measuring where zu happened to look rather than
/// what the standard says.
///
/// The standard has a second code with the same name, `22G12`, and the
/// artifacts give no text separating the two, so an engine that picks
/// either has not made a mistake anyone can point at.
fn bad_type(detail: String) -> ZuError {
    ZuError::gql(codes::C22G03, detail)
}

/// Refuses a clause that assigns to one property of one element twice.
///
/// `SET n.a = 1, n.a = 2` names an element that holds two values for
/// `a` afterwards, which is not an element, and the standard has a code
/// for saying so rather than an order of evaluation to lean on. The
/// whole record is the same assignment written the other way round, so
/// `SET n = {a: 1}, n.a = 2` is refused too, and a second whole record
/// is the plainest case there is.
///
/// The check is per clause. Two `SET` clauses one after the other are
/// two assignments in sequence, and the second reading what the first
/// wrote is the whole point of writing them that way.
fn once_each(verb: &str, items: &[BoundSetItem]) -> Result<()> {
    for (at, item) in items.iter().enumerate() {
        let clash = items[..at].iter().find(|before| {
            before.target == item.target
                && match (&before.into, &item.into) {
                    (BoundSetInto::Labels { .. }, _) | (_, BoundSetInto::Labels { .. }) => false,
                    (BoundSetInto::Record, _) | (_, BoundSetInto::Record) => true,
                    (BoundSetInto::Property(a), BoundSetInto::Property(b)) => a == b,
                }
        });
        let Some(clash) = clash else { continue };
        let what = match (&clash.into, &item.into) {
            (BoundSetInto::Property(key), BoundSetInto::Property(_)) => {
                format!("property '{key}' of one element twice")
            }
            _ => "one element's whole record and a property of it".to_string(),
        };
        return Err(ZuError::gql(
            codes::C22G0M,
            format!("this {verb} assigns to {what}, and an element holds one value per property"),
        ));
    }
    Ok(())
}

/// One existence block lifted out of a WHERE, with the NOT in front of
/// it folded in: `NOT EXISTS { ... }` is the same match asked the other
/// way round, and asking it the other way round is one flag rather than
/// a second operator.
struct ExistsBlock<'a> {
    negated: bool,
    patterns: &'a [ast::PathPattern],
    filter: &'a Option<Box<Expr>>,
}

/// Takes the existence blocks off the top of a WHERE and returns the
/// predicate that is left, `None` when the WHERE was nothing else.
///
/// Only the top level of the AND chain is taken, because that is where
/// a block is a decision about the row and costs nothing to make one:
/// the match runs and the row is kept or it is not. A block anywhere
/// else in the predicate is left where it is and becomes a mark, which
/// is the same match with the answer written down instead of acted on.
fn peel_exists<'a>(expr: &'a Expr, out: &mut Vec<ExistsBlock<'a>>) -> Option<Expr> {
    match expr {
        Expr::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
        } => match (peel_exists(lhs, out), peel_exists(rhs, out)) {
            (Some(lhs), Some(rhs)) => Some(Expr::Binary {
                op: BinaryOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            }),
            (Some(one), None) | (None, Some(one)) => Some(one),
            (None, None) => None,
        },
        Expr::Exists { patterns, filter } => {
            out.push(ExistsBlock {
                negated: false,
                patterns,
                filter,
            });
            None
        }
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => match expr.as_ref() {
            Expr::Exists { patterns, filter } => {
                out.push(ExistsBlock {
                    negated: true,
                    patterns,
                    filter,
                });
                None
            }
            _ => Some(Expr::Unary {
                op: UnaryOp::Not,
                expr: expr.clone(),
            }),
        },
        other => Some(other.clone()),
    }
}

/// One edge pattern of a list, as edge distinctness reads it: what it
/// was written as, what it bound, and the two ends it joins.
#[derive(Clone, Copy)]
struct Step<'a> {
    pat: &'a ast::RelPattern,
    rel: &'a BoundRel,
    ends: (usize, usize),
}

/// Whether two steps join the same pair of ends, which is what makes
/// one edge able to answer both.
///
/// A step written `-[]->` takes its ends in the order they stand and
/// one written `<-[]-` takes them the other way round, so those two
/// have to agree on which end an edge leaves and which it reaches. The
/// five spellings that read an edge either way round have no such
/// order, and one of those beside anything else agrees when the pair is
/// the same pair, `(a)~[e]~(b)` and `(b)~[f]~(a)` alike.
fn same_ends(left: &Step<'_>, right: &Step<'_>) -> bool {
    let one_way = |dir: RelDirection| matches!(dir, RelDirection::Out | RelDirection::In);
    let ordered = |step: &Step<'_>| match step.rel.direction {
        RelDirection::In => (step.ends.1, step.ends.0),
        _ => step.ends,
    };
    match one_way(left.rel.direction) && one_way(right.rel.direction) {
        true => ordered(left) == ordered(right),
        false => {
            let (a, b) = left.ends;
            let (c, d) = right.ends;
            (a, b) == (c, d) || (a, b) == (d, c)
        }
    }
}

/// Whether two edge patterns name types that have nothing in common,
/// which is the one thing a pattern says that can put an edge out of
/// reach of it whatever the graph holds.
///
/// A step naming no type reaches every edge, so it overlaps with
/// anything.
fn disjoint_types(lhs: &ast::RelPattern, rhs: &ast::RelPattern) -> bool {
    if lhs.types.is_empty() || rhs.types.is_empty() {
        return false;
    }
    !lhs.types.iter().any(|name| rhs.types.contains(name))
}

/// The test that keeps two edge patterns apart, which is an inequality
/// between what they bound when both walked one edge and a membership
/// test when one of them walked a list of edges.
///
/// Two steps that both repeat would need a test between two lists, and
/// there is no such test to write yet, so that pair answers nothing.
fn distinct_test(left: &Step<'_>, right: &Step<'_>) -> Option<BoundExpr> {
    let (left, right) = (left.rel, right.rel);
    match (left.range.is_some(), right.range.is_some()) {
        (false, false) => Some(BoundExpr::Binary {
            op: BinaryOp::Ne,
            lhs: Box::new(BoundExpr::Var(left.slot)),
            rhs: Box::new(BoundExpr::Var(right.slot)),
        }),
        (false, true) => Some(not_among(left.slot, right.slot)),
        (true, false) => Some(not_among(right.slot, left.slot)),
        (true, true) => None,
    }
}

/// `NOT (edge IN walked)`: the edge one step bound is none of the edges
/// another step walked.
fn not_among(edge: usize, walked: usize) -> BoundExpr {
    BoundExpr::Unary {
        op: UnaryOp::Not,
        expr: Box::new(BoundExpr::Binary {
            op: BinaryOp::In,
            lhs: Box::new(BoundExpr::Var(edge)),
            rhs: Box::new(BoundExpr::Var(walked)),
        }),
    }
}

/// Folds tests into a predicate that already stands there, which is how
/// a rule the language states as a rule joins one the query wrote.
fn and_all(filter: Option<BoundExpr>, tests: Vec<BoundExpr>) -> Option<BoundExpr> {
    tests.into_iter().fold(filter, |left, right| match left {
        Some(left) => Some(BoundExpr::Binary {
            op: BinaryOp::And,
            lhs: Box::new(left),
            rhs: Box::new(right),
        }),
        None => Some(right),
    })
}

/// The slots a stretch of a bound pattern walks, in the order a path
/// value holds them (docs/07 §5).
///
/// `from` and `to` are node positions, the first node of the pattern
/// being zero, so the whole pattern is `0` to the number of steps and a
/// bracket around part of it is a shorter run of the same walk. A
/// stretch of one node and no edge is a path of length zero, which is a
/// path value like any other.
fn walk(
    start: &BoundNode,
    steps: &[(BoundRel, BoundNode)],
    from: usize,
    to: usize,
) -> Vec<PathPart> {
    let node = |at: usize| match at {
        0 => start.slot,
        at => steps[at - 1].1.slot,
    };
    let mut parts = vec![PathPart::Node(node(from))];
    for (rel, node) in &steps[from..to] {
        parts.push(match rel.range.is_some() {
            true => PathPart::VarRel(rel.slot),
            false => PathPart::Rel(rel.slot),
        });
        parts.push(PathPart::Node(node.slot));
    }
    parts
}

/// The path mode a step walks under when the brackets around it named
/// one, the tightest pair of them winning.
///
/// Brackets nest, so a step may sit inside several, and the one nearest
/// it is the one that speaks about it: `WALK ((a)-[e]->(b) (TRAIL
/// (b)-[f*]->(c)))` walks the outer stretch and trails the inner. A
/// bracket that named no mode says nothing and is passed over, which
/// leaves the mode the pattern or its match mode settles.
fn subpath_mode(subpaths: &[ast::Subpath], at: usize) -> Option<PathMode> {
    subpaths
        .iter()
        .filter(|sub| sub.mode.is_some() && sub.from <= at && at < sub.to)
        .min_by_key(|sub| sub.to - sub.from)
        .and_then(|sub| sub.mode)
}

/// One node table: a label naming the row domain `0..node_count`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDef {
    pub id: u32,
    pub name: String,
    pub node_count: u64,
    /// The labels this table's rows may carry, as ids into
    /// [`Schema::labels`], the table's own name first. Every row
    /// carries that first one and any of the rest. Empty here means the
    /// caller left it to [`Schema::new`], which fills in the table's
    /// own name and nothing else.
    pub labels: Vec<u16>,
}

impl NodeDef {
    /// The label every row of this table carries, which is the table's
    /// own name.
    pub fn primary_label(&self) -> u16 {
        self.labels.first().copied().unwrap_or(0)
    }

    /// The bits a row of this table may hold. A pattern asking for a
    /// bit outside this mask cannot be satisfied here, which is what
    /// lets the binder drop the table before the plan is built.
    pub fn label_mask(&self) -> u64 {
        self.labels.iter().fold(0, |mask, &l| mask | 1 << l)
    }
}

/// One rel table: a typed CSR pair between two node tables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelDef {
    pub id: u32,
    pub name: String,
    pub from: u32,
    pub to: u32,
    pub edge_count: u64,
    /// Whether the edges here have no direction (GH02). An undirected
    /// edge is still stored once, from one end to the other, so both
    /// stored lists answer for it and only the patterns that admit an
    /// undirected edge may walk it.
    pub undirected: bool,
}

/// One rel table's COLOR summary (docs/07 §6): `counts[c]` nodes hold
/// color `c` and every triple is `(from_color, to_color, edge_count,
/// max_degree)`. The optimizer walks these as sparse matrices to keep a
/// frontier's color distribution through multi-hop chains.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColorSummary {
    pub counts: Vec<u64>,
    pub triples: Vec<(u32, u32, u64, u64)>,
    /// The committed epoch `ANALYZE` built this at, and the edges the
    /// table held then. Writes land in the table without touching the
    /// summary, so this pair is what [`ColorSummary::scale`] measures
    /// the drift against. Both zero on a file written before the stamp
    /// existed, which reads as no drift rather than as infinite drift.
    pub epoch: u64,
    pub edges: u64,
}

/// How far a rel table may move away from the COLOR summary built over
/// it before the optimizer stops steering by it at all (docs/07 §6).
///
/// A summary sits between builds while writes land under it, and COLOR
/// degrades gracefully rather than breaking: the coloring goes on
/// describing the shape of the graph long after the counts under it
/// have moved, so a scale correction carries it most of the way. That
/// only holds while the graph is still recognizably the one that was
/// colored. Past this factor it is not, and the degree histograms,
/// coarse as they are, describe the table better than a precise
/// statement about a graph that no longer exists.
pub const COLOR_DRIFT_LIMIT: f64 = 8.0;

impl ColorSummary {
    /// What this summary's edge counts have to be multiplied by to
    /// speak for a table that now holds `edges`. One when the table has
    /// not moved, when the summary carries no stamp, and when either
    /// side is empty and there is no ratio to take.
    pub fn scale(&self, edges: u64) -> f64 {
        match self.edges > 0 && edges > 0 {
            true => edges as f64 / self.edges as f64,
            false => 1.0,
        }
    }

    /// Whether the summary still describes the table closely enough to
    /// order joins by, meaning it has drifted less than
    /// [`COLOR_DRIFT_LIMIT`] in either direction.
    pub fn fresh_enough(&self, edges: u64) -> bool {
        (1.0 / COLOR_DRIFT_LIMIT..=COLOR_DRIFT_LIMIT).contains(&self.scale(edges))
    }
}

/// The lp norms of one rel table's degree sequence in one direction
/// (perf/12 §1), over the nodes that hold at least one edge. These are
/// the inputs to the LpBound-style ceilings the DP clamps its estimates
/// with; all zero means the table predates them and the DP falls back
/// to the coarser histogram ceilings.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DegreeNorms {
    /// Sum of degrees, which is the edge count.
    pub l1: f64,
    /// Square root of the sum of squared degrees.
    pub l2: f64,
    /// Cube root of the sum of cubed degrees.
    pub l3: f64,
    /// The largest degree of any one node.
    pub linf: f64,
}

impl DegreeNorms {
    /// Whether the table carries norms at all. A table loaded before
    /// they existed decodes to zeros, and so does an empty table, and
    /// both mean there is no ceiling here to work with.
    pub fn known(&self) -> bool {
        self.l1 > 0.0
    }

    /// Ceiling on the sum of the `n` largest degrees, which is the most
    /// rows an expand can produce when the side being expanded holds
    /// `n` rows whose join keys are all distinct.
    ///
    /// By Holder that sum is at most `n^(1-1/p) * lp` for every p, so
    /// the tightest of the four wins. l1 caps them all, because the
    /// whole sequence is only so big, and once `n` passes the node
    /// count l1 is the only one left saying anything.
    pub fn top_sum(&self, n: f64) -> f64 {
        let n = n.max(1.0);
        [
            self.l1,
            n.sqrt() * self.l2,
            n.powf(2.0 / 3.0) * self.l3,
            n * self.linf,
        ]
        .into_iter()
        .fold(f64::INFINITY, f64::min)
    }
}

/// Both degree sequences of one rel table plus the number that mixes
/// them, the sum over nodes of out-degree times in-degree.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DegreeStats {
    pub out: DegreeNorms,
    pub inn: DegreeNorms,
    pub cross: f64,
}

impl DegreeStats {
    /// The norms of one side: 0 is out-degree, 1 is in-degree.
    pub fn side(&self, side: usize) -> DegreeNorms {
        if side == 0 { self.out } else { self.inn }
    }

    /// Mean degree on `side` seen by rows that hold one edge each and
    /// therefore sit on a node drawn in proportion to its degree on
    /// `spread`.
    ///
    /// A row per edge means node v carries `d_spread(v) / l1` of the
    /// rows, so the mean it sees is `sum(d_spread * d_side) / l1`. When
    /// the two sides are the same that sum is `l2^2`, and when they
    /// differ it is exactly `cross`, which is the two hop path count.
    /// That is what makes a chain through a hub come out right: the hub
    /// is counted once per edge into it, not once.
    pub fn weighted(&self, spread: usize, side: usize) -> f64 {
        let top = if spread == side {
            let l2 = self.side(side).l2;
            l2 * l2
        } else {
            self.cross
        };
        top / self.out.l1
    }

    /// Whether the table carries norms at all. See [`DegreeNorms::known`].
    pub fn known(&self) -> bool {
        self.out.known() && self.inn.known()
    }
}

/// One property column's statistics (perf/12 §1): the row count, the
/// distinct count, the most frequent values, and equi-depth bucket
/// boundaries over the rest.
///
/// Values are order-preserving byte keys, [`zu_common::int_key`] for
/// an integer column and the raw bytes for a string one, so a
/// boundary, a top value, and the literal a query compares against all
/// meet as bytes and the comparison means what the query means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ColStats {
    pub rows: u64,
    pub ndv: u64,
    pub top: Vec<(Vec<u8>, u64)>,
    pub bounds: Vec<Vec<u8>>,
}

impl ColStats {
    /// Rows the top values account for.
    pub fn top_rows(&self) -> u64 {
        self.top.iter().map(|(_, n)| *n).sum()
    }

    /// Selectivity of `col = value` for a value the query knows. A hit
    /// in the top list is its own frequency; anything else shares what
    /// the top list left behind between the values it left behind,
    /// which is the uniformity assumption made where it does the least
    /// damage, over the tail after the skew has been taken out.
    pub fn eq_selectivity(&self, value: &[u8]) -> f64 {
        if self.rows == 0 {
            return 0.0;
        }
        if let Some((_, n)) = self.top.iter().find(|(v, _)| v == value) {
            return *n as f64 / self.rows as f64;
        }
        // When the top list already holds every distinct value, and
        // the literal is short enough that it would have been in the
        // list if it were in the column, the predicate matches
        // nothing. Claim one row rather than none: an estimate of zero
        // is a claim no statistic earns, and one row is already the
        // smallest thing the DP can order around.
        if self.ndv <= self.top.len() as u64 && value.len() <= VALUE_CAP {
            return 1.0 / self.rows as f64;
        }
        let rest_rows = self.rows.saturating_sub(self.top_rows()) as f64;
        let rest_ndv = self.ndv.saturating_sub(self.top.len() as u64).max(1) as f64;
        (rest_rows / rest_ndv / self.rows as f64).clamp(0.0, 1.0)
    }

    /// Selectivity of `col = ?` for a value the query does not know
    /// yet, which is every parameter: the average value's share.
    pub fn eq_average(&self) -> f64 {
        1.0 / self.ndv.max(1) as f64
    }

    /// Share of rows below `value`, read off the equi-depth buckets.
    /// A boundary is only a prefix of the value that set it, so the
    /// bucket the value falls inside counts as half rather than
    /// pretending to a precision the truncation threw away.
    pub fn below(&self, value: &[u8]) -> Option<f64> {
        let buckets = self.bounds.len().checked_sub(1)?;
        if buckets == 0 {
            return None;
        }
        if value <= self.bounds[0].as_slice() {
            return Some(0.0);
        }
        let full = self.bounds[1..]
            .iter()
            .filter(|b| b.as_slice() < value)
            .count();
        if full == buckets {
            return Some(1.0);
        }
        Some((full as f64 + 0.5) / buckets as f64)
    }
}

/// Longest value a column statistic stores. Anything longer is left
/// out of the top list rather than truncated into it, so a miss on a
/// value this long says nothing about whether the column holds it.
pub const VALUE_CAP: usize = 32;

/// The table shape the binder resolves against.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    nodes: Vec<NodeDef>,
    rels: Vec<RelDef>,
    /// Per-direction log2 degree histograms per rel table id, forward
    /// then backward, from the engine's stats blocks (docs/07 §6).
    /// Bucket `i` counts sources with degree in `[2^i, 2^(i+1))`;
    /// absent for engines that carry no statistics yet, and every
    /// estimate then falls back to the count ratios.
    degree_hists: BTreeMap<u32, [Vec<u64>; 2]>,
    /// The lp degree norms per rel table id. Absent for a table loaded
    /// before they existed, and the DP then falls back to the
    /// histogram's power-of-two ceilings.
    degree_norms: BTreeMap<u32, DegreeStats>,
    /// COLOR summaries per rel table id, present after an ANALYZE.
    color_summaries: BTreeMap<u32, ColorSummary>,
    /// Property column statistics per node table id, then column name,
    /// present for any table whose properties have been stored since
    /// perf/12 §1 landed. Absent means the estimator falls back to the
    /// fixed selectivities it used before.
    col_stats: BTreeMap<u32, BTreeMap<String, ColStats>>,
    /// The graph's label dictionary, a name at the position of its id.
    /// The ids are the engine's, because the bits the binder compiles a
    /// pattern into are read straight out of storage, so a schema built
    /// from a catalog carries that catalog's order rather than one of
    /// its own.
    labels: Vec<String>,
    /// GV60 and GE01. The graphs a graph reference expression may
    /// name, which is every graph in the catalog the statement was
    /// bound against, with the working one and the home one called out
    /// by id. Empty for a schema built without a catalog behind it,
    /// and a graph reference in a statement bound against one of those
    /// is refused rather than answered wrongly.
    graphs: Vec<GraphHandle>,
    working_graph: Option<u32>,
    home_graph: Option<u32>,
    /// How far the summed ceilings may run past the summed estimates
    /// before the join order DP reruns minimizing the ceiling instead
    /// (perf/12 §2.4). Higher trusts the estimates further, lower
    /// reaches for the robust order sooner.
    bound_disagreement: f64,
}

/// The most labels one graph holds, one bit of the word a row carries.
pub const MAX_LABELS: usize = 64;

/// The factor the ceilings have to beat the estimates by before the
/// join order DP takes the robust order, when nothing overrides it.
pub const DEFAULT_BOUND_DISAGREEMENT: f64 = 100.0;

impl Schema {
    /// Builds a schema whose only labels are the node table names, in
    /// table order, which is the graph a file written before label sets
    /// existed describes and the graph every caller that says nothing
    /// about labels means.
    pub fn new(nodes: Vec<NodeDef>, rels: Vec<RelDef>) -> Result<Self> {
        let labels = nodes.iter().map(|n| n.name.clone()).collect();
        Self::with_labels(nodes, rels, labels)
    }

    /// Builds a schema over a graph's label dictionary. A node table
    /// that declares nothing gets its own name, which is the label
    /// every one of its rows carries.
    pub fn with_labels(
        nodes: Vec<NodeDef>,
        rels: Vec<RelDef>,
        labels: Vec<String>,
    ) -> Result<Self> {
        let mut schema = Schema {
            nodes,
            rels,
            degree_hists: BTreeMap::new(),
            degree_norms: BTreeMap::new(),
            color_summaries: BTreeMap::new(),
            col_stats: BTreeMap::new(),
            labels,
            graphs: Vec::new(),
            working_graph: None,
            home_graph: None,
            bound_disagreement: DEFAULT_BOUND_DISAGREEMENT,
        };
        if schema.labels.len() > MAX_LABELS {
            return Err(invalid(format!(
                "a graph holds at most {MAX_LABELS} labels and this one holds {}",
                schema.labels.len()
            )));
        }
        for i in 0..schema.nodes.len() {
            if schema.nodes[i].labels.is_empty() {
                let own = schema.label_id(&schema.nodes[i].name).ok_or_else(|| {
                    invalid(format!(
                        "node table '{}' is not in the label dictionary",
                        schema.nodes[i].name
                    ))
                })?;
                schema.nodes[i].labels.push(own);
            }
            for &label in &schema.nodes[i].labels {
                if usize::from(label) >= schema.labels.len() {
                    return Err(invalid(format!(
                        "node table '{}' declares label {label}, which no name backs",
                        schema.nodes[i].name
                    )));
                }
            }
        }
        let mut seen = HashMap::new();
        for n in &schema.nodes {
            if seen.insert(n.name.clone(), ()).is_some() {
                return Err(invalid(format!("duplicate table name '{}'", n.name)));
            }
        }
        for r in &schema.rels {
            if seen.insert(r.name.clone(), ()).is_some() {
                return Err(invalid(format!("duplicate table name '{}'", r.name)));
            }
            if schema.node_by_id(r.from).is_none() || schema.node_by_id(r.to).is_none() {
                return Err(invalid(format!(
                    "rel table '{}' references a missing node table",
                    r.name
                )));
            }
        }
        Ok(schema)
    }

    /// Adds a node table the file does not hold, under a label of its
    /// own name.
    ///
    /// This is what a registered frame is to the binder: a table of the
    /// graph for as long as the session keeps it, named the same way
    /// every other table is named and matched by the same patterns. It
    /// is added here rather than built into the schema so that the
    /// statistics a schema was loaded with survive a registration,
    /// which is the difference between rebuilding one per `register`
    /// call and not.
    pub fn add_node_table(&mut self, mut def: NodeDef) -> Result<u16> {
        if self.nodes.iter().any(|n| n.name == def.name)
            || self.rels.iter().any(|r| r.name == def.name)
        {
            return Err(invalid(format!(
                "'{}' is already a table of this graph",
                def.name
            )));
        }
        // A name the graph already has a label under costs nothing; it
        // is only a new one that has to fit.
        let label = match self.label_id(&def.name) {
            Some(id) => id,
            None => {
                if self.labels.len() >= MAX_LABELS {
                    return Err(invalid(format!(
                        "a graph holds at most {MAX_LABELS} labels and this one already holds {}",
                        self.labels.len()
                    )));
                }
                self.labels.push(def.name.clone());
                (self.labels.len() - 1) as u16
            }
        };
        def.labels = vec![label];
        self.nodes.push(def);
        Ok(label)
    }

    pub fn nodes(&self) -> &[NodeDef] {
        &self.nodes
    }

    pub fn rels(&self) -> &[RelDef] {
        &self.rels
    }

    pub fn node_by_name(&self, name: &str) -> Option<&NodeDef> {
        self.nodes.iter().find(|n| n.name == name)
    }

    pub fn rel_by_name(&self, name: &str) -> Option<&RelDef> {
        self.rels.iter().find(|r| r.name == name)
    }

    pub fn node_by_id(&self, id: u32) -> Option<&NodeDef> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn rel_by_id(&self, id: u32) -> Option<&RelDef> {
        self.rels.iter().find(|r| r.id == id)
    }

    /// The label dictionary, a name at the position of its id.
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// The id of a label name, `None` when the graph has no such label.
    pub fn label_id(&self, name: &str) -> Option<u16> {
        self.labels.iter().position(|l| l == name).map(|i| i as u16)
    }

    /// The name behind a label id.
    pub fn label_name(&self, id: u16) -> Option<&str> {
        self.labels.get(usize::from(id)).map(String::as_str)
    }

    /// Attaches the engine's degree histograms, forward then backward
    /// per rel table id.
    pub fn set_degree_hists(&mut self, hists: BTreeMap<u32, [Vec<u64>; 2]>) {
        self.degree_hists = hists;
    }

    /// The log2 degree histograms of one rel table, forward then
    /// backward, when the engine carries statistics for it.
    pub fn degree_hist(&self, rel: u32) -> Option<&[Vec<u64>; 2]> {
        self.degree_hists.get(&rel)
    }

    /// Retunes the dual run threshold. A value that is not finite and
    /// above one is ignored, since a threshold at or below one would
    /// hand every order to the ceilings and there would be no point
    /// estimating anything.
    pub fn set_bound_disagreement(&mut self, factor: f64) {
        if factor.is_finite() && factor > 1.0 {
            self.bound_disagreement = factor;
        }
    }

    /// Tells the schema which graphs a graph reference may name, and
    /// which of them the statement is running against.
    ///
    /// The whole catalog rather than the working graph alone, because
    /// `GRAPH other` names a graph the statement is not running
    /// against, and answering it is the point: a reference is a value
    /// that says which graph, not a promise to read one.
    pub fn set_graphs(&mut self, graphs: Vec<GraphHandle>, working: u32, home: u32) {
        self.graphs = graphs;
        self.working_graph = Some(working);
        self.home_graph = Some(home);
    }

    /// The handle on the graph with this id, or `None` for an id no
    /// catalog behind this schema holds.
    fn graph_by_id(&self, id: u32) -> Option<&GraphHandle> {
        self.graphs.iter().find(|g| g.id == id)
    }

    /// How far the ceilings may run past the estimates before the join
    /// order DP reruns on the ceilings.
    pub fn bound_disagreement(&self) -> f64 {
        self.bound_disagreement
    }

    /// Attaches the engine's lp degree norms per rel table id.
    pub fn set_degree_norms(&mut self, norms: BTreeMap<u32, DegreeStats>) {
        self.degree_norms = norms;
    }

    /// The lp degree norms of one rel table, when the table was loaded
    /// since they existed.
    pub fn degree_norm(&self, rel: u32) -> Option<DegreeStats> {
        self.degree_norms
            .get(&rel)
            .copied()
            .filter(DegreeStats::known)
    }

    /// Attaches the engine's COLOR summaries per rel table id.
    pub fn set_color_summaries(&mut self, summaries: BTreeMap<u32, ColorSummary>) {
        self.color_summaries = summaries;
    }

    /// The COLOR summary of one rel table, when an ANALYZE built one.
    pub fn color_summary(&self, rel: u32) -> Option<&ColorSummary> {
        self.color_summaries.get(&rel)
    }

    /// Attaches the engine's property column statistics, per node
    /// table id then column name.
    pub fn set_col_stats(&mut self, stats: BTreeMap<u32, BTreeMap<String, ColStats>>) {
        self.col_stats = stats;
    }

    /// The statistics of one property column, when the engine carries
    /// them for it.
    pub fn col_stats(&self, table: u32, column: &str) -> Option<&ColStats> {
        self.col_stats.get(&table)?.get(column)
    }
}

/// The type lattice for bound expressions. `Any` is the unknown that
/// unifies with everything: parameters before their first use and
/// property access until the column catalog lands.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Any,
    Bool,
    Int,
    Float,
    Str,
    Node,
    Rel,
    Path,
    List(Box<Type>),
    Record,
    /// GV60, the type of a graph reference. A value of it says which
    /// graph, and that is all it says: the graph's own contents are
    /// read by the clauses that read graphs, never by holding one of
    /// these.
    Graph,
}

impl Type {
    fn is_numeric(&self) -> bool {
        matches!(self, Type::Any | Type::Int | Type::Float)
    }

    fn is_bool(&self) -> bool {
        matches!(self, Type::Any | Type::Bool)
    }

    fn is_str(&self) -> bool {
        matches!(self, Type::Any | Type::Str)
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Any => write!(f, "ANY"),
            Type::Bool => write!(f, "BOOL"),
            Type::Int => write!(f, "INT"),
            Type::Float => write!(f, "FLOAT"),
            Type::Str => write!(f, "STRING"),
            Type::Node => write!(f, "NODE"),
            Type::Rel => write!(f, "REL"),
            Type::Path => write!(f, "PATH"),
            Type::List(t) => write!(f, "LIST<{t}>"),
            Type::Record => write!(f, "RECORD"),
            Type::Graph => write!(f, "GRAPH"),
        }
    }
}

/// One bound variable. Pattern elements without a name in the query get
/// a slot too, named `#slot`, so the optimizer addresses everything the
/// same way.
#[derive(Debug, Clone, PartialEq)]
pub struct VarDef {
    pub name: String,
    pub ty: Type,
    /// Candidate node tables, narrowed by labels and rel endpoints.
    /// Empty unless `ty` is `Node`.
    pub node_tables: Vec<u32>,
    /// Candidate rel tables, narrowed by types and endpoint tables.
    /// Empty unless `ty` is `Rel` or `LIST<REL>`.
    pub rel_tables: Vec<u32>,
}

/// The whole bound query: clauses over slots, the slot table, parameter
/// names in first-use order, and the output column names.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundQuery {
    pub clauses: Vec<BoundClause>,
    pub variables: Vec<VarDef>,
    pub params: Vec<String>,
    pub columns: Vec<String>,
    /// Shape of each path variable by slot, for `RETURN p` assembly.
    pub path_shapes: BTreeMap<usize, Vec<PathPart>>,
    /// The label dictionary the query was bound against, so a label
    /// predicate can be printed with the names the query wrote rather
    /// than with the bits it compiled to.
    pub labels: Vec<String>,
    /// The operands joined onto this one at the composite level, in
    /// written order, empty for the ordinary query that is one linear
    /// statement.
    ///
    /// Each carries a bound query of its own rather than more clauses,
    /// because the operands of a set operator share nothing but their
    /// columns: a variable one of them matched is not a variable the
    /// other has, so each gets its own slot table and its own plan and
    /// the operator meets them as two tables of rows.
    pub conjoined: Vec<Conjoined>,
    /// GQ18. The value query expressions written anywhere in this
    /// query, in the order they were bound, which is the order
    /// [`BoundExpr::Scalar`] indexes them in.
    ///
    /// Each is a query of its own with its own slots, because a value
    /// query expression shares nothing with the query around it but
    /// its parameters and whatever it captures.
    pub scalars: Vec<BoundQuery>,
    /// What this query reads from the query it is written inside,
    /// empty for every query that reads nothing from one.
    ///
    /// Only a value query expression ever has any: it is the only
    /// query written inside another. An empty list is the whole test
    /// for whether it decorrelates, because a query that reads nothing
    /// from the rows around it answers the same value for all of them
    /// and is worked out once.
    pub captures: Vec<Capture>,
    /// True when what is written around this query asks only whether it
    /// answered a row, which is what `EXISTS { ... }` says (ISO 19.4).
    ///
    /// It rides here rather than beside the expression that reads it
    /// because the executor reaches these queries by the index the
    /// expression holds and has nothing else to tell them apart by.
    /// What it changes is the answer and the work: the run stops at the
    /// first row, the columns are never read, and several rows are an
    /// answer rather than the error a value query would raise.
    pub exists: bool,
}

/// One name a value query expression reads from the query around it
/// (ISO 20.6, GQ18).
///
/// The value arrives in a parameter, so the query inside reads it the
/// way it reads anything the caller passed in and nothing below the
/// binder has to know where it came from. What the executor does with
/// one of these is fill that position from the row it is standing on,
/// once per row, which is what a correlated subquery costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// The name as written, for the message and the EXPLAIN line.
    pub name: String,
    /// The slot the value is read out of, in the query around this one.
    pub slot: usize,
    /// The parameter position this query reads that value at.
    pub param: usize,
}

/// The parameter name a capture is given, which is the variable's own
/// name behind a nul byte.
///
/// No writer can name a parameter this, because no statement can hold
/// a nul, so a capture never takes a position the caller meant to fill
/// and a caller never fills one the binder meant for a capture.
fn capture_param(name: &str) -> String {
    format!("\0{name}")
}

/// Whether a parameter position is one the binder made for a capture
/// rather than one the caller is expected to fill.
pub fn is_capture_param(name: &str) -> bool {
    name.starts_with('\0')
}

impl BoundQuery {
    /// Whether this query is one linear statement, which is what every
    /// caller that walks `clauses` alone is written for.
    pub fn is_linear(&self) -> bool {
        self.conjoined.is_empty()
    }

    /// Calls `f` on this query and on every operand joined to it.
    pub fn walk(&self, f: &mut dyn FnMut(&BoundQuery)) {
        f(self);
        for joined in &self.conjoined {
            joined.query.walk(f);
        }
    }
}

/// One operand of a composite query and the conjunction that joined it
/// to what stands to its left.
#[derive(Debug, Clone, PartialEq)]
pub struct Conjoined {
    pub how: Conjunction,
    pub query: Box<BoundQuery>,
}

/// What a match does with the rows underneath it.
///
/// The first two are written as MATCH and OPTIONAL MATCH. The rest are
/// what an existence predicate becomes: `EXISTS { ... }` is a match
/// whose rows are never returned and whose only use is that there was
/// one, `NOT EXISTS { ... }` is the same match keeping the rows it
/// found nothing for, and a block that has to answer with a value
/// instead of a decision is the mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Required,
    Optional,
    /// `EXISTS { ... }`: the outer row survives if the match finds
    /// anything, once, however much it finds.
    Semi,
    /// `NOT EXISTS { ... }`: the outer row survives if it finds
    /// nothing.
    Anti,
    /// A block written where a decision about the row would be wrong,
    /// which is under an OR: every outer row survives, once, carrying
    /// whether the match found anything in the slot named here. The
    /// predicate that was written around the block then reads that
    /// slot like any other boolean. `negated` is a NOT in front of the
    /// block, folded in so the predicate reads the slot as it stands.
    Mark {
        slot: usize,
        negated: bool,
    },
}

/// The counter a `FOR ... WITH ORDINALITY` or `WITH OFFSET` binds:
/// the slot the number lands in and what the first element of a list
/// is numbered, one for ordinality and zero for offset. The two words
/// differ in nothing else, so they are one shape here and the start is
/// the whole of the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ordinal {
    pub slot: usize,
    pub start: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundClause {
    Match {
        kind: MatchKind,
        patterns: Vec<BoundPath>,
        filter: Option<BoundExpr>,
    },
    /// A match written several ways, ISO 16.7 and features G030 and
    /// G032: the walks, and what is done with the rows they answer.
    ///
    /// Each branch is a match of its own over slots of its own, and
    /// that is the point of the shape. Two branches describe the same
    /// names and not the same walk, so the tables a name may be found
    /// in are the tables one branch says or the tables the other says,
    /// and a single slot narrowed by both would be narrowed to what
    /// they agree on, which is the wrong answer and quietly so. So each
    /// branch binds its own slots, the row it answers is projected into
    /// the slots named here, and the clauses after the fork read those.
    ///
    /// The session runs this the way it runs a write, as parts with the
    /// rows carried between them, and for the same reason: what the
    /// clauses after it read is rows that several walks answered, and
    /// there is no operator in one pipeline that is several walks.
    Fork {
        branches: Vec<ForkBranch>,
        /// Whether a row two branches both answered is answered once,
        /// which is the path pattern union of feature G032, or twice,
        /// which is the multiset alternation of G030.
        distinct: bool,
        /// The slots in scope where the fork runs, which is what the
        /// branches read back and what they carry across. The names the
        /// fork itself binds follow them, in the order the branches
        /// project them.
        carry: Vec<usize>,
        /// How many of `carry` were in scope before the fork. The rest
        /// are the names the branches bound, and the split projects the
        /// first `base` of them into the branches and reads all of
        /// `carry` back out.
        base: usize,
    },
    /// `INSERT`, the elements the statement creates: the nodes first,
    /// then the edges between them, which is the order the write runs
    /// in because an edge is written between two rows that have to
    /// exist by the time it is.
    Insert {
        nodes: Vec<BoundInsertNode>,
        rels: Vec<BoundInsertRel>,
        /// The slots in scope where the write runs, in slot order.
        /// The write runs once for each row the clauses before it
        /// answered, and the clauses after it read those rows rather
        /// than reading the store again, so this is what the run
        /// carries across the write.
        carry: Vec<usize>,
    },
    /// `MERGE`, the statement that finds a pattern or writes it.
    ///
    /// The pattern is here twice, once as the walk that looks for it
    /// and once as the elements written when the walk finds nothing,
    /// and both are over the same slots. That is what makes the run
    /// simple: the probe is an optional match, so a row where the
    /// pattern's own slots came back null is a row the write runs for
    /// and every other row is one it found.
    Merge {
        /// The walk that looks for the pattern.
        probe: BoundPath,
        /// What the walk asks past the pattern itself, which is the
        /// edge distinctness the pattern's own steps imply.
        filter: Option<BoundExpr>,
        /// The elements written when the walk finds nothing, over the
        /// probe's own slots. `ON CREATE SET` is in here as well, as
        /// properties of the element it writes, because that is what
        /// it is: the element is being made, so a property it takes on
        /// creation is one of the properties it is made with.
        nodes: Vec<BoundInsertNode>,
        rels: Vec<BoundInsertRel>,
        /// `ON MATCH SET`, run over the rows the walk found.
        on_match: Vec<BoundSetItem>,
        /// The slots in scope where the write runs, the same thing
        /// [`BoundClause::Insert::carry`] holds, and then the slots the
        /// pattern writes in the order `nodes` and `rels` name them.
        /// The second run is the one that says whether the row was
        /// found, so it is carried whether or not anything reads it.
        carry: Vec<usize>,
        /// Where that second run starts, which is how many slots were
        /// in scope before the clause.
        at: usize,
    },
    /// `SET`, the assignments the statement makes to elements earlier
    /// clauses found.
    Set {
        items: Vec<BoundSetItem>,
        /// The slots in scope where the write runs, the same thing
        /// [`BoundClause::Insert::carry`] holds. A `SET` creates
        /// nothing, so the row on the other side of it is this and
        /// nothing more.
        carry: Vec<usize>,
    },
    /// `DELETE`, the elements the statement takes out of the graph, as
    /// the slots they were found in.
    Delete {
        slots: Vec<usize>,
        /// The delete items written as `VALUE { ... }`, one query each,
        /// still unbound. A nested query specification is a statement
        /// against the same graph rather than a piece of this one, so
        /// it is compiled and run on its own and the element it answers
        /// is what the item deletes.
        queries: Vec<crate::ast::Query>,
        /// The slots in scope where the write runs, the same thing
        /// [`BoundClause::Insert::carry`] holds. A `DELETE` creates
        /// nothing either, so the row on the other side of it is what
        /// came into it, with the elements it took away still named:
        /// GQL leaves a deleted element bound, and a clause after the
        /// delete that reads one gets 22G11.
        carry: Vec<usize>,
        /// Whether the edges on the element go with it, which is what
        /// `DETACH` says. Without it an element that still has edges is
        /// refused, so the flag is the difference between taking the
        /// edges away and being told they are there.
        detach: bool,
    },
    Unwind {
        expr: BoundExpr,
        slot: usize,
        ordinal: Option<Ordinal>,
    },
    /// A table function call, always the first clause: the kernel runs
    /// once over `rel` and yields one row per node of its domain.
    Call {
        func: TableFunc,
        rel: u32,
        /// The rel's node table, the type of the `node` column.
        table: u32,
        /// Arguments after the rel name: the sssp source key, and the
        /// weight column's name where the kernel takes one.
        args: Vec<BoundExpr>,
        /// One slot per YIELD column, node first.
        slots: Vec<usize>,
    },
    /// `WITH` and `RETURN` share one shape; `RETURN` is the final one.
    Project {
        distinct: bool,
        items: Vec<BoundItem>,
        order_by: Vec<SortKey<BoundExpr>>,
        skip: Option<BoundExpr>,
        limit: Option<BoundExpr>,
        filter: Option<BoundExpr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundItem {
    pub expr: BoundExpr,
    pub ty: Type,
    pub name: String,
    /// The slot a `WITH` item projects into; `None` on `RETURN`.
    pub slot: Option<usize>,
    /// True when the item contains an aggregate call; the others are
    /// the grouping keys.
    pub aggregate: bool,
}

/// One node an `INSERT` creates.
///
/// The table is settled here rather than at runtime, because a pattern
/// that says which labels an element carries says which table it goes
/// in, and a pattern that leaves that open is a pattern the writer
/// cannot answer: reading `(x)` finds every table, writing `(x)` would
/// have to pick one.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsertNode {
    /// The slot the created element binds, which is what a later clause
    /// reads when the pattern named a variable.
    pub slot: usize,
    /// The node table the element is created in.
    pub table: u32,
    /// The properties the pattern wrote, in written order.
    pub props: Vec<(String, BoundExpr)>,
}

/// One edge an `INSERT` creates.
///
/// The ends are slots rather than tables, because an edge is written
/// between two rows and a row is what a slot holds. An end is either a
/// node the same clause creates or one a `MATCH` found, and in the
/// second case the row is a different one on every row the match
/// answered, which is why the write runs once per row.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsertRel {
    /// The slot the created edge binds, which is what a later clause
    /// reads when the pattern named a variable.
    pub slot: usize,
    /// The rel table the edge is created in.
    pub table: u32,
    /// The slot holding the row the edge leaves, which is the tail of
    /// the arrow whichever way round the pattern was written.
    pub src: usize,
    /// The slot holding the row the edge arrives at.
    pub dst: usize,
    /// The properties the pattern wrote, in written order.
    pub props: Vec<(String, BoundExpr)>,
}

/// What one assignment writes: one property, every property, or the
/// labels the element carries. A `REMOVE` of labels binds to the same
/// place as a `SET` of them with the labels going the other way, for
/// the reason a `REMOVE` of a property binds to an assignment of null.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundSetInto {
    Property(String),
    Record,
    Labels {
        labels: Vec<String>,
        /// Whether the element takes the labels on or stops carrying
        /// them.
        on: bool,
    },
}

/// One assignment a `SET` makes.
///
/// The element is a slot rather than a table and a row, because which
/// element it is differs per row and the row is what says. Which column
/// the key names is settled where the write runs, for the same reason
/// an inserted element's columns are: the columns are in the file
/// rather than in the schema the binder is given. A label is settled
/// there too, because which bit it is is a question about the file's
/// label dictionary.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSetItem {
    /// The slot holding the element the assignment changes.
    pub target: usize,
    /// What the assignment writes.
    pub into: BoundSetInto,
    /// What the property takes, evaluated once per row. An item that
    /// writes labels carries a null here, because what it writes is in
    /// the statement rather than in a value.
    pub value: BoundExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundPath {
    pub slot: Option<usize>,
    pub start: BoundNode,
    pub steps: Vec<(BoundRel, BoundNode)>,
}

/// One way of matching, under [`BoundClause::Fork`].
#[derive(Debug, Clone, PartialEq)]
pub struct ForkBranch {
    pub patterns: Vec<BoundPath>,
    pub filter: Option<BoundExpr>,
    /// The slots this branch's row holds, in the order
    /// [`BoundClause::Fork::carry`] names them. The first of them are
    /// the slots the fork was given, which every branch shares, and the
    /// rest are this branch's own: the same name has a slot per branch,
    /// so the row is what makes the branches line up.
    pub slots: Vec<usize>,
}

/// One element of a path variable's shape: how the executor
/// reassembles `RETURN p` as the alternating node/rel list from the
/// pattern's slots (docs/07 §5). A var-length rel slot holds a PMR
/// chain whose interior nodes splice in between the endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathPart {
    Node(usize),
    Rel(usize),
    VarRel(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundNode {
    pub slot: usize,
    /// Inline `{key: expr}` equality predicates.
    pub props: Vec<(String, BoundExpr)>,
    /// What is left of the occurrence's label expression once the
    /// candidate tables have answered what they can, `None` when the
    /// narrowing answered the whole of it.
    pub label: Option<LabelTest>,
    /// The element pattern predicate written inside the parentheses
    /// (G041), asked of this node where the pattern reaches it. It is
    /// bound with everything to its left already in scope, so it may
    /// read the nodes and edges the pattern bound before it.
    pub filter: Option<BoundExpr>,
}

/// A compiled label expression: bit tests over the one word a row
/// carries (docs/03 §1). The bits are dictionary ids, which are a
/// property of the graph and not of any one table, so a test reads the
/// same whichever table the row came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelTest {
    /// Every bit of the mask is set. This is the conjunction the
    /// common pattern compiles to, `(n:A)` and `(n:A:B)` alike, and
    /// an empty mask is the test nothing fails.
    All(u64),
    /// GQL's `%`: the row carries a label, any label.
    Any,
    /// A name the graph's label dictionary has never held, so no row
    /// carries it and no row can. A pattern naming one matches nothing,
    /// which is an answer rather than an error: the question a label
    /// asks is which elements carry it, and none is a good answer to
    /// it. The name is kept so the plan text and a message about the
    /// pattern can still say what was asked for.
    Never(String),
    Not(Box<LabelTest>),
    And(Box<LabelTest>, Box<LabelTest>),
    Or(Box<LabelTest>, Box<LabelTest>),
}

impl LabelTest {
    /// The answer the type alone gives for a table that declares
    /// `declared` and gives every row bit `primary`, or `None` when the
    /// answer is in the row and the type cannot reach it.
    ///
    /// This is three valued logic over the two masks: `AND` is false as
    /// soon as one side is, `OR` is true as soon as one side is, and
    /// everything else that touches an unknown is unknown.
    pub fn constant(&self, declared: u64, primary: u64) -> Option<bool> {
        match self {
            // A bit the table never declares can never be set, and a
            // bit every row carries is always set.
            LabelTest::All(mask) if mask & !declared != 0 => Some(false),
            LabelTest::All(mask) if mask & !primary == 0 => Some(true),
            LabelTest::All(_) => None,
            // Every row carries its own table's label.
            LabelTest::Any => Some(true),
            // No row anywhere carries a label the graph does not have,
            // so every table answers this one the same way.
            LabelTest::Never(_) => Some(false),
            LabelTest::Not(inner) => inner.constant(declared, primary).map(|b| !b),
            LabelTest::And(lhs, rhs) => {
                match (
                    lhs.constant(declared, primary),
                    rhs.constant(declared, primary),
                ) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                }
            }
            LabelTest::Or(lhs, rhs) => {
                match (
                    lhs.constant(declared, primary),
                    rhs.constant(declared, primary),
                ) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                }
            }
        }
    }

    /// The conjunction of two tests, merging masks so the common
    /// pattern stays a single AND against the row's word.
    pub fn and(lhs: LabelTest, rhs: LabelTest) -> LabelTest {
        match (lhs, rhs) {
            (LabelTest::All(a), LabelTest::All(b)) => LabelTest::All(a | b),
            // An empty mask asks nothing, which is what a conjunct
            // pruning has emptied out has become.
            (LabelTest::All(0), other) | (other, LabelTest::All(0)) => other,
            (lhs, rhs) => LabelTest::And(Box::new(lhs), Box::new(rhs)),
        }
    }

    /// The test with the bits every candidate row is known to carry
    /// taken out of it. Dropping a bit that is always set leaves the
    /// same answer on every row those tables hold, which is what turns
    /// `(n:Person&Employee)` over the Person table into a test for
    /// Employee alone, and `(n:Person)` into no test at all.
    pub fn prune(&self, guaranteed: u64) -> LabelTest {
        match self {
            LabelTest::All(mask) => LabelTest::All(mask & !guaranteed),
            // Every node carries its own table's label, so asking
            // whether it carries one is asking nothing.
            LabelTest::Any => LabelTest::All(0),
            // Nothing a table guarantees can make a label the graph
            // does not have appear on a row.
            LabelTest::Never(name) => LabelTest::Never(name.clone()),
            LabelTest::Not(inner) => LabelTest::Not(Box::new(inner.prune(guaranteed))),
            LabelTest::And(lhs, rhs) => {
                LabelTest::and(lhs.prune(guaranteed), rhs.prune(guaranteed))
            }
            LabelTest::Or(lhs, rhs) => LabelTest::Or(
                Box::new(lhs.prune(guaranteed)),
                Box::new(rhs.prune(guaranteed)),
            ),
        }
    }

    /// Whether a row whose label word is `word` satisfies the test.
    pub fn matches(&self, word: u64) -> bool {
        match self {
            LabelTest::All(mask) => word & mask == *mask,
            LabelTest::Any => word != 0,
            LabelTest::Never(_) => false,
            LabelTest::Not(inner) => !inner.matches(word),
            LabelTest::And(lhs, rhs) => lhs.matches(word) && rhs.matches(word),
            LabelTest::Or(lhs, rhs) => lhs.matches(word) || rhs.matches(word),
        }
    }

    /// The test written the way a query would write it, for plan text
    /// and error messages. `names` is the graph's label dictionary.
    pub fn text(&self, names: &[String]) -> String {
        self.text_at(names, 0)
    }

    /// `level` is the precedence of the position this sits in, so a
    /// looser operator inside a tighter one gets its parentheses and
    /// nothing else does: 0 is the top, 1 is inside an `|`, 2 is inside
    /// an `&`, and 3 is under a `!`.
    fn text_at(&self, names: &[String], level: u8) -> String {
        let wrap = |text: String, own: u8| {
            if level > own {
                format!("({text})")
            } else {
                text
            }
        };
        match self {
            LabelTest::All(mask) => {
                let parts: Vec<&str> = (0..u64::BITS as u16)
                    .filter(|bit| mask & 1 << bit != 0)
                    .map(|bit| names.get(usize::from(bit)).map_or("?", String::as_str))
                    .collect();
                if parts.is_empty() {
                    "%".into()
                } else {
                    wrap(parts.join("&"), if parts.len() > 1 { 2 } else { 3 })
                }
            }
            LabelTest::Any => "%".into(),
            LabelTest::Never(name) => wrap(name.clone(), 3),
            LabelTest::Not(inner) => format!("!{}", inner.text_at(names, 3)),
            LabelTest::And(lhs, rhs) => wrap(
                format!("{}&{}", lhs.text_at(names, 2), rhs.text_at(names, 2)),
                2,
            ),
            LabelTest::Or(lhs, rhs) => wrap(
                format!("{}|{}", lhs.text_at(names, 1), rhs.text_at(names, 1)),
                1,
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoundRel {
    pub slot: usize,
    pub direction: RelDirection,
    pub range: Option<(Option<u64>, Option<u64>)>,
    /// The path's mode, consulted only by variable-length expansion.
    pub mode: PathMode,
    /// The path's selector, restricting a variable-length rel to
    /// minimum-hop paths.
    pub selector: Option<Selector>,
    pub props: Vec<(String, BoundExpr)>,
    /// The `WHERE` written inside the brackets, asked of every edge
    /// the step walks rather than of the path it ends up building.
    pub filter: Option<BoundExpr>,
    /// The slot that predicate reads the edge out of. On a
    /// variable-length step it is a slot of its own, holding whichever
    /// edge the walk is standing on; on a single step it is the rel's
    /// own slot, and the predicate is an ordinary filter after the
    /// expand.
    pub edge_slot: Option<usize>,
}

/// Whole-graph table functions the binder accepts in `CALL` (docs/07
/// §4). Each fixes its argument shape and YIELD columns here; the
/// engine's kernels do the work at execution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableFunc {
    Pagerank,
    Wcc,
    /// Hop levels following stored edge direction. Takes a source.
    Bfs,
    /// Hop levels over the undirected view. Takes a source.
    Sssp,
    /// Shortest paths over an edge weight column, following stored
    /// edge direction. Takes a source and the column's name.
    SsspWeighted,
    Cdlp,
    Lcc,
    /// Undirected triangles a node is a corner of. Takes nothing.
    TriangleCount,
    /// Shortest path traffic through a node, accumulated from a list of
    /// sources. Takes that list.
    Betweenness,
    Louvain,
}

impl TableFunc {
    fn resolve(name: &str) -> Option<TableFunc> {
        Some(match name.to_ascii_lowercase().as_str() {
            "pagerank" => TableFunc::Pagerank,
            "wcc" => TableFunc::Wcc,
            "bfs" => TableFunc::Bfs,
            "sssp" => TableFunc::Sssp,
            "sssp_weighted" => TableFunc::SsspWeighted,
            "cdlp" => TableFunc::Cdlp,
            "lcc" => TableFunc::Lcc,
            "triangle_count" => TableFunc::TriangleCount,
            "betweenness" => TableFunc::Betweenness,
            "louvain" => TableFunc::Louvain,
            _ => return None,
        })
    }

    /// The engine-facing kernel name.
    pub fn name(self) -> &'static str {
        match self {
            TableFunc::Pagerank => "pagerank",
            TableFunc::Wcc => "wcc",
            TableFunc::Bfs => "bfs",
            TableFunc::Sssp => "sssp",
            TableFunc::SsspWeighted => "sssp_weighted",
            TableFunc::Cdlp => "cdlp",
            TableFunc::Lcc => "lcc",
            TableFunc::TriangleCount => "triangle_count",
            TableFunc::Betweenness => "betweenness",
            TableFunc::Louvain => "louvain",
        }
    }

    /// The YIELD column after the leading `node`, with its type. The
    /// distance column is an integer that comes back null for nodes
    /// the source does not reach.
    fn value_column(self) -> (&'static str, Type) {
        match self {
            TableFunc::Pagerank => ("rank", Type::Float),
            TableFunc::Wcc => ("component", Type::Int),
            TableFunc::Bfs => ("level", Type::Int),
            TableFunc::Sssp | TableFunc::SsspWeighted => ("distance", Type::Int),
            TableFunc::Cdlp => ("community", Type::Int),
            TableFunc::Lcc => ("coefficient", Type::Float),
            TableFunc::TriangleCount => ("triangles", Type::Int),
            TableFunc::Betweenness => ("centrality", Type::Float),
            TableFunc::Louvain => ("community", Type::Int),
        }
    }
}

/// Which of the two standard deviations of GF10 a call asks for, which
/// is the whole of what tells `STDDEV_SAMP` from `STDDEV_POP`.
///
/// A population is every value there is and a sample is some of them,
/// so a sample's spread is the wider of the two: dividing by one less
/// than the count is Bessel's correction, and it is there because the
/// mean a sample is measured against is the sample's own mean and so
/// sits closer to the sample than the true mean does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Deviation {
    /// `STDDEV_SAMP`, divided by one less than the count, and null
    /// where the count is one, a single value being no sample of a
    /// spread at all.
    Sample,
    /// `STDDEV_POP`, divided by the count, and nought where the count
    /// is one.
    Population,
}

/// Which of the two percentiles of GF11 a call asks for, which is what
/// a fraction landing between two of the values is answered with.
///
/// `PERCENTILE_DISC` answers one of the values that were there, so its
/// answer has the type the values had and is a number somebody could
/// point at in the input. `PERCENTILE_CONT` answers the point the
/// fraction names on the line drawn through them, which is a float
/// whatever went in and is usually not a value that was there at all.
/// The median of two and four is three under one and two under the
/// other, and neither is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Percentile {
    /// `PERCENTILE_CONT`, which interpolates between the two values the
    /// fraction falls between and always answers a float.
    Continuous,
    /// `PERCENTILE_DISC`, which answers the first value whose share of
    /// the group reaches the fraction, with the type it had.
    Discrete,
}

/// Builtin functions the binder accepts in v0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// ISO 20.9's `COLLECT_LIST`, the set function that answers what it
    /// was given rather than a number about it. `COLLECT` is the name
    /// openCypher gives the same function and is kept as a spelling of
    /// it.
    Collect,
    /// GF10, ISO 20.9. The standard deviation of what it was given,
    /// over the sample or over the population. One arm rather than two,
    /// the two differing only in what the sum of squares is divided by,
    /// and one accumulator behind both.
    Stddev(Deviation),
    /// GF11, ISO 20.9. The one set function that takes two arguments:
    /// the values, and the fraction of the way through them to answer.
    /// The fraction is the standard's independent value expression and
    /// is the same for the whole group, which is checked rather than
    /// assumed.
    Percentile(Percentile),
    Id,
    /// G100, ISO 20.10. An identifier for an element, node or edge.
    ///
    /// What the identifier is, the standard leaves to the engine, and
    /// it lists the choice in both its implementation-defined and its
    /// implementation-dependent tables. zu answers a string rather than
    /// a number, because the one thing an identifier has to do is name
    /// one element, and a number that is the node's offset would be the
    /// same number as some edge's, so a query holding two of these
    /// could not tell whether it was holding one element twice. The
    /// spelling is written down in the conformance declaration, since a
    /// client that stores one of these is relying on it.
    ///
    /// This is not `ID`. `ID` answers a node's offset as an integer and
    /// null for an edge, which is zu's own function and older than the
    /// standard's; both stay, under the names they are asked for.
    ElementId,
    Size,
    /// GF12. The element count of a list, which is `SIZE` asked for by
    /// its other name and refused on anything that is not a list.
    Cardinality,
    /// GF04. The number of edges in a path, which is one less than the
    /// number of hops in the element list and not the element count:
    /// a two node path has three elements and a length of one.
    PathLength,
    /// ISO 20.16. The elements of a path as a list, nodes and edges
    /// alternating in the order the walk took them. It is the one way
    /// to read what a path holds, since a path is a value rather than
    /// a list and nothing else indexes into it.
    Elements,
    /// G113. Whether the elements named are all different elements.
    /// It is written like a call and reads like one, so it is one,
    /// which is what keeps the parser free of a predicate that would
    /// otherwise need a keyword of its own.
    AllDifferent,
    /// G114. The other half of the same question: whether the elements
    /// named are all the same element.
    Same,
    /// ISO 20.22. The character count of a string, which is what a
    /// reader means by its length, so a character outside the basic
    /// plane counts once rather than by the bytes it takes.
    /// CHARACTER_LENGTH is the same word written out.
    CharLength,
    /// ISO 20.22. The byte count of the same string, which is the other
    /// question and answers differently for anything but ASCII. It is
    /// what a byte string is measured by, and the only length a byte
    /// string has.
    OctetLength,
    /// ISO 20.24. The string with every character folded up.
    Upper,
    /// ISO 20.24. The string with every character folded down.
    Lower,
    /// GF05 and GF06: the trim family, under one arm for the reason the
    /// numeric library is one. Which end is trimmed and whether the
    /// characters trimmed are one character or a set of them is what
    /// [`Trim`] says, and it is a question for the registry and the
    /// kernel behind it.
    Trim(Trim),
    /// ISO 20.24, the substring function: `LEFT` and `RIGHT`, which are
    /// one family for the reason the trims are, both taking a string
    /// and a length and both raising the same condition when the length
    /// is one no string has. Which end the characters are counted from
    /// is what [`Cut`] says.
    Cut(Cut),
    /// ISO 20.27, the datetime value functions. Five cuts of the one
    /// instant the statement is running at, which is the argument the
    /// call carries, so the clock is read once per statement however
    /// many of these it holds and however many rows they answer over.
    Temporal(TemporalFn),
    /// ISO 20.28, the datetime subtraction. The kind is on the function
    /// rather than in the arguments for the reason a normal form is: it
    /// is a qualifier the statement wrote and not a value a row holds,
    /// so no row can change whether the answer counts months or
    /// nanoseconds. A call that wrote no qualifier carries DAY TO
    /// SECOND, which is what leaving it out means.
    DurationBetween(DurationKind),
    /// ISO 20.24. The string in one of the four Unicode normal forms.
    /// The form is on the function rather than in the arguments because
    /// it is a word the statement wrote and not a value a row holds, so
    /// no row can change which normalization runs.
    Normalize(NormalForm),
    /// ISO 19.7. Whether the string is already in that form. It is a
    /// function here and a predicate in the query text, which is the
    /// same arrangement `IS TYPED` has: one question, two spellings.
    IsNormalized(NormalForm),
    /// GF01, GF02 and GF03: the numeric library, under one arm rather
    /// than twenty one of them, so that a match over `Func` stays a
    /// match a reader can hold and the exhaustiveness check keeps its
    /// value. Which of the twenty one it is is a question for the
    /// registry and for the kernel behind it, and for nobody else.
    Math(Math),
}

/// ISO 20.22, the functions over one or two numbers: the
/// arithmetic set, the trigonometric set and the logarithms.
///
/// They are together because they are one family to everything outside
/// the registry: none of them reads the store, all of them answer the
/// same thing every time they are asked, and the whole of what tells
/// them apart is which arm of which kernel they take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Math {
    /// GF01. The distance from nought, exact for an exact argument.
    Abs,
    /// GF01. The nearest whole number at or above the argument.
    Ceil,
    /// GF01. The nearest whole number at or below the argument.
    Floor,
    /// GF01. The nearest whole number, halves away from nought, and to
    /// a written number of digits when a second argument says so.
    Round,
    /// GF01. Minus one, nought or one, which is exact whatever arrived.
    Sign,
    /// GF01. The square root, which is the power of one half and
    /// refuses a negative argument for that reason.
    Sqrt,
    /// GF01. One number raised to another.
    Power,
    /// GF01. The remainder of a division, which is the function spelling
    /// of the operator and raises on a nought divisor the same way.
    Mod,
    /// GF03. The exponential, e raised to the argument.
    Exp,
    /// GF03. The natural logarithm.
    Ln,
    /// GF03. The logarithm of the second argument in the base the first
    /// one gives.
    Log,
    /// GF03. The logarithm in base ten.
    Log10,
    /// GF02. The sine of an angle in radians.
    Sin,
    /// GF02. The cosine of an angle in radians.
    Cos,
    /// GF02. The tangent of an angle in radians.
    Tan,
    /// GF02. The cotangent, which is the cosine over the sine and has
    /// no answer where the sine is nought.
    Cot,
    /// GF02. The angle whose sine is the argument.
    Asin,
    /// GF02. The angle whose cosine is the argument.
    Acos,
    /// GF02. The angle whose tangent is the argument.
    Atan,
    /// GF02. Radians read as degrees.
    Degrees,
    /// GF02. Degrees read as radians.
    Radians,
}

/// ISO 20.24, the trim family: which end of a string is trimmed, and
/// whether what is trimmed is one character or a set of them.
///
/// The two questions are one enum because the answers are not
/// independent of each other in the standard. `TRIM` takes a trim
/// character, singular, and raises `22027` when it is handed anything
/// else, and trimming a set of characters is a separate feature with
/// three functions of its own, which is why an implementation can have
/// the first without the second. So each of the six is a function, and
/// the three that trim a set say so in their name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trim {
    /// `TRIM(s)` and `TRIM([BOTH] [c] FROM s)`: one character off both
    /// ends, a space when none is named.
    Both,
    /// `TRIM(LEADING c FROM s)`: the front only.
    Leading,
    /// `TRIM(TRAILING c FROM s)`: the back only.
    Trailing,
    /// GF05. `BTRIM(s, cs)`: every character of the set off both ends.
    Btrim,
    /// GF05. `LTRIM(s, cs)`: the front only.
    Ltrim,
    /// GF05. `RTRIM(s, cs)`: the back only.
    Rtrim,
}

/// ISO 20.24, the substring function: which end of a string the
/// characters are counted from.
///
/// GQL has no `SUBSTRING`. The word is reserved for a later standard to
/// use and these two are the whole of the substring function, so a
/// query that wants the middle of a string writes one inside the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// `LEFT(s, n)`: the first n characters.
    Left,
    /// `RIGHT(s, n)`: the last n characters.
    Right,
}

impl Func {
    /// Whether this is a set function, which the registry row says and
    /// nothing else does. It was a list here once and a list here is a
    /// second table: the day a set function is added and the list is
    /// not touched is the day the executor is asked for an accumulator
    /// the function has no arm for.
    pub fn is_aggregate(&self) -> bool {
        functions::row_of(*self).is_some_and(|at| functions::REGISTRY[at as usize].aggregate)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundExpr {
    Literal(Literal),
    /// GE01, a graph named where a value goes. The handle is settled
    /// while the statement is bound, because which graph the words name
    /// is a question the catalog answers once and not once per row.
    Graph(GraphHandle),
    /// GE03, a name that stands for a value inside one expression.
    ///
    /// The values are worked out in the order they are written, each
    /// into the slot beside it, and the body reads them the way it
    /// reads anything else the row holds. The slots are the binder's
    /// own and are in no chunk, which is why [`expr_slots`] takes them
    /// back out of what this expression asks the row for: the row owes
    /// this expression what it reads from outside and nothing it made
    /// itself.
    Let {
        values: Vec<(usize, BoundExpr)>,
        body: Box<BoundExpr>,
    },
    /// Index into `BoundQuery::params`.
    Param(usize),
    Var(usize),
    Property {
        base: Box<BoundExpr>,
        key: String,
    },
    Unary {
        op: UnaryOp,
        expr: Box<BoundExpr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<BoundExpr>,
        rhs: Box<BoundExpr>,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
    },
    /// `expr IS TYPED type`. The target keeps its lattice type for the
    /// same reason a cast's does: the width, the declared digits and
    /// the nullability are exactly what the predicate reads.
    IsTyped {
        expr: Box<BoundExpr>,
        ty: LogicalType,
        negated: bool,
    },
    /// The instant the statement is running at, the hidden argument
    /// every datetime value function is handed. No query writes it: the
    /// binder plants it under the five words of ISO 20.27.
    ///
    /// It is a leaf and not a literal on purpose. A literal is folded
    /// where the arguments are all literals, and folding a clock would
    /// write the compile time into the plan cache and answer every
    /// later statement with it. A leaf the run supplies is read where
    /// the run is, once per statement.
    Clock,
    Call {
        func: Func,
        /// Which row of the function registry answers this call, found
        /// while the statement is bound. The evaluator reads the row at
        /// this number rather than looking the function up again, so a
        /// call costs a jump and not a walk of the table however many
        /// rows the table grows to.
        sig: u16,
        distinct: bool,
        star: bool,
        args: Vec<BoundExpr>,
    },
    List(Vec<BoundExpr>),
    /// GE09. An aggregate over a group variable, which folds the
    /// elements one row bound rather than the rows a clause answered.
    ///
    /// It is an expression and not an aggregate: nothing groups for it,
    /// it answers one value per row, and the arguments are the row's
    /// bindings written out, so folding one reads the row and allocates
    /// nothing. Null elements are skipped, which is what an aggregate
    /// over a column does with a null row.
    Fold {
        func: Func,
        distinct: bool,
        args: Vec<BoundExpr>,
    },
    Map(Vec<(String, BoundExpr)>),
    /// GE06. The elements of a path, in the order the query wrote
    /// them. Whether they make a path is a runtime question, because
    /// whether an edge joins two nodes is not something a type knows.
    Path(Vec<BoundExpr>),
    /// `CAST(expr AS type)`. The target keeps its full lattice type
    /// rather than collapsing to [`Type`], because the width and the
    /// declared digit count are exactly what the executor checks and
    /// [`Type`] has room for neither.
    Cast {
        expr: Box<BoundExpr>,
        ty: LogicalType,
    },
    /// GE01. `CASE` in both forms, the branches in the order they were
    /// written, which is the order they are asked in: a branch is only
    /// reached when every branch above it said no, and the value it
    /// answers with is the only one evaluated.
    Case {
        /// The value the simple form compares each branch with, `None`
        /// for the searched form, which asks each branch for a truth.
        subject: Option<Box<BoundExpr>>,
        branches: Vec<(BoundExpr, BoundExpr)>,
        otherwise: Option<Box<BoundExpr>>,
    },
    /// GE01's case abbreviation: the first argument that is not null,
    /// and null where every one of them is.
    Coalesce(Vec<BoundExpr>),
    /// GE01's other case abbreviation: null where the two are equal,
    /// and the first of them otherwise.
    NullIf {
        value: Box<BoundExpr>,
        compared: Box<BoundExpr>,
    },
    /// Whether the node in `slot` satisfies a label expression, the
    /// runtime half of a label set. The binder only plants one where
    /// the candidate tables leave the answer open, so a pattern naming
    /// a table's own label compiles to no predicate at all.
    HasLabels {
        slot: usize,
        test: LabelTest,
    },
    /// G110. `expr IS [NOT] DIRECTED`. Which rel tables hold
    /// undirected edges is in the schema and the executor has none, so
    /// the binder writes the tables out here, sorted by id. Almost
    /// every graph leaves this empty, which is the answer true for
    /// every edge.
    IsDirected {
        expr: Box<BoundExpr>,
        undirected: Vec<u32>,
        negated: bool,
    },
    /// G111. `expr IS [NOT] LABELED <label expression>`.
    ///
    /// A node answers with the word its row carries, the way a pattern
    /// does. An edge carries no word: its label is the name of the
    /// table it is in, so the binder asks the expression of each rel
    /// table's name once and writes down the tables that said yes.
    IsLabeled {
        expr: Box<BoundExpr>,
        node: LabelTest,
        rels: Vec<u32>,
        negated: bool,
    },
    /// G112. `node IS [NOT] SOURCE OF edge`, and the destination twin.
    ///
    /// An edge value already holds the rows of both of its ends, so the
    /// test is one comparison once the tables agree. Which node table
    /// each end is in is the schema's answer and is written out here,
    /// as `(rel table, from table, to table)` sorted by the first.
    IsEndpoint {
        node: Box<BoundExpr>,
        rel: Box<BoundExpr>,
        end: ast::EdgeEnd,
        ends: Vec<(u32, u32, u32)>,
        negated: bool,
    },
    /// G115. `PROPERTY_EXISTS(element, name)`: whether the element
    /// carries a property of this name, which is a question about the
    /// element's table and not about the value stored there, so a
    /// property that is present and null answers true.
    PropertyExists {
        expr: Box<BoundExpr>,
        key: String,
    },
    /// GQ18. A value query expression, as an index into
    /// [`BoundQuery::scalars`].
    ///
    /// A query that reads nothing from the row this expression stands in
    /// answers the same value for every row of the run, so the executor
    /// works it out once, before the plan above it starts. That is the
    /// whole of the decorrelation: what is left here is a constant the
    /// run is handed, which is why the optimizer treats one the way it
    /// treats a parameter.
    Scalar {
        ix: usize,
        /// The slots of this query the one inside reads, which is
        /// [`BoundQuery::captures`] of `scalars[ix]` by slot and is
        /// empty for a query that decorrelated.
        ///
        /// It is written here as well because the slots an expression
        /// reads is a question asked of the expression alone, in a
        /// dozen places that have no query to hand, and a slot read
        /// per row has to be flattened like any other.
        reads: Vec<usize>,
    },
}

/// Who reads the rows a projection makes, which is the whole of the
/// difference between the three clauses that make one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Projected {
    /// A `WITH`, read by the clauses behind it in the same statement.
    Onward,
    /// A `RETURN` with a `NEXT` behind it, read by the statement the
    /// chain hands its rows to.
    Chained,
    /// The `RETURN` the query ends with, read by whoever asked.
    Answer,
}

/// Binds a parsed query against a schema.
///
/// A composite is bound operand by operand, left to right. Each gets a
/// binder of its own, because the operands share no variables: what
/// they do share is the parameter list, which is positional and belongs
/// to the statement rather than to any one operand, so it is carried
/// across and each operand's names are appended to it.
pub fn bind(query: &ast::Query, schema: &Schema) -> Result<BoundQuery> {
    let mut params = Vec::new();
    let mut bound = bind_body(&query.body, schema, &mut params, &[])?;
    // The parameter list is the statement's, so every operand's plan
    // reads the same positions the caller filled in.
    let all = params.clone();
    spread_params(&mut bound, &all);
    Ok(bound)
}

/// Binds the operands of a composite query against one parameter list.
///
/// `outer` names what is in scope in the query this one is written
/// inside, which is nothing at the top level and the names around it
/// for the query a `VALUE { ... }` carries. Nothing is bound from it:
/// it is there so that a reference to one of those names is refused by
/// saying what is wrong rather than by saying the name does not exist.
fn bind_body(
    body: &ast::Composite,
    schema: &Schema,
    params: &mut Vec<String>,
    outer: &[String],
) -> Result<BoundQuery> {
    let mut operands = Vec::new();
    collect_operands(body, &mut operands);
    let mut bound = bind_linear(operands[0].0, schema, params, outer)?;
    for (linear, how) in &operands[1..] {
        let how = how.expect("only the leftmost operand has no conjunction");
        let right = bind_linear(linear, schema, params, outer)?;
        // A statement that writes is taken apart at its write and run
        // in parts, and a part is one pipeline. An operand of a
        // composite is a second one, so the two cannot be the same
        // statement yet, and saying so beats writing half of it.
        for operand in [&bound, &right] {
            if operand.clauses.iter().any(writes) {
                return Err(invalid(
                    "a statement that writes may not be an operand of a composite query, write it on its own".into(),
                ));
            }
            // A match written several ways is run in parts for the
            // same reason a write is, so it runs into the same wall.
            // The alternatives are already a union, so writing them as
            // operands of this one is what the reader wanted anyway.
            if operand.clauses.iter().any(forks) {
                return Err(invalid(
                    "a match written several ways may not be an operand of a composite query, write each alternative as an operand of its own".into(),
                ));
            }
        }
        agree_on_columns(&bound, &right, how)?;
        bound.conjoined.push(Conjoined {
            how,
            query: Box::new(right),
        });
    }
    Ok(bound)
}

/// Gives every query under this one the statement's whole parameter
/// list, so that an operand of a composite and a value query
/// expression read the same positions the caller filled in.
fn spread_params(query: &mut BoundQuery, all: &[String]) {
    query.params = all.to_vec();
    for joined in &mut query.conjoined {
        spread_params(&mut joined.query, all);
    }
    for scalar in &mut query.scalars {
        spread_params(scalar, all);
    }
}

/// The first name this query writes that the query around it already
/// wrote, if it wrote any.
///
/// A query written inside an expression shares no slots with the one
/// around it, so a name in both is one word standing for two things.
/// Every operand of a composite is asked, since each writes names of
/// its own, and the queries written inside this one are not: their
/// names were checked against this scope when they were bound.
fn shadowed(query: &BoundQuery, outer: &[String]) -> Option<String> {
    let mut mine = query.variables.iter().map(|v| &v.name);
    if let Some(name) = mine.find(|name| outer.iter().any(|o| o == *name)) {
        return Some(name.clone());
    }
    query
        .conjoined
        .iter()
        .find_map(|joined| shadowed(&joined.query, outer))
}

/// Whether a bound clause changes the graph.
fn writes(clause: &BoundClause) -> bool {
    matches!(
        clause,
        BoundClause::Insert { .. } | BoundClause::Set { .. } | BoundClause::Delete { .. }
    )
}

/// Whether a bound clause is a match written several ways.
pub fn forks(clause: &BoundClause) -> bool {
    matches!(clause, BoundClause::Fork { .. })
}

/// The operands of a composite in written order, each beside the
/// conjunction that joined it to what stood to its left.
fn collect_operands<'a>(
    body: &'a ast::Composite,
    out: &mut Vec<(&'a ast::Linear, Option<ast::Conjunction>)>,
) {
    match body {
        ast::Composite::Linear(linear) => out.push((linear, None)),
        ast::Composite::Conjoined { left, how, right } => {
            collect_operands(left, out);
            out.push((right, Some(*how)));
        }
    }
}

/// Refuses two operands whose result tables cannot meet.
///
/// A set operator reads two tables of rows column by column, so the
/// operands have to have the same columns. The standard says the same
/// of `OTHERWISE`, and for the same reason: whichever operand answers,
/// the caller was promised one shape of answer.
fn agree_on_columns(left: &BoundQuery, right: &BoundQuery, how: ast::Conjunction) -> Result<()> {
    let word = match how {
        ast::Conjunction::Otherwise => "OTHERWISE",
        ast::Conjunction::Set { op, .. } => op.keyword(),
    };
    if left.columns.len() != right.columns.len() {
        return Err(ZuError::gql(
            codes::C42001,
            format!(
                "{word} joins two result tables, and these have {} and {} columns",
                left.columns.len(),
                right.columns.len()
            ),
        ));
    }
    for (a, b) in left.columns.iter().zip(&right.columns) {
        if a != b {
            return Err(ZuError::gql(
                codes::C42001,
                format!(
                    "{word} joins two result tables column by column, and one calls a column '{a}' where the other calls it '{b}'"
                ),
            ));
        }
    }
    Ok(())
}

/// Whether a label expression holds of an element whose whole label
/// set is one name, which is what an edge carries: the name of the
/// table it is in (G111). The label dictionary is the node tables', so
/// an edge cannot be asked with a bit test and is asked by name here
/// instead, once per table at bind time rather than once per row.
fn label_holds(expr: &ast::LabelExpr, name: &str) -> bool {
    match expr {
        ast::LabelExpr::Label(label) => label == name,
        // Every edge carries the label its table's name is.
        ast::LabelExpr::Wildcard => true,
        ast::LabelExpr::Not(inner) => !label_holds(inner, name),
        ast::LabelExpr::And(lhs, rhs) => label_holds(lhs, name) && label_holds(rhs, name),
        ast::LabelExpr::Or(lhs, rhs) => label_holds(lhs, name) || label_holds(rhs, name),
    }
}

/// One end of a merged pattern, as the slot holding it, adding the
/// element to `nodes` when it is one the statement writes.
///
/// An end is one of three things. A name an earlier clause bound is one
/// the pattern is pointing at, and a name this pattern already used is
/// the same element again; neither is written, and neither may carry a
/// label or a property, because that would be describing an element
/// that has already been described. Everything else is an element the
/// pattern describes, which is one to write when the walk finds nothing.
///
/// This is a free function rather than a method because it holds a
/// borrow of the list it is adding to for as long as it runs.
fn merge_end(
    binder: &Binder<'_>,
    pat: &NodePattern,
    bound: &BoundNode,
    carry: &[usize],
    nodes: &mut Vec<BoundInsertNode>,
) -> Result<usize> {
    let again = carry.contains(&bound.slot) || nodes.iter().any(|node| node.slot == bound.slot);
    if again {
        if pat.label.is_some() || !pat.props.is_empty() {
            return Err(invalid(format!(
                "'{}' already stands for an element here, so writing a label or a property on it would be describing an element that is already described",
                pat.var.as_deref().unwrap_or("")
            )));
        }
        return Ok(bound.slot);
    }
    let table = binder.insert_table(pat, "MERGE")?;
    nodes.push(BoundInsertNode {
        slot: bound.slot,
        table,
        props: bound.props.clone(),
    });
    Ok(bound.slot)
}

/// The properties of whichever written element a slot names. The slot
/// has been checked against the list of what the clause writes, so one
/// of the two holds it.
fn merge_props<'a>(
    nodes: &'a mut [BoundInsertNode],
    rels: &'a mut [BoundInsertRel],
    slot: usize,
) -> &'a mut Vec<(String, BoundExpr)> {
    if let Some(node) = nodes.iter_mut().find(|node| node.slot == slot) {
        return &mut node.props;
    }
    rels.iter_mut()
        .find(|rel| rel.slot == slot)
        .map(|rel| &mut rel.props)
        .expect("the slot was checked against what the clause writes")
}

/// Every slot a bound expression reads.
///
/// The set is whatever the caller keeps them in, since the executor
/// wants them in slot order and the optimizer only asks whether one is
/// in there. It lives here because it is a fact about `BoundExpr` and
/// three places need it; two of them used to hold a copy each, which is
/// two places to forget when a variant is added.
pub(crate) fn expr_slots(expr: &BoundExpr, out: &mut impl Extend<usize>) {
    match expr {
        BoundExpr::Literal(_) | BoundExpr::Param(_) | BoundExpr::Graph(_) | BoundExpr::Clock => {}
        // A value query expression reads the slots the query inside it
        // captured, and nothing at all when it captured none, which is
        // what makes that one a single value for the whole run.
        BoundExpr::Scalar { reads, .. } => out.extend(reads.iter().copied()),
        BoundExpr::Var(slot) | BoundExpr::HasLabels { slot, .. } => out.extend([*slot]),
        BoundExpr::Property { base, .. } => expr_slots(base, out),
        BoundExpr::Unary { expr, .. } => expr_slots(expr, out),
        BoundExpr::Binary { lhs, rhs, .. } => {
            expr_slots(lhs, out);
            expr_slots(rhs, out);
        }
        BoundExpr::IsNull { expr, .. } => expr_slots(expr, out),
        BoundExpr::IsTyped { expr, .. } => expr_slots(expr, out),
        BoundExpr::Call { args, .. } | BoundExpr::Fold { args, .. } => {
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
            for (_, value) in pairs {
                expr_slots(value, out);
            }
        }
        BoundExpr::Path(elements) => {
            for element in elements {
                expr_slots(element, out);
            }
        }
        // GE03. The row supplies what the parts read less the names
        // this expression made, since those are written and read
        // inside it and no operator under it has ever heard of them.
        BoundExpr::Let { values, body } => {
            let mut read = Vec::new();
            for (_, value) in values {
                expr_slots(value, &mut read);
            }
            expr_slots(body, &mut read);
            out.extend(
                read.into_iter()
                    .filter(|slot| !values.iter().any(|(made, _)| made == slot)),
            );
        }
        BoundExpr::Cast { expr, .. } => expr_slots(expr, out),
        BoundExpr::Case {
            subject,
            branches,
            otherwise,
        } => {
            for expr in subject.iter().chain(otherwise.iter()) {
                expr_slots(expr, out);
            }
            for (when, then) in branches {
                expr_slots(when, out);
                expr_slots(then, out);
            }
        }
        BoundExpr::Coalesce(args) => {
            for arg in args {
                expr_slots(arg, out);
            }
        }
        BoundExpr::NullIf { value, compared } => {
            expr_slots(value, out);
            expr_slots(compared, out);
        }
        BoundExpr::IsDirected { expr, .. }
        | BoundExpr::IsLabeled { expr, .. }
        | BoundExpr::PropertyExists { expr, .. } => expr_slots(expr, out),
        BoundExpr::IsEndpoint { node, rel, .. } => {
            expr_slots(node, out);
            expr_slots(rel, out);
        }
    }
}

/// Binds one linear query statement, appending whatever parameters it
/// names to `params` so the positions stay the statement's.
fn bind_linear(
    linear: &ast::Linear,
    schema: &Schema,
    params: &mut Vec<String>,
    outer: &[String],
) -> Result<BoundQuery> {
    let mut binder = Binder {
        schema,
        variables: Vec::new(),
        scope: HashMap::new(),
        params: std::mem::take(params),
        columns: Vec::new(),
        path_shapes: BTreeMap::new(),
        pending: Vec::new(),
        marks: None,
        scalars: Vec::new(),
        outer: outer.to_vec(),
        captures: Vec::new(),
        groups: HashMap::new(),
        forked: BTreeSet::new(),
    };
    let mut clauses = Vec::new();
    // Where the clause stands in the whole query rather than in the
    // statement it was written in, because CALL runs over the graph and
    // a NEXT in front of it does not change that.
    let mut written = 0;
    for (n, simple) in linear.statements.iter().enumerate() {
        let last = n + 1 == linear.statements.len();
        for clause in &simple.clauses {
            if written > 0 && matches!(clause, Clause::Call { .. }) {
                return Err(invalid(
                    "CALL must be the first clause, table functions run once over the whole graph"
                        .into(),
                ));
            }
            written += 1;
            clauses.push(binder.bind_clause(clause)?);
            // An existence block written in the clause's WHERE is a
            // match of its own and runs where the predicate would have,
            // which is straight after the clause it was written in.
            clauses.append(&mut binder.pending);
        }
        // A result statement in the middle of a chain is what the next
        // statement reads and not what the caller gets back, so it
        // binds as a projection that keeps its slots. Only the last one
        // names the columns of the answer.
        if let Some(projection) = &simple.result {
            written += 1;
            let role = if last {
                Projected::Answer
            } else {
                Projected::Chained
            };
            clauses.push(binder.bind_projection(projection, role, &None)?);
            clauses.append(&mut binder.pending);
        }
    }
    *params = binder.params.clone();
    Ok(BoundQuery {
        clauses,
        variables: binder.variables,
        params: binder.params,
        columns: binder.columns,
        path_shapes: binder.path_shapes,
        labels: schema.labels.clone(),
        conjoined: Vec::new(),
        scalars: binder.scalars,
        captures: binder.captures,
        exists: false,
    })
}

struct Binder<'a> {
    schema: &'a Schema,
    variables: Vec<VarDef>,
    /// Name to slot for everything visible right now. `WITH` replaces
    /// it wholesale; slots stay in `variables` either way.
    scope: HashMap<String, usize>,
    params: Vec<String>,
    columns: Vec<String>,
    path_shapes: BTreeMap<usize, Vec<PathPart>>,
    /// Matches lifted out of the clause being bound, in written order.
    /// Drained by [`bind`] once the clause itself is bound, so an
    /// existence block lands where its predicate would have.
    pending: Vec<BoundClause>,
    /// The mark matches the predicate being bound has asked for, and
    /// whether it may ask at all. `None` is every place a mark cannot
    /// go, which is anywhere the predicate is not a WHERE standing
    /// straight over the pattern its block reads.
    marks: Option<Vec<BoundClause>>,
    /// The value query expressions bound so far, which become
    /// [`BoundQuery::scalars`].
    scalars: Vec<BoundQuery>,
    /// The names in scope in the query this one is written inside,
    /// empty for a query nothing is written inside. A reference to one
    /// of these is what makes a value query expression correlated, so
    /// reading one is not an undefined name: it is a name that arrives
    /// per row.
    outer: Vec<String>,
    /// The names read out of `outer` so far, which become
    /// [`BoundQuery::captures`]. The slot in each is filled in by the
    /// binder around this one, which is the one that has it.
    captures: Vec<Capture>,
    /// The group variables the patterns of the clause being bound wrote
    /// (ISO 16.11, feature GQ17), by name.
    ///
    /// A group variable is not in `scope`, because it is not a slot: a
    /// stretch repeated n times binds its names n times and what the
    /// name stands for is those n bindings as a list. So it is kept
    /// here, as the slots it gathers, and reading the name builds the
    /// list out of the row the walk already filled. A query that never
    /// reads the name costs nothing for it.
    groups: HashMap<String, GroupVar>,
    /// The group variables a match written several ways wrote, which
    /// are out of reach behind it.
    ///
    /// A group is the slots of the walk that bound it, and the ways of
    /// such a match walk differently, so there is no one list of slots
    /// to hand the clauses behind the fork. The name is written down
    /// here instead of being kept, and reading it is refused with that
    /// as the reason. A query that never reads the name is answered,
    /// because a binding nothing reads is a binding nothing can tell
    /// apart from any other.
    forked: BTreeSet<String>,
}

/// A group variable: what the elements are and where the row holds them.
#[derive(Debug, Clone)]
struct GroupVar {
    /// The type of one element, which is what says whether the group is
    /// a list of nodes or a list of edges.
    element: Type,
    slots: Vec<usize>,
}

/// Expression context: where aggregates are legal and whether one was
/// seen, so projections can split grouping keys from aggregates.
struct ExprCtx {
    allow_aggregates: bool,
    in_aggregate: bool,
    saw_aggregate: bool,
}

impl ExprCtx {
    fn new(allow_aggregates: bool) -> Self {
        ExprCtx {
            allow_aggregates,
            in_aggregate: false,
            saw_aggregate: false,
        }
    }
}

impl Binder<'_> {
    fn new_slot(&mut self, name: String, ty: Type) -> usize {
        let slot = self.variables.len();
        self.variables.push(VarDef {
            name,
            ty,
            node_tables: Vec::new(),
            rel_tables: Vec::new(),
        });
        slot
    }

    fn anon_slot(&mut self, ty: Type) -> usize {
        let name = format!("#{}", self.variables.len());
        self.new_slot(name, ty)
    }

    fn declare(&mut self, name: &str, ty: Type) -> Result<usize> {
        if self.scope.contains_key(name) {
            return Err(bad_reference(format!(
                "variable '{name}' is already defined"
            )));
        }
        let slot = self.new_slot(name.to_string(), ty);
        self.scope.insert(name.to_string(), slot);
        Ok(slot)
    }

    /// Binds one way of matching: the patterns of a list, and the
    /// conditions that decide the match.
    fn bind_way(
        &mut self,
        optional: bool,
        patterns: &[ast::PathPattern],
        filter: &Option<Expr>,
    ) -> Result<(Vec<BoundPath>, Option<BoundExpr>)> {
        let mut bound = Vec::new();
        for path in patterns {
            bound.push(self.bind_path(path)?);
        }
        for path in &bound {
            self.narrow_path(path)?;
        }
        for path in &mut bound {
            self.settle_labels(path);
        }
        // A mark is a match of its own that has to run before the
        // predicate reading it, and an OPTIONAL MATCH is one bracket
        // already: its WHERE decides the match rather than filtering
        // behind it, so a mark there would have to run inside that
        // bracket and refuses instead.
        let mut filter = self.bind_where(filter, !optional)?;
        // A condition written inside a pattern's brackets decides the
        // match the way the clause's own WHERE does, so the two fold
        // together rather than one running behind the other. It is
        // bound after every pattern of the list, because a condition
        // inside one bracket may read a name another pattern wrote.
        for path in patterns {
            let inner = self.bind_where(&path.filter, !optional)?;
            filter = and_all(filter, inner.into_iter().collect());
        }
        let filter = and_all(filter, self.edge_distinctness(patterns, &bound));
        Ok((bound, filter))
    }

    /// Binds a match written several ways (ISO 16.7, features G030 and
    /// G032).
    ///
    /// Each way binds against the scope the fork started in, so a name
    /// two ways both write has a slot in each of them and neither way
    /// narrows the other's. What the ways share is the names, and the
    /// standard asks for that: a variable one alternative binds and the
    /// other does not would be a variable the clauses after the fork
    /// could only sometimes read, so a fork whose ways disagree about
    /// what they bind is refused here rather than answered with nulls.
    ///
    /// The names the fork binds are given slots of their own, one per
    /// name however many ways there are, and what a slot may hold is
    /// what any way could put in it: the candidate tables are the union
    /// across the ways, because a row from either of them arrives in
    /// the same place.
    fn bind_fork(
        &mut self,
        optional: bool,
        patterns: &[ast::PathPattern],
        alts: &[Vec<ast::PathPattern>],
        distinct: bool,
        filter: &Option<Expr>,
    ) -> Result<BoundClause> {
        if optional {
            return Err(ZuError::gql(
                codes::C42001,
                "an OPTIONAL MATCH answers a row of nulls where it found nothing, and a \
                 match written several ways would have one such row per way; write the \
                 alternatives as separate statements joined with UNION",
            ));
        }
        let base_scope = self.scope.clone();
        let base_vars = self.variables.clone();
        let base_slots = self.carried();
        let base_groups: Vec<String> = self.groups.keys().cloned().collect();
        let mut branches: Vec<ForkBranch> = Vec::new();
        // The names the first way bound, in the order it bound them,
        // which is the order every way projects them in.
        let mut names: Vec<String> = Vec::new();
        for (at, way) in std::iter::once(patterns)
            .chain(alts.iter().map(Vec::as_slice))
            .enumerate()
        {
            if at > 0 {
                self.scope = base_scope.clone();
                // A slot the fork's own ways narrowed is narrowed to
                // what that way needs, and the next way is a different
                // walk over the same graph. Only the slots that were
                // there before the fork are put back: the ones a way
                // made are its own and stay as it left them.
                self.variables[..base_vars.len()].clone_from_slice(&base_vars);
            }
            let pending = self.pending.len();
            let (bound, filter) = self.bind_way(false, way, filter)?;
            // Two things a way may write that the row between the
            // parts has nowhere to put. A path variable is assembled
            // from the slots of the walk that bound it and the ways
            // walk differently, and an existence block is a match of
            // its own that would be lifted once per way and run that
            // many times. Each is refused by name, because a reader who
            // wrote one is owed the reason rather than a wrong count.
            if bound.iter().any(|path| path.slot.is_some()) {
                return Err(ZuError::gql(
                    codes::C42001,
                    "a path variable is put together out of the slots of the walk that \
                     bound it, and the alternatives of a match walk differently; name \
                     the path in each alternative as a statement of its own, joined \
                     with UNION",
                ));
            }
            // A group a way wrote is put out of reach rather than
            // carried. What the name stands for is the slots of the
            // walk that bound it, the ways walk different numbers of
            // steps, and the row between the parts holds one column per
            // name and not one per step. A way that wrote the name is
            // still a way that matched, so the walk stands and only the
            // reading of the name is refused.
            let wrote_groups: Vec<String> = self
                .groups
                .keys()
                .filter(|name| !base_groups.contains(name))
                .cloned()
                .collect();
            for name in wrote_groups {
                self.groups.remove(&name);
                self.forked.insert(name);
            }
            if self.pending.len() != pending {
                return Err(ZuError::gql(
                    codes::C42001,
                    "an existence block under a match written several ways is a match of \
                     its own that would run once per alternative; write the alternatives \
                     as statements of their own, joined with UNION",
                ));
            }
            let mut wrote: Vec<(String, usize)> = self
                .scope
                .iter()
                .filter(|(name, _)| !base_scope.contains_key(*name))
                .map(|(name, slot)| (name.clone(), *slot))
                .collect();
            wrote.sort_by_key(|(_, slot)| *slot);
            if at == 0 {
                names = wrote.iter().map(|(name, _)| name.clone()).collect();
            }
            let mut slots = base_slots.clone();
            for name in &names {
                let Some((_, slot)) = wrote.iter().find(|(wrote, _)| wrote == name) else {
                    return Err(ZuError::gql(
                        codes::C42001,
                        format!(
                            "one way of matching binds '{name}' and another does not, so \
                             the clauses after them could only sometimes read it; every \
                             alternative binds the same names"
                        ),
                    ));
                };
                slots.push(*slot);
            }
            if slots.len() != base_slots.len() + wrote.len() {
                let extra = wrote
                    .iter()
                    .map(|(name, _)| name)
                    .find(|name| !names.contains(name))
                    .expect("a way that bound more names than the first bound one of them");
                return Err(ZuError::gql(
                    codes::C42001,
                    format!(
                        "one way of matching binds '{extra}' and another does not, so the \
                         clauses after them could only sometimes read it; every \
                         alternative binds the same names"
                    ),
                ));
            }
            branches.push(ForkBranch {
                patterns: bound,
                filter,
                slots,
            });
        }
        // The slots the fork answers in. A name's slot is new, because
        // what it holds is a row from any of the ways, and what it may
        // hold is what any of them could have put there.
        self.scope = base_scope;
        self.variables[..base_vars.len()].clone_from_slice(&base_vars);
        let mut carry = base_slots.clone();
        for (at, name) in names.iter().enumerate() {
            let mut def = self.variables[branches[0].slots[base_slots.len() + at]].clone();
            def.name = name.clone();
            for branch in &branches[1..] {
                let other = &self.variables[branch.slots[base_slots.len() + at]];
                if other.ty != def.ty {
                    return Err(ZuError::gql(
                        codes::C42001,
                        format!(
                            "one way of matching binds '{name}' as {} and another as {}, \
                             and a name stands for one kind of thing",
                            def.ty, other.ty
                        ),
                    ));
                }
                def.node_tables.extend(other.node_tables.iter().copied());
                def.rel_tables.extend(other.rel_tables.iter().copied());
            }
            def.node_tables.sort_unstable();
            def.node_tables.dedup();
            def.rel_tables.sort_unstable();
            def.rel_tables.dedup();
            let slot = self.variables.len();
            self.variables.push(def);
            self.scope.insert(name.clone(), slot);
            carry.push(slot);
        }
        Ok(BoundClause::Fork {
            branches,
            distinct,
            carry,
            base: base_slots.len(),
        })
    }

    fn bind_clause(&mut self, clause: &Clause) -> Result<BoundClause> {
        match clause {
            Clause::Match {
                optional,
                patterns,
                alts,
                distinct,
                filter,
            } => {
                if !alts.is_empty() {
                    return self.bind_fork(*optional, patterns, alts, *distinct, filter);
                }
                let (bound, filter) = self.bind_way(*optional, patterns, filter)?;
                Ok(BoundClause::Match {
                    kind: match optional {
                        true => MatchKind::Optional,
                        false => MatchKind::Required,
                    },
                    patterns: bound,
                    filter,
                })
            }
            Clause::Insert { patterns } => {
                // Read before the clause writes anything into scope,
                // so that what the run carries across the write is the
                // rows the clauses before it answered and nothing the
                // write itself made.
                let carry = self.carried();
                let mut nodes = Vec::new();
                let mut rels = Vec::new();
                for path in patterns {
                    if path.var.is_some() {
                        return Err(not_yet(
                            "INSERT binding the path it wrote, which is a walk over rows that are being made rather than found,",
                        ));
                    }
                    if path.selector.is_some() || path.mode.is_some() {
                        return Err(invalid(
                            "a selector or a path mode says which of the walks that are there to pick, and INSERT is making one rather than picking one".into(),
                        ));
                    }
                    if !path.subpaths.is_empty() || path.filter.is_some() || !path.groups.is_empty()
                    {
                        return Err(invalid(
                            "brackets around part of a pattern name a stretch of a walk or say something that has to hold of one, and INSERT is writing the elements rather than walking them".into(),
                        ));
                    }
                    let mut left = self.bind_insert_end(&path.start, &mut nodes)?;
                    for (rel, node) in &path.steps {
                        let right = self.bind_insert_end(node, &mut nodes)?;
                        rels.push(self.bind_insert_rel(rel, left, right, &nodes)?);
                        left = right;
                    }
                }
                Ok(BoundClause::Insert { nodes, rels, carry })
            }
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => self.bind_merge(pattern, on_create, on_match),
            Clause::Set { items } => {
                let carry = self.carried();
                let mut bound = Vec::with_capacity(items.len());
                for item in items {
                    bound.push(self.bind_set_item(item)?);
                }
                once_each("SET", &bound)?;
                Ok(BoundClause::Set {
                    items: bound,
                    carry,
                })
            }
            Clause::Remove { items } => {
                let carry = self.carried();
                let mut bound = Vec::with_capacity(items.len());
                for item in items {
                    bound.push(self.bind_remove_item(item)?);
                }
                once_each("REMOVE", &bound)?;
                Ok(BoundClause::Set {
                    items: bound,
                    carry,
                })
            }
            Clause::Delete { targets, detach } => {
                let carry = self.carried();
                let mut slots = Vec::new();
                let mut queries = Vec::new();
                for target in targets {
                    match target {
                        DeleteTarget::Variable(name) => {
                            slots.push(self.bind_delete_target(name)?);
                        }
                        // A nested query specification is a statement of
                        // its own against the same graph, so it is not
                        // bound in this scope and does not see the
                        // variables around it. It is carried through as
                        // written and compiled where it runs.
                        DeleteTarget::Value(nested) => queries.push((**nested).clone()),
                    }
                }
                Ok(BoundClause::Delete {
                    slots,
                    queries,
                    carry,
                    detach: *detach,
                })
            }
            Clause::Unwind {
                expr,
                alias,
                ordinal,
            } => {
                let mut ctx = ExprCtx::new(false);
                let (bound, ty) = self.bind_expr(expr, &mut ctx)?;
                let element = match ty {
                    Type::List(inner) => *inner,
                    Type::Any => Type::Any,
                    other => {
                        return Err(invalid(format!(
                            "UNWIND needs a list, got {other} from {}",
                            text(expr)
                        )));
                    }
                };
                let slot = self.declare(alias, element)?;
                // The counter is declared after the value, so a FOR
                // that names the same thing twice is refused by the
                // rule that refuses any redefinition rather than by a
                // rule of its own, and it is an integer because it
                // counts.
                let ordinal = match ordinal {
                    Some(ast::Ordinal { name, start }) => Some(Ordinal {
                        slot: self.declare(name, Type::Int)?,
                        start: *start,
                    }),
                    None => None,
                };
                Ok(BoundClause::Unwind {
                    expr: bound,
                    slot,
                    ordinal,
                })
            }
            Clause::Call { name, args, yields } => self.bind_table_call(name, args, yields),
            // A FILTER is the WHERE of a MATCH with no pattern under
            // it, which is a shape the binder already has: a mark's
            // predicate is queued as exactly this. Binding it to that
            // rather than to a clause of its own is not a shortcut, it
            // is what the statement is, and it means every executor
            // and the optimizer's filter handling take it as they
            // stand.
            Clause::Filter { expr } => {
                let filter = self.bind_where(&Some(expr.clone()), true)?;
                Ok(BoundClause::Match {
                    kind: MatchKind::Required,
                    patterns: Vec::new(),
                    filter,
                })
            }
            Clause::Let { items } => self.bind_let(items),
            Clause::Yield { items } => self.bind_yield(items),
            Clause::With { projection, filter } => {
                self.bind_projection(projection, Projected::Onward, filter)
            }
            Clause::Order { keys, skip, limit } => self.bind_order(keys, skip, limit),
            Clause::Finish => self.bind_finish(),
        }
    }

    /// Binds a WHERE, the existence blocks in it lifted into matches of
    /// their own first.
    ///
    /// A block is a match and not a value, so it cannot be bound where
    /// it was written: what comes back here is the predicate that is
    /// left, and the blocks are queued in `pending` to run after the
    /// clause. A block at the top of the AND chain is lifted whole,
    /// since keeping the row is all its answer is used for. One written
    /// anywhere else in the predicate becomes a mark when `marks_ok`,
    /// and the predicate around it then has to run after the mark
    /// rather than with the clause, so it is queued too, as a filter of
    /// its own straight behind the matches that answer it.
    fn bind_where(&mut self, filter: &Option<Expr>, marks_ok: bool) -> Result<Option<BoundExpr>> {
        let Some(expr) = filter else {
            return Ok(None);
        };
        let mut blocks = Vec::new();
        let rest = peel_exists(expr, &mut blocks);
        for block in blocks {
            let clause = self.bind_exists(block, None)?;
            self.pending.push(clause);
        }
        let Some(expr) = rest else {
            return Ok(None);
        };
        let held = std::mem::replace(&mut self.marks, marks_ok.then(Vec::new));
        let bound = self.bind_bool(&expr, "WHERE");
        let marks = std::mem::replace(&mut self.marks, held);
        let bound = bound?;
        match marks {
            Some(marks) if !marks.is_empty() => {
                self.pending.extend(marks);
                self.pending.push(BoundClause::Match {
                    kind: MatchKind::Required,
                    patterns: Vec::new(),
                    filter: Some(bound),
                });
                Ok(None)
            }
            _ => Ok(Some(bound)),
        }
    }

    /// Binds one existence block into a mark match and hands back the
    /// variable that reads its answer.
    ///
    /// `negated` folds a NOT in front of the block into the match, the
    /// same fold [`peel_exists`] does for a lifted one, so a predicate
    /// never has to negate the mark it reads.
    fn bind_mark(
        &mut self,
        patterns: &[ast::PathPattern],
        filter: &Option<Box<Expr>>,
        negated: bool,
    ) -> Result<BoundExpr> {
        if self.marks.is_none() {
            return Err(invalid(
                "EXISTS is a match and not a value: write it in the WHERE of a MATCH, as a \
                 whole conjunct or under an OR there"
                    .into(),
            ));
        }
        let slot = self.anon_slot(Type::Bool);
        let clause = self.bind_exists(
            ExistsBlock {
                negated,
                patterns,
                filter,
            },
            Some(slot),
        )?;
        self.marks
            .as_mut()
            .expect("the mark sink was there a moment ago")
            .push(clause);
        Ok(BoundExpr::Var(slot))
    }

    /// Binds one existence block into the match it stands for, as a
    /// decision about the outer row or, with a slot, as the mark that
    /// writes the same answer down instead.
    ///
    /// The block sees the scope around it, which is what ties it to the
    /// row being tested, and the names it writes itself are gone again
    /// when it ends: an EXISTS says whether a match was there and hands
    /// back nothing to read, so a variable of its own that outlived it
    /// would name a row nobody kept.
    ///
    /// The block's own WHERE takes no marks. One there would be a match
    /// run per row of a match whose rows nothing keeps, so it refuses
    /// where it stands.
    fn bind_exists(&mut self, block: ExistsBlock, mark: Option<usize>) -> Result<BoundClause> {
        let outer = self.scope.clone();
        // A group the block's own patterns write is the block's, the
        // same as a name they write, so it goes out of scope with them.
        let held_groups = self.groups.clone();
        let mut bound = Vec::new();
        for path in block.patterns {
            bound.push(self.bind_path(path)?);
        }
        for path in &bound {
            self.narrow_path(path)?;
        }
        for path in &mut bound {
            self.settle_labels(path);
        }
        let held = self.marks.take();
        let mut filter = match &block.filter {
            Some(expr) => Some(self.bind_bool(expr, "WHERE")?),
            None => None,
        };
        for path in block.patterns {
            let Some(expr) = &path.filter else { continue };
            let inner = self.bind_bool(expr, "WHERE")?;
            filter = and_all(filter, vec![inner]);
        }
        let filter = and_all(filter, self.edge_distinctness(block.patterns, &bound));
        self.marks = held;
        self.scope = outer;
        self.groups = held_groups;
        Ok(BoundClause::Match {
            kind: match (mark, block.negated) {
                (Some(slot), negated) => MatchKind::Mark { slot, negated },
                (None, true) => MatchKind::Anti,
                (None, false) => MatchKind::Semi,
            },
            patterns: bound,
            filter,
        })
    }

    fn bind_table_call(
        &mut self,
        name: &str,
        args: &[Expr],
        yields: &[(String, Option<String>)],
    ) -> Result<BoundClause> {
        let func = TableFunc::resolve(name).ok_or_else(|| {
            invalid(format!(
                "unknown table function '{name}', the v0 functions are \
                 pagerank, wcc, bfs, sssp, sssp_weighted, cdlp, lcc, \
                 triangle_count, betweenness, louvain"
            ))
        })?;
        // The rel table must resolve at bind time, so the first
        // argument is a string literal, not an expression.
        let Some(Expr::Literal(Literal::Str(rel_name))) = args.first() else {
            return Err(invalid(format!(
                "{}'s first argument must be a string naming a rel table",
                func.name()
            )));
        };
        let rel = self
            .schema
            .rel_by_name(rel_name)
            .ok_or_else(|| bad_reference(format!("unknown rel table '{rel_name}'")))?;
        if rel.from != rel.to {
            return Err(invalid(format!(
                "{} needs a rel table over one node table, '{}' connects two",
                func.name(),
                rel.name
            )));
        }
        let (rel_id, table) = (rel.id, rel.from);
        let mut bound_args = Vec::new();
        for arg in &args[1..] {
            let mut ctx = ExprCtx::new(false);
            let (expr, ty) = self.bind_expr(arg, &mut ctx)?;
            bound_args.push((expr, ty));
        }
        match func {
            TableFunc::Bfs | TableFunc::Sssp => {
                let name = func.name();
                if bound_args.len() != 1 {
                    return Err(invalid(format!(
                        "{name} takes the rel table and a source node id"
                    )));
                }
                if !matches!(bound_args[0].1, Type::Int | Type::Any) {
                    return Err(invalid(format!(
                        "{name}'s source must be a node id, got {}",
                        bound_args[0].1
                    )));
                }
            }
            TableFunc::SsspWeighted => {
                // The weight column is named rather than assumed: a rel
                // table can carry several numeric columns and which one
                // is a distance is the caller's to say.
                if bound_args.len() != 2 {
                    return Err(invalid(
                        "sssp_weighted takes the rel table, a source node id, and the name of \
                         the weight column"
                            .into(),
                    ));
                }
                if !matches!(bound_args[0].1, Type::Int | Type::Any) {
                    return Err(invalid(format!(
                        "sssp_weighted's source must be a node id, got {}",
                        bound_args[0].1
                    )));
                }
                if !matches!(args[2], Expr::Literal(Literal::Str(_))) {
                    return Err(invalid(
                        "sssp_weighted's weight column must be a string literal".into(),
                    ));
                }
            }
            TableFunc::Betweenness => {
                // The sources are a list and not a single node,
                // because the score a node gets is a sum over the
                // sample and running the sample one source at a time
                // would be one pass of the graph per source with the
                // adding left to the caller.
                if bound_args.len() != 1 {
                    return Err(invalid(
                        "betweenness takes the rel table and a list of source node ids".into(),
                    ));
                }
                if !matches!(bound_args[0].1, Type::List(_) | Type::Any) {
                    return Err(invalid(format!(
                        "betweenness's sources must be a list of node ids, got {}",
                        bound_args[0].1
                    )));
                }
            }
            TableFunc::Cdlp => {
                // The round count is what makes label propagation
                // reproducible, so it is spellable, and the default is
                // the one Graphalytics fixed.
                if bound_args.len() > 1 {
                    return Err(invalid(
                        "cdlp takes the rel table and an optional round count".into(),
                    ));
                }
                if let Some((_, ty)) = bound_args.first()
                    && !matches!(ty, Type::Int | Type::Any)
                {
                    return Err(invalid(format!(
                        "cdlp's round count must be an integer, got {ty}"
                    )));
                }
            }
            _ => {
                if !bound_args.is_empty() {
                    return Err(invalid(format!("{} takes only the rel table", func.name())));
                }
            }
        }
        let (value_name, value_ty) = func.value_column();
        let expected = ["node", value_name];
        if yields.len() != expected.len()
            || yields
                .iter()
                .zip(expected)
                .any(|((col, _), want)| col != want)
        {
            return Err(invalid(format!(
                "{} yields the columns node, {value_name}",
                func.name()
            )));
        }
        let mut slots = Vec::new();
        for (column, alias) in yields {
            let visible = alias.as_deref().unwrap_or(column);
            let ty = if column == "node" {
                Type::Node
            } else {
                value_ty.clone()
            };
            let slot = self.declare(visible, ty)?;
            if column == "node" {
                self.variables[slot].node_tables = vec![table];
            }
            slots.push(slot);
        }
        Ok(BoundClause::Call {
            func,
            rel: rel_id,
            table,
            args: bound_args.into_iter().map(|(expr, _)| expr).collect(),
            slots,
        })
    }

    // Projections.

    fn bind_projection(
        &mut self,
        projection: &Projection,
        role: Projected,
        filter: &Option<Expr>,
    ) -> Result<BoundClause> {
        let clause = match role {
            Projected::Onward => "WITH",
            Projected::Chained | Projected::Answer => "RETURN",
        };
        // A projection that hands its rows to something else has to
        // name them, so two items of one name are refused and the names
        // stay on their slots. The answer has neither problem: nothing
        // reads it by name, and duplicate column names in a result are
        // ordinary.
        let is_answer = role == Projected::Answer;
        let names_rows = role != Projected::Answer;
        // `*` expands the visible variables in slot order before any
        // explicit items.
        let mut items: Vec<BoundItem> = Vec::new();
        if projection.star {
            let mut visible: Vec<(usize, String)> = self
                .scope
                .iter()
                .map(|(name, &slot)| (slot, name.clone()))
                .collect();
            if visible.is_empty() {
                return Err(invalid(format!(
                    "{clause} * needs at least one variable in scope"
                )));
            }
            visible.sort_unstable();
            for (slot, name) in visible {
                items.push(BoundItem {
                    expr: BoundExpr::Var(slot),
                    ty: self.variables[slot].ty.clone(),
                    name,
                    slot: None,
                    aggregate: false,
                });
            }
        }
        for item in &projection.items {
            let mut ctx = ExprCtx::new(true);
            let (expr, ty) = self.bind_expr(&item.expr, &mut ctx)?;
            let name = match (&item.alias, &item.expr) {
                (Some(alias), _) => alias.clone(),
                (None, Expr::Variable(v)) => v.clone(),
                (None, other) => {
                    if role == Projected::Onward {
                        return Err(invalid(format!(
                            "WITH item {} needs an alias, only plain variables may go unaliased",
                            text(other)
                        )));
                    }
                    text(other)
                }
            };
            items.push(BoundItem {
                expr,
                ty,
                name,
                slot: None,
                aggregate: ctx.saw_aggregate,
            });
        }
        let mut has_aggregate = items.iter().any(|i| i.aggregate);
        let mut grouped_without_aggregate = false;

        // An explicit GROUP BY says what a group is, and the items say
        // what a row of one holds, so the two have to agree: every
        // item that is not an aggregate is read once per group and can
        // only be something the grouping already fixed.
        //
        // The keys are checked against the items rather than carried
        // beside them, because the grouping this engine runs is the
        // non-aggregate items themselves. That is the same grouping
        // when the two agree, and a key the projection does not carry
        // is refused by name rather than grouped by silently and
        // dropped.
        if !projection.group_by.is_empty() {
            let mut keys = Vec::new();
            for key in &projection.group_by {
                let mut ctx = ExprCtx::new(false);
                let (bound, _) = self.bind_expr(key, &mut ctx)?;
                keys.push((bound, text(key)));
            }
            for item in items.iter().filter(|item| !item.aggregate) {
                if !keys.iter().any(|(key, _)| *key == item.expr) {
                    return Err(invalid(format!(
                        "'{}' is read once per group, so it has to be one of the GROUP BY keys or an aggregate over the group",
                        item.name
                    )));
                }
            }
            for (key, written) in &keys {
                if !items
                    .iter()
                    .any(|item| !item.aggregate && item.expr == *key)
                {
                    return Err(invalid(format!(
                        "the GROUP BY key {written} is not one of the {clause} items, so the rows it groups have no column saying which group they are: project it as well"
                    )));
                }
            }
            // Grouping with nothing to aggregate answers one row per
            // group, which is what DISTINCT over the keys is, and the
            // keys are the items.
            grouped_without_aggregate = !has_aggregate;
            has_aggregate = true;
        }

        // ORDER BY and a WITH's WHERE see the projected names; without
        // aggregation the pre-projection variables stay visible too.
        let old_scope = self.scope.clone();
        let mut new_scope: HashMap<String, usize> = HashMap::new();
        for item in &mut items {
            if names_rows && new_scope.contains_key(&item.name) {
                return Err(bad_reference(format!(
                    "duplicate name '{}' in {clause}",
                    item.name
                )));
            }
            // Projecting a plain variable keeps its slot; anything else
            // gets a fresh one carrying the item's type.
            let slot = match item.expr {
                BoundExpr::Var(slot) => slot,
                _ => self.new_slot(item.name.clone(), item.ty.clone()),
            };
            item.slot = Some(slot);
            new_scope.entry(item.name.clone()).or_insert(slot);
        }
        let mut order_scope = new_scope.clone();
        if !has_aggregate {
            for (name, slot) in &old_scope {
                order_scope.entry(name.clone()).or_insert(*slot);
            }
        }

        self.scope = order_scope;
        let mut order_by = Vec::new();
        for key in &projection.order_by {
            let mut ctx = ExprCtx::new(true);
            let (bound, _) = self.bind_expr(&key.expr, &mut ctx)?;
            order_by.push(key.with_expr(bound));
        }
        let skip = self.bind_count_limit(&projection.skip, "SKIP")?;
        let limit = self.bind_count_limit(&projection.limit, "LIMIT")?;

        // The clause's ongoing scope is exactly the projected names,
        // which is also what an existence block in the WHERE sees: the
        // block runs after the projection, so it reads the projected
        // names and nothing the projection dropped.
        self.scope = new_scope;
        // A group stands for slots the walk filled, and behind a
        // projection those slots are gone: what a clause behind one
        // reads is the columns it projected. A group carried across is
        // carried as a list under a name of its own, which is what
        // projecting the group writes.
        self.groups.clear();
        self.forked.clear();
        // A WITH's WHERE runs over projected rows, and a mark there
        // would be a match under a projection, which is a shape
        // neither executor has. The block refuses rather than being
        // lifted somewhere it would read different rows.
        let filter = self.bind_where(filter, false)?;
        if is_answer {
            self.columns = items.iter().map(|i| i.name.clone()).collect();
            for item in &mut items {
                item.slot = None;
            }
        }
        Ok(BoundClause::Project {
            distinct: projection.distinct || grouped_without_aggregate,
            items,
            order_by,
            skip,
            limit,
            filter,
        })
    }

    /// Binds a `LET`: the names it gives, added to everything already
    /// in hand.
    ///
    /// The clause binds to the same projection every `WITH` binds to,
    /// carrying the variables in scope through unchanged and putting
    /// the new ones after them. That is what the statement means and it
    /// costs nothing to say it that way, since projecting a plain
    /// variable keeps the slot it was already in, so the carried names
    /// are not copied anywhere.
    ///
    /// The definitions are bound left to right and each one is in scope
    /// for the ones after it, which is how a reader writes a pair where
    /// the second is about the first. A projection evaluates all of its
    /// items against the row that came into it, so a definition that
    /// reads one the same statement made cannot be an item beside it:
    /// it starts a projection of its own, running behind the one that
    /// made what it reads. Definitions that read nothing new stay
    /// together, so the ordinary `LET a = ..., b = ...` is one operator
    /// and not one per name.
    fn bind_let(&mut self, items: &[ast::LetItem]) -> Result<BoundClause> {
        let mut stages: Vec<Vec<BoundItem>> = vec![Vec::new()];
        // The slots this statement has made so far. A definition that
        // reads one of them is the boundary between two stages.
        let mut fresh: HashSet<usize> = HashSet::new();
        for item in items {
            let mut ctx = ExprCtx::new(true);
            let (expr, ty) = self.bind_expr(&item.expr, &mut ctx)?;
            if ctx.saw_aggregate {
                // A set function reads a group of rows and a LET reads
                // one, so there is no group here for it to be over.
                return Err(invalid(format!(
                    "LET names what one row holds, so '{}' cannot be an aggregate: write it in a RETURN or a WITH, which is where the rows are grouped",
                    item.name
                )));
            }
            let mut read = HashSet::new();
            expr_slots(&expr, &mut read);
            if !read.is_disjoint(&fresh) {
                stages.push(Vec::new());
                fresh.clear();
            }
            let slot = self.declare(&item.name, ty.clone())?;
            fresh.insert(slot);
            stages.last_mut().expect("a stage is open").push(BoundItem {
                expr,
                ty,
                name: item.name.clone(),
                slot: Some(slot),
                aggregate: false,
            });
        }
        // Every stage carries the whole scope as it stood when the
        // stage was written, which is what makes this a LET and not a
        // WITH: the names already in hand go through it. Reading them
        // off the scope at the end and cutting each stage's list at the
        // slots that existed then works because a slot number only ever
        // goes up.
        let mut named: Vec<(usize, String)> = self
            .scope
            .iter()
            .map(|(name, &slot)| (slot, name.clone()))
            .collect();
        named.sort_unstable();
        let mut built = Vec::new();
        for stage in stages {
            let first_new = stage
                .first()
                .and_then(|item| item.slot)
                .expect("a stage holds at least one definition");
            let carried = named
                .iter()
                .filter(|(slot, _)| *slot < first_new)
                .map(|(slot, name)| BoundItem {
                    expr: BoundExpr::Var(*slot),
                    ty: self.variables[*slot].ty.clone(),
                    name: name.clone(),
                    slot: Some(*slot),
                    aggregate: false,
                });
            built.push(BoundClause::Project {
                distinct: false,
                items: carried.chain(stage).collect(),
                order_by: Vec::new(),
                skip: None,
                limit: None,
                filter: None,
            });
        }
        // The clause the caller gets is the first stage; the rest run
        // straight behind it, which is where `pending` puts them.
        let first = built.remove(0);
        self.pending.extend(built);
        Ok(first)
    }

    /// Binds a `YIELD`: the names that leave the match it stands after.
    ///
    /// It binds to the projection a `WITH` of the same names binds to,
    /// which is what the clause means and needs no operator of its own
    /// to say: projecting a plain variable keeps the slot it was
    /// already in, so the values the yield carries are the ones the
    /// match matched rather than copies of them. It does not group, it
    /// does not order and it does not cut, so the rows a match answered
    /// are the rows the yield answers.
    ///
    /// An item is a variable rather than an expression, so a name the
    /// match did not write is refused here rather than bound to
    /// whatever else is in scope, and two items ending under one name
    /// are refused for the reason a `WITH` refuses them, which is that
    /// the clause after this one would have two things to read for it.
    fn bind_yield(&mut self, items: &[ast::YieldItem]) -> Result<BoundClause> {
        let mut bound = Vec::with_capacity(items.len());
        let mut scope = HashMap::new();
        for item in items {
            let Some(&slot) = self.scope.get(&item.name) else {
                if self.groups.contains_key(&item.name) {
                    return Err(invalid(format!(
                        "'{}' stands for one element per repetition of the stretch that bound it, which is a list rather than a variable the match wrote, so a YIELD cannot carry it: project it in a RETURN or a WITH",
                        item.name
                    )));
                }
                return Err(invalid(format!(
                    "YIELD lets a variable out of the match in front of it, and '{}' is not one: yield a name the match wrote",
                    item.name
                )));
            };
            let name = item.alias.clone().unwrap_or_else(|| item.name.clone());
            if scope.contains_key(&name) {
                return Err(invalid(format!(
                    "YIELD names '{name}' twice, so the clause after it would have two of them to read: write one of them AS another name"
                )));
            }
            scope.insert(name.clone(), slot);
            bound.push(BoundItem {
                expr: BoundExpr::Var(slot),
                ty: self.variables[slot].ty.clone(),
                name,
                slot: Some(slot),
                aggregate: false,
            });
        }
        self.scope = scope;
        self.groups.clear();
        self.forked.clear();
        Ok(BoundClause::Project {
            distinct: false,
            items: bound,
            order_by: Vec::new(),
            skip: None,
            limit: None,
            filter: None,
        })
    }

    /// Binds `FINISH`, the primitive result statement of ISO 14.10.
    ///
    /// What it answers is a table with no columns and no rows, and
    /// that is what it binds to: a projection that keeps no column of
    /// what the clauses found, with a page of no rows behind it. The
    /// two halves are both needed and neither is a trick. A projection
    /// of nothing is what makes the columns none, and a result with no
    /// columns is not the same thing as a result with no rows, since a
    /// statement that carries rows between two others has slots and no
    /// column names. The page of nothing is what makes the rows none,
    /// and it is what lets the executor stop early, since a statement
    /// that says it wants nothing back should not pay for the rows it
    /// is about to throw away.
    ///
    /// It is always the answer, the parser having refused anything
    /// that would read from it.
    fn bind_finish(&mut self) -> Result<BoundClause> {
        let projection = Projection {
            distinct: false,
            star: false,
            items: Vec::new(),
            group_by: Vec::new(),
            order_by: Vec::new(),
            skip: None,
            limit: Some(Expr::Literal(Literal::Int(0))),
        };
        self.bind_projection(&projection, Projected::Answer, &None)
    }

    /// Binds the order by and page statement of ISO 14.9, the one that
    /// stands where a statement stands rather than behind a `RETURN`.
    ///
    /// It says which rows come first and which of them come out at all,
    /// and it says nothing about the columns: everything in hand stays
    /// in hand. That is what a `WITH *` carrying the same tail already
    /// means, so that is what this binds to, rather than to a clause of
    /// its own that would have to answer the same questions about
    /// groups and paths a second time and could answer one of them
    /// differently.
    fn bind_order(
        &mut self,
        keys: &[SortKey<Expr>],
        skip: &Option<Expr>,
        limit: &Option<Expr>,
    ) -> Result<BoundClause> {
        let projection = Projection {
            distinct: false,
            star: true,
            items: Vec::new(),
            group_by: Vec::new(),
            order_by: keys.to_vec(),
            skip: skip.clone(),
            limit: limit.clone(),
        };
        self.bind_projection(&projection, Projected::Onward, &None)
    }

    fn bind_count_limit(&mut self, expr: &Option<Expr>, what: &str) -> Result<Option<BoundExpr>> {
        let Some(expr) = expr else { return Ok(None) };
        let mut ctx = ExprCtx::new(false);
        let (bound, ty) = self.bind_expr(expr, &mut ctx)?;
        if !matches!(ty, Type::Int | Type::Any) {
            return Err(bad_type(format!("{what} needs an integer, got {ty}")));
        }
        Ok(Some(bound))
    }

    fn bind_bool(&mut self, expr: &Expr, what: &str) -> Result<BoundExpr> {
        let mut ctx = ExprCtx::new(false);
        let (bound, ty) = self.bind_expr(expr, &mut ctx)?;
        if !ty.is_bool() {
            return Err(invalid(format!(
                "{what} needs a boolean, got {ty} from {}",
                text(expr)
            )));
        }
        Ok(bound)
    }

    // Patterns.

    fn bind_path(&mut self, path: &ast::PathPattern) -> Result<BoundPath> {
        let slot = match &path.var {
            Some(name) => Some(self.declare(name, Type::Path)?),
            None => None,
        };
        let start = self.bind_node(&path.start)?;
        let mut steps = Vec::new();
        for (at, (rel, node)) in path.steps.iter().enumerate() {
            // A pattern that named no path mode walks under the one its
            // match mode settles, which is TRAIL under DIFFERENT EDGES
            // and WALK under REPEATABLE ELEMENTS, and a step inside
            // brackets that named one walks under that instead.
            let mode = subpath_mode(&path.subpaths, at)
                .or(path.mode)
                .unwrap_or(path.list.mode.path_mode());
            let rel = self.bind_rel(rel, mode, path.selector)?;
            let node = self.bind_node(node)?;
            steps.push((rel, node));
        }
        if path.selector.is_some() && steps.iter().all(|(rel, _)| rel.range.is_none()) {
            // A pattern of fixed length matches one path per set of
            // elements, so there is nothing for a selector to choose
            // between and no search for it to cut short. Two parallel
            // edges are the one case where there would be, and picking
            // between those is not implemented, so it is refused rather
            // than answered as though the selector had been left out.
            return Err(invalid(
                "a path selector needs a variable-length relationship to choose between paths"
                    .into(),
            ));
        }
        if let Some(slot) = slot {
            let parts = walk(&start, &steps, 0, steps.len());
            self.path_shapes.insert(slot, parts);
        }
        // A subpath variable is the same kind of value over a stretch of
        // the same walk, so it is bound the same way and told where to
        // start and stop.
        for sub in &path.subpaths {
            let Some(name) = &sub.var else { continue };
            let slot = self.declare(name, Type::Path)?;
            let parts = walk(&start, &steps, sub.from, sub.to);
            self.path_shapes.insert(slot, parts);
        }
        // A name a repeated stretch bound stands for the elements of
        // every repetition, so it points at the slots the walk filled
        // rather than at a slot of its own.
        for group in &path.groups {
            if self.scope.contains_key(&group.name) {
                return Err(bad_reference(format!(
                    "'{}' already stands for one element, and a name inside a repeated \
                     stretch stands for one per repetition",
                    group.name
                )));
            }
            let element = match group.kind {
                ast::GroupKind::Node => Type::Node,
                ast::GroupKind::Rel => Type::Rel,
            };
            let slots = group
                .at
                .iter()
                .map(|&at| match group.kind {
                    ast::GroupKind::Node if at == 0 => start.slot,
                    ast::GroupKind::Node => steps[at - 1].1.slot,
                    ast::GroupKind::Rel => steps[at].0.slot,
                })
                .collect();
            self.groups
                .insert(group.name.clone(), GroupVar { element, slots });
        }
        Ok(BoundPath { slot, start, steps })
    }

    /// The predicate `DIFFERENT EDGES` stands for (ISO 16.9): no edge
    /// of the graph answers two of the edge patterns of one pattern
    /// list at once.
    ///
    /// What a path mode forbids inside one path this forbids across the
    /// patterns of a list, and it is the mode a list that named none
    /// runs under, so `MATCH (a)-[e]->(b), (a)-[f]->(b)` answers the
    /// pairs of distinct edges between a pair of nodes rather than
    /// every edge paired with itself. The patterns are grouped by the
    /// list they were written in, because a match statement block
    /// gathers several lists and each of them keeps its own edges
    /// apart.
    ///
    /// A pair is tested when the two steps describe the same pair of
    /// ends and their types overlap, which is when any graph at all can
    /// answer both with one edge. Two steps that end somewhere else are
    /// left alone, and that is where this stops short of what the
    /// standard asks: `(a)-[e]->(b)-[f]->(c)` can answer both with one
    /// edge too, when the graph holds a loop at a node and `a`, `b` and
    /// `c` all land on it. Testing that pair costs a comparison on
    /// every two hop walk of every query, to rule out a shape that no
    /// graph without a loop holds, so the test belongs behind a fact
    /// about the graph rather than in front of it. docs/07 says what is
    /// checked and what is not.
    fn edge_distinctness(
        &self,
        patterns: &[ast::PathPattern],
        bound: &[BoundPath],
    ) -> Vec<BoundExpr> {
        let mut tests = Vec::new();
        let mut lists: Vec<(u32, Vec<Step<'_>>)> = Vec::new();
        for (path, bound) in patterns.iter().zip(bound) {
            let mut near = bound.start.slot;
            let mut steps = Vec::with_capacity(path.steps.len());
            for ((pat, _), (rel, node)) in path.steps.iter().zip(&bound.steps) {
                steps.push(Step {
                    pat,
                    rel,
                    ends: (near, node.slot),
                });
                near = node.slot;
            }
            tests.extend(self.repeat_distinctness(path, &steps));
            if path.list.mode != ast::MatchMode::DifferentEdges {
                continue;
            }
            match lists.iter_mut().find(|(at, _)| *at == path.list.at) {
                Some((_, edges)) => edges.append(&mut steps),
                None => lists.push((path.list.at, steps)),
            }
        }
        for (_, edges) in &lists {
            for (at, left) in edges.iter().enumerate() {
                for right in &edges[at + 1..] {
                    if !same_ends(left, right) || disjoint_types(left.pat, right.pat) {
                        continue;
                    }
                    tests.extend(distinct_test(left, right));
                }
            }
        }
        tests
    }

    /// The edges a quantified stretch has to keep apart.
    ///
    /// A stretch repeated a fixed number of times is written out as that
    /// many copies of the same steps, so the ends test above passes the
    /// copies over: they end at different nodes of the pattern, and only
    /// a graph holding a loop answers two of them with one edge. Here
    /// the engine knows the steps are copies of one step rather than two
    /// steps that happen to look alike, so the pair is worth testing and
    /// the cost is paid only by a query that wrote the quantifier. It is
    /// what makes `((x)-[:knows]->(y)){2}` answer what `-[:knows*2..2]->`
    /// answers on a graph with a loop in it.
    ///
    /// The stretch walks under the mode the pattern around it settles,
    /// which is `TRAIL` under `DIFFERENT EDGES` and `WALK` under
    /// `REPEATABLE ELEMENTS`, and a walk repeats what it likes.
    fn repeat_distinctness(&self, path: &ast::PathPattern, steps: &[Step<'_>]) -> Vec<BoundExpr> {
        let mut tests = Vec::new();
        for repeat in &path.repeats {
            let mode = subpath_mode(&path.subpaths, repeat.from)
                .or(path.mode)
                .unwrap_or(path.list.mode.path_mode());
            if mode == PathMode::Walk {
                continue;
            }
            let copies = &steps[repeat.from..repeat.to];
            for (at, left) in copies.iter().enumerate() {
                for right in &copies[at + 1..] {
                    // The pair the ends test already wrote, and a pair
                    // no graph answers with one edge because the two
                    // steps name types with nothing in common.
                    let written =
                        path.list.mode == ast::MatchMode::DifferentEdges && same_ends(left, right);
                    if written || disjoint_types(left.pat, right.pat) {
                        continue;
                    }
                    tests.extend(distinct_test(left, right));
                }
            }
        }
        tests
    }

    /// Binds one end of a path written under `INSERT`, and says which
    /// slot holds the row that end is.
    ///
    /// An end is either an element the clause makes or a name already
    /// standing for one. `INSERT (a:person), (a)-[:knows]->(b:person)`
    /// writes two elements and one edge, and the `(a)` in the second
    /// pattern is the first one being pointed at rather than a third
    /// element with the same name. A name that is being pointed at
    /// carries nothing of its own, because a label or a property there
    /// would be describing an element that has already been described.
    fn bind_insert_end(
        &mut self,
        pat: &NodePattern,
        nodes: &mut Vec<BoundInsertNode>,
    ) -> Result<usize> {
        let bound = pat
            .var
            .as_deref()
            .and_then(|name| self.scope.get(name).copied());
        let Some(slot) = bound else {
            let node = self.bind_insert_node(pat)?;
            let slot = node.slot;
            nodes.push(node);
            return Ok(slot);
        };
        if pat.label.is_some() || !pat.props.is_empty() {
            return Err(invalid(format!(
                "'{}' already stands for an element here, so writing a label or a property on it would be describing an element that is already described",
                pat.var.as_deref().unwrap_or("")
            )));
        }
        if self.variables[slot].ty != Type::Node {
            return Err(bad_type(format!(
                "'{}' stands for an edge, and an edge is not an end of another one",
                pat.var.as_deref().unwrap_or("")
            )));
        }
        Ok(slot)
    }

    /// Binds `MERGE`, the statement that finds a pattern or writes it.
    ///
    /// The pattern is bound once, as a walk to look for, and the
    /// elements to write are read off what that bound. The other way
    /// round does not work: binding the write first declares the
    /// variables, and the walk would then be a pattern over names that
    /// already stand for something, which is a different pattern and
    /// one the binder refuses anyway.
    ///
    /// So what comes out is an optional match and an insert over the
    /// same slots, and the row on the other side is what tells them
    /// apart: a walk that found nothing leaves nulls where the pattern
    /// is, and those are the rows the insert runs for.
    fn bind_merge(
        &mut self,
        pattern: &ast::PathPattern,
        on_create: &[SetItem],
        on_match: &[SetItem],
    ) -> Result<BoundClause> {
        // Read before the clause writes anything into scope, for the
        // reason `INSERT` reads it there.
        let carry = self.carried();
        let at = carry.len();
        self.refuse_unmergeable(pattern)?;
        let mut probe = self.bind_path(pattern)?;
        self.narrow_path(&probe)?;
        self.settle_labels(&mut probe);
        // A pattern whose steps could walk one edge twice is a pattern
        // that finds a walk the store does not hold, and then the write
        // would run for a row that was there. The test is the one every
        // match asks.
        let filter = and_all(
            None,
            self.edge_distinctness(std::slice::from_ref(pattern), std::slice::from_ref(&probe)),
        );
        let mut nodes = Vec::new();
        let mut rels = Vec::new();
        let mut left = merge_end(self, &pattern.start, &probe.start, &carry, &mut nodes)?;
        for ((step, node), (rel, bound)) in pattern.steps.iter().zip(&probe.steps) {
            let right = merge_end(self, node, bound, &carry, &mut nodes)?;
            let (table, src, dst) = self.insert_rel_ends(step, left, right, &nodes, "MERGE")?;
            rels.push(BoundInsertRel {
                slot: rel.slot,
                table,
                src,
                dst,
                props: rel.props.clone(),
            });
            left = right;
        }
        if nodes.is_empty() && rels.is_empty() {
            return Err(invalid(
                "this MERGE names only elements earlier clauses already found, so there is nothing for it to look for and nothing for it to write".into(),
            ));
        }
        let created: Vec<usize> = nodes
            .iter()
            .map(|node| node.slot)
            .chain(rels.iter().map(|rel| rel.slot))
            .collect();
        for item in on_create {
            self.merge_on_create(item, &created, &mut nodes, &mut rels)?;
        }
        let mut matched = Vec::with_capacity(on_match.len());
        for item in on_match {
            matched.push(self.bind_set_item(item)?);
        }
        once_each("ON MATCH SET", &matched)?;
        let mut carry = carry;
        carry.extend(created);
        Ok(BoundClause::Merge {
            probe,
            filter,
            nodes,
            rels,
            on_match: matched,
            carry,
            at,
        })
    }

    /// Turns away the pattern shapes a `MERGE` cannot mean, before any
    /// of it is bound.
    ///
    /// The pattern is read and written by the same brackets, so
    /// anything in it that only makes sense on one side of that is
    /// turned away rather than read one way and written another. A
    /// selector and a path mode pick between the walks that are there,
    /// and this clause writes one when there are none. A condition
    /// inside a bracket says which elements match, and this clause
    /// makes an element when none does, which is not something a
    /// condition describes.
    fn refuse_unmergeable(&self, pattern: &ast::PathPattern) -> Result<()> {
        if pattern.var.is_some() {
            return Err(not_yet(
                "MERGE binding the path it found, which is a walk over rows some of which may be being made,",
            ));
        }
        if pattern.selector.is_some() || pattern.mode.is_some() {
            return Err(invalid(
                "a selector or a path mode says which of the walks that are there to pick, and MERGE writes the walk when there is none".into(),
            ));
        }
        if !pattern.subpaths.is_empty() || !pattern.groups.is_empty() {
            return Err(invalid(
                "brackets around part of a pattern name a stretch of a walk, and MERGE is one pattern that is either all found or all written".into(),
            ));
        }
        let inside = pattern.filter.is_some()
            || pattern.start.filter.is_some()
            || pattern
                .steps
                .iter()
                .any(|(rel, node)| rel.filter.is_some() || node.filter.is_some());
        if inside {
            return Err(invalid(
                "a condition inside a pattern picks which elements match it, and MERGE writes the elements when none does, which is not something a condition describes".into(),
            ));
        }
        Ok(())
    }

    /// Folds one `ON CREATE SET` item into the element it writes.
    ///
    /// A property an element takes at creation is one of the properties
    /// it is created with, so this is not a write after the write: the
    /// item joins the properties the pattern already named and the
    /// element is made holding both. That is also why the value cannot
    /// read the element: it is worked out before there is one.
    fn merge_on_create(
        &mut self,
        item: &SetItem,
        created: &[usize],
        nodes: &mut [BoundInsertNode],
        rels: &mut [BoundInsertRel],
    ) -> Result<()> {
        let target = self.write_target("ON CREATE SET", &item.target)?;
        if !created.contains(&target) {
            return Err(invalid(format!(
                "ON CREATE SET writes the elements the pattern wrote, and '{}' is one MERGE was given rather than one it makes",
                item.target
            )));
        }
        let SetInto::Property(key) = &item.into else {
            return Err(not_yet(
                "ON CREATE SET writing an element's whole record or its labels, which is a write to an element that is still being made,",
            ));
        };
        let mut ctx = ExprCtx::new(false);
        let (value, _) = self.bind_expr(&item.value, &mut ctx)?;
        let mut reads: Vec<usize> = Vec::new();
        expr_slots(&value, &mut reads);
        if let Some(&slot) = reads.iter().find(|slot| created.contains(slot)) {
            return Err(not_yet(&format!(
                "ON CREATE SET reading {}, which the statement is making and which holds nothing until it has,",
                self.var_text(slot)
            )));
        }
        let props = merge_props(nodes, rels, target);
        if props.iter().any(|(written, _)| written == key) {
            return Err(ZuError::gql(
                codes::C22G0M,
                format!(
                    "this MERGE writes property '{key}' of one element twice, and an element holds one value per property"
                ),
            ));
        }
        props.push((key.clone(), value));
        Ok(())
    }

    /// The slots a write carries across itself, which is everything in
    /// scope where it sits.
    ///
    /// A write runs once for every row the clauses before it answered
    /// and the clauses after it read those rows rather than the store,
    /// so every slot they might read has to come along.
    fn carried(&self) -> Vec<usize> {
        let mut carry: Vec<usize> = self.scope.values().copied().collect();
        // A path variable is assembled from the slots of its walk, and
        // the ones the query never named are in no scope, so they come
        // along by way of the shape.
        let parts: Vec<usize> = carry
            .iter()
            .filter_map(|slot| self.path_shapes.get(slot))
            .flatten()
            .map(|part| match part {
                PathPart::Node(slot) | PathPart::Rel(slot) | PathPart::VarRel(slot) => *slot,
            })
            .collect();
        carry.extend(parts);
        carry.sort_unstable();
        carry.dedup();
        carry
    }

    /// Binds one assignment under `SET`.
    ///
    /// The element has to be in scope, because a statement changes what
    /// it found: a name no clause bound stands for nothing to change.
    /// A node and an edge are both elements here, and which column the
    /// key names is settled where the table's columns are, which is the
    /// file rather than the schema the binder is given. An item that
    /// names no key writes the whole record, and which columns that
    /// covers is a question about the same file, so it is settled in the
    /// same place.
    fn bind_set_item(&mut self, item: &SetItem) -> Result<BoundSetItem> {
        let target = self.write_target("SET", &item.target)?;
        let mut ctx = ExprCtx::new(false);
        let (value, _) = self.bind_expr(&item.value, &mut ctx)?;
        Ok(BoundSetItem {
            target,
            into: match &item.into {
                SetInto::Property(key) => BoundSetInto::Property(key.clone()),
                SetInto::Record => BoundSetInto::Record,
                SetInto::Labels(labels) => BoundSetInto::Labels {
                    labels: labels.clone(),
                    on: true,
                },
            },
            value,
        })
    }

    /// Binds one `REMOVE` item as the assignment GQL says it is: the
    /// property takes null, and a column takes a null by holding
    /// nothing in that row. Everything downstream of here sees a `SET`,
    /// which is why `SET p.age = null` and `REMOVE p.age` do the same
    /// thing rather than nearly the same thing.
    fn bind_remove_item(&mut self, item: &RemoveItem) -> Result<BoundSetItem> {
        let target = self.write_target("REMOVE", &item.target)?;
        let mut ctx = ExprCtx::new(false);
        let (value, _) = self.bind_expr(&Expr::Literal(Literal::Null), &mut ctx)?;
        Ok(BoundSetItem {
            target,
            into: match &item.what {
                Removed::Property(key) => BoundSetInto::Property(key.clone()),
                Removed::Labels(labels) => BoundSetInto::Labels {
                    labels: labels.clone(),
                    on: false,
                },
            },
            value,
        })
    }

    /// The slot a write names, which has to be a node an earlier clause
    /// bound. `verb` is the statement asking, so that a reader is told
    /// which of their clauses is the one that cannot run.
    fn write_target(&self, verb: &str, name: &str) -> Result<usize> {
        let Some(&target) = self.scope.get(name) else {
            return Err(bad_reference(format!(
                "'{name}' stands for nothing here, and {verb} changes an element an earlier clause found"
            )));
        };
        match self.variables[target].ty {
            // An edge takes a property the way a node does. Where it
            // keeps it is different, since an edge column is in the
            // order its table holds its edges rather than in row order,
            // but which column a key names is settled against the file
            // and not here either way.
            Type::Node | Type::Rel => Ok(target),
            ref other => Err(bad_type(format!(
                "{verb} changes an element, and '{name}' is {other}"
            ))),
        }
    }

    /// Binds one variable under `DELETE` to the slot it stands for.
    ///
    /// A delete takes away an element, so the variable has to stand for
    /// one: a value is not deletable, and an edge is not deletable yet
    /// because taking one away means taking it out of the adjacency its
    /// table holds it in.
    fn bind_delete_target(&mut self, name: &str) -> Result<usize> {
        let Some(&target) = self.scope.get(name) else {
            return Err(bad_reference(format!(
                "'{name}' stands for nothing here, and DELETE takes away an element an earlier clause found"
            )));
        };
        match self.variables[target].ty {
            // An edge is deletable the same way an element is, and it
            // is the one thing a plain DELETE never has to refuse: an
            // edge has no edges on it, so taking it away leaves both
            // the rows it ran between standing.
            Type::Node | Type::Rel => Ok(target),
            ref other => Err(bad_type(format!(
                "DELETE takes away an element, and '{name}' is {other}"
            ))),
        }
    }

    /// Binds one edge written under `INSERT`, between the two slots the
    /// ends of its step landed in.
    ///
    /// `nodes` is what the clause has made so far, which is how an end
    /// this clause writes is told apart from one an earlier clause
    /// found: the first is in the table its label named, and the second
    /// is in whichever tables the match left open.
    fn bind_insert_rel(
        &mut self,
        pat: &RelPattern,
        left: usize,
        right: usize,
        nodes: &[BoundInsertNode],
    ) -> Result<BoundInsertRel> {
        let (table, src, dst) = self.insert_rel_ends(pat, left, right, nodes, "INSERT")?;
        let slot = match &pat.var {
            Some(name) => self.declare(name, Type::Rel)?,
            None => self.anon_slot(Type::Rel),
        };
        self.variables[slot].rel_tables = vec![table];
        let props = self.bind_props(&pat.props)?;
        Ok(BoundInsertRel {
            slot,
            table,
            src,
            dst,
            props,
        })
    }

    /// Which rel table a written edge pattern goes in and which way
    /// round it runs, as the table and the two slots. This is the part
    /// of writing an edge that only reads: `INSERT` declares the slot
    /// after it and `MERGE` already has one.
    fn insert_rel_ends(
        &self,
        pat: &RelPattern,
        left: usize,
        right: usize,
        nodes: &[BoundInsertNode],
        verb: &str,
    ) -> Result<(u32, usize, usize)> {
        if pat.range.is_some() {
            return Err(invalid(format!(
                "a hop range asks for a walk of some length, and {verb} writes one edge"
            )));
        }
        let [name] = pat.types.as_slice() else {
            return Err(match pat.types.is_empty() {
                true => invalid(format!(
                    "{verb} needs an edge type saying which table the edge goes in, and this one names none"
                )),
                false => invalid(format!(
                    "an edge goes in one table, and '{}' names {}",
                    pat.types.join("|"),
                    pat.types.len()
                )),
            });
        };
        let rel = self
            .schema
            .rels
            .iter()
            .find(|r| r.name == *name)
            .ok_or_else(|| bad_reference(format!("no edge table is named '{name}'")))?
            .clone();
        // Which way round the arrow points is which row the edge leaves
        // and which it arrives at, and that is the whole of the
        // difference: the edge is stored once either way round. A
        // pattern that does not point says the edge is one of several
        // things, and an edge being made has to be one thing. Which
        // arrows a table takes is the table's own answer: an undirected
        // one holds edges that point nowhere, and writing one of those
        // with an arrow would be writing something the table cannot
        // hold.
        let (src, dst) = match (pat.direction, rel.undirected) {
            (RelDirection::Out, false) => (left, right),
            (RelDirection::In, false) => (right, left),
            (RelDirection::Undirected, true) => (left, right),
            (_, undirected) => {
                return Err(invalid(format!(
                    "'{}' holds {} edges, and this pattern is not written as one of those",
                    rel.name,
                    match undirected {
                        true => "undirected",
                        false => "directed",
                    }
                )));
            }
        };
        // A rel table holds edges between two node tables and nothing
        // else, so an end in the wrong table is an edge the file has
        // nowhere to put. Saying so here names both tables; letting it
        // through would write an edge whose endpoint no reader resolves.
        // An end this clause writes is in the table its label named. An
        // end a MATCH found is in whichever table the match narrowed it
        // to, which can be more than one, and then the row itself is
        // the answer and the write checks it against the one it has.
        for (end, want, side) in [(src, rel.from, "leaves"), (dst, rel.to, "arrives at")] {
            let tables = match nodes.iter().find(|n| n.slot == end) {
                Some(made) => std::slice::from_ref(&made.table),
                None => self.variables[end].node_tables.as_slice(),
            };
            if !tables.is_empty() && !tables.contains(&want) {
                let names: Vec<&str> = tables.iter().map(|t| self.table_name(*t)).collect();
                return Err(bad_reference(format!(
                    "an edge in '{}' {side} an element of '{}', and {} is in '{}'",
                    rel.name,
                    self.table_name(want),
                    self.var_text(end),
                    names.join("|")
                )));
            }
        }
        Ok((rel.id, src, dst))
    }

    /// How to write a slot in a message: the name the statement gave
    /// it, or a description when the statement gave it none.
    fn var_text(&self, slot: usize) -> String {
        let name = &self.variables[slot].name;
        match name.starts_with('#') {
            true => "the element at that end".to_string(),
            false => format!("'{name}'"),
        }
    }

    /// The name of a node table, for a message that has to say which
    /// one an element is in.
    fn table_name(&self, table: u32) -> &str {
        self.schema
            .nodes
            .iter()
            .find(|n| n.id == table)
            .map_or("?", |n| n.name.as_str())
    }

    /// Binds one node pattern written under `INSERT`.
    ///
    /// A written pattern is a description, and reading one and writing
    /// one read the description differently. A label expression is a
    /// test when it is matched and an instruction when it is inserted,
    /// so the only expressions that make sense here are the ones that
    /// name exactly one table: `(x:person)` says where the row goes,
    /// `(x)` and `(x:person|company)` do not say, and a table nobody
    /// named is not one this can pick.
    fn bind_insert_node(&mut self, pat: &NodePattern) -> Result<BoundInsertNode> {
        if pat.filter.is_some() {
            return Err(invalid(
                "a condition inside an element pattern picks which elements match it, and \
                 an INSERT describes an element to make rather than one to find"
                    .into(),
            ));
        }
        let table = self.insert_table(pat, "INSERT")?;
        let props = self.bind_props(&pat.props)?;
        let slot = match &pat.var {
            Some(name) => self.declare(name, Type::Node)?,
            None => self.anon_slot(Type::Node),
        };
        self.variables[slot].node_tables = vec![table];
        Ok(BoundInsertNode { slot, table, props })
    }

    /// The node table a written element pattern goes in, which is the
    /// table whose own name is the label the pattern carries. `verb` is
    /// the statement asking, since `INSERT` and `MERGE` both write an
    /// element out of a pattern and a reader wants to be told which of
    /// their clauses is the one that cannot run.
    fn insert_table(&self, pat: &NodePattern, verb: &str) -> Result<u32> {
        let Some(label) = &pat.label else {
            return Err(invalid(format!(
                "{verb} needs a label saying which table the element goes in, and '({})' names none",
                pat.var.as_deref().unwrap_or("")
            )));
        };
        let LabelExpr::Label(name) = label else {
            return Err(not_yet(&format!(
                "{verb} of an element whose labels are written as anything but one name,"
            )));
        };
        // A row lands in a table, and the label a table gives every row
        // it holds is its own name, so that is the one that says where
        // an element goes. A secondary label is something a row carries
        // rather than somewhere it lives, and adding one to an element
        // being created is a key label set change, which is its own
        // line on the milestone.
        Ok(self
            .schema
            .nodes
            .iter()
            .find(|n| n.name == *name)
            .ok_or_else(|| {
                bad_reference(format!(
                    "no node table is named '{name}', and an element is created in the table whose own name is the label"
                ))
            })?
            .id)
    }

    fn bind_node(&mut self, pat: &NodePattern) -> Result<BoundNode> {
        // A label expression is bit tests over the word a row carries.
        // Folding it against a table's declared set answers it outright
        // for most tables: a table that cannot satisfy it is dropped
        // here and a table that always satisfies it needs no test, so
        // the runtime only sees what the schema left open.
        let test = pat
            .label
            .as_ref()
            .map(|expr| self.compile_label(expr))
            .transpose()?;
        let mut candidates = Vec::new();
        let mut settled = true;
        for node in &self.schema.nodes {
            let answer = match &test {
                None => Some(true),
                Some(test) => test.constant(node.label_mask(), 1 << node.primary_label()),
            };
            if answer == Some(false) {
                continue;
            }
            settled &= answer == Some(true);
            candidates.push(node.id);
        }
        // No candidate table left is not a refusal. The labels asked
        // for something the graph holds nowhere, so the pattern matches
        // nothing, and a scan over no tables is how that is said: the
        // statement runs, the clauses after it see an empty binding
        // table, and an aggregate over it still answers.
        let residue = match settled {
            true => None,
            false => test.map(|test| test.prune(guaranteed(self.schema, &candidates))),
        };
        let slot = match &pat.var {
            Some(name) => match self.scope.get(name).copied() {
                Some(slot) => {
                    if self.variables[slot].ty != Type::Node {
                        // The name resolves and resolves to something
                        // already taken, which is what 42002 is for.
                        return Err(bad_reference(format!(
                            "'{name}' is already bound as {}, not a node",
                            self.variables[slot].ty
                        )));
                    }
                    // A reused variable narrows to the tables both
                    // occurrences allow.
                    let existing = &self.variables[slot].node_tables;
                    let merged: Vec<u32> = existing
                        .iter()
                        .copied()
                        .filter(|id| candidates.contains(id))
                        .collect();
                    self.variables[slot].node_tables = merged;
                    slot
                }
                None => {
                    let slot = self.declare(name, Type::Node)?;
                    self.variables[slot].node_tables = candidates;
                    slot
                }
            },
            None => {
                let slot = self.anon_slot(Type::Node);
                self.variables[slot].node_tables = candidates;
                slot
            }
        };
        // The other names the node was written under stand for the one
        // element, so they name the one slot rather than a slot each.
        // A name already standing for something else is refused: making
        // it stand for this as well would be two elements under one
        // name, which is what 42002 is for.
        for name in &pat.aliases {
            match self.scope.get(name).copied() {
                Some(seen) if seen == slot => {}
                Some(_) => {
                    return Err(bad_reference(format!(
                        "'{name}' already stands for something else, and where two stretches of \
                         a pattern meet it would have to stand for the node they meet at"
                    )));
                }
                None => {
                    self.scope.insert(name.clone(), slot);
                }
            }
        }
        let props = self.bind_props(&pat.props)?;
        // The predicate is bound after the node's own name is in scope,
        // since it is mostly about this node, and everything the
        // pattern bound to its left is in scope already, which is what
        // lets it compare this node with one the walk came from.
        let filter = match &pat.filter {
            Some(expr) => {
                let mut ctx = ExprCtx::new(false);
                Some(self.bind_expr(expr, &mut ctx)?.0)
            }
            None => None,
        };
        Ok(BoundNode {
            slot,
            props,
            label: residue,
            filter,
        })
    }

    /// Resolves the names in a label expression to dictionary bits.
    /// A conjunction of plain names collapses to one mask, which is the
    /// shape almost every pattern has and the one the runtime answers
    /// with a single AND.
    fn compile_label(&self, expr: &LabelExpr) -> Result<LabelTest> {
        Ok(match expr {
            // A name the dictionary does not hold is not a mistake to
            // refuse. GQL asks a label expression of each element, and
            // an element carrying a label no element carries is a
            // question with the answer no, so the pattern matches
            // nothing and the statement runs over no rows.
            LabelExpr::Label(name) => match self.schema.label_id(name) {
                Some(id) => LabelTest::All(1 << id),
                None => LabelTest::Never(name.clone()),
            },
            LabelExpr::Wildcard => LabelTest::Any,
            LabelExpr::Not(inner) => LabelTest::Not(Box::new(self.compile_label(inner)?)),
            LabelExpr::And(lhs, rhs) => {
                match (self.compile_label(lhs)?, self.compile_label(rhs)?) {
                    (LabelTest::All(a), LabelTest::All(b)) => LabelTest::All(a | b),
                    (lhs, rhs) => LabelTest::And(Box::new(lhs), Box::new(rhs)),
                }
            }
            LabelExpr::Or(lhs, rhs) => LabelTest::Or(
                Box::new(self.compile_label(lhs)?),
                Box::new(self.compile_label(rhs)?),
            ),
        })
    }

    /// Drops a label test the endpoint narrowing has since answered.
    /// `bind_node` folds against every table in the graph, but a rel
    /// type cuts the candidates down further, and a test every table
    /// still standing satisfies is no test at all.
    fn settle_labels(&self, path: &mut BoundPath) {
        self.settle_node(&mut path.start);
        for (_, node) in &mut path.steps {
            self.settle_node(node);
        }
    }

    fn settle_node(&self, node: &mut BoundNode) {
        let Some(test) = &node.label else { return };
        let tables = &self.variables[node.slot].node_tables;
        let settled = tables
            .iter()
            .filter_map(|id| self.schema.node_by_id(*id))
            .all(|n| test.constant(n.label_mask(), 1 << n.primary_label()) == Some(true));
        node.label = match settled {
            true => None,
            false => Some(test.prune(guaranteed(self.schema, tables))),
        };
    }

    fn bind_rel(
        &mut self,
        pat: &RelPattern,
        mode: PathMode,
        selector: Option<Selector>,
    ) -> Result<BoundRel> {
        let mut candidates = Vec::new();
        if pat.types.is_empty() {
            candidates.extend(self.schema.rels.iter().map(|r| r.id));
        } else {
            // A type the graph has no table for holds no edges, so a
            // step asking for it walks nothing. That is the same
            // reading a label the dictionary does not hold gets: the
            // pattern matches nothing and the statement answers over
            // no rows rather than being refused.
            for ty in &pat.types {
                if let Some(rel) = self.schema.rel_by_name(ty) {
                    candidates.push(rel.id);
                }
            }
        }
        if let Some((min, max)) = pat.range {
            if min == Some(0) || max == Some(0) {
                return Err(invalid(
                    "zero-length hops are not supported, ranges start at 1".into(),
                ));
            }
            if min.zip(max).is_some_and(|(min, max)| max < min) {
                let (min, max) = (min.unwrap_or(0), max.unwrap_or(0));
                return Err(invalid(format!("hop range *{min}..{max} is empty")));
            }
            // What tames an unbounded WALK is a search that cannot walk
            // a cycle twice, and only the two that keep the least length
            // and nothing else are that. They are also the two the
            // executor answers by levelling the graph rather than by
            // walking the paths, and it can only do that when the lower
            // bound is one hop, so the same condition stands here: a
            // pattern this accepts is a pattern that search can answer.
            let tamed = selector.is_some_and(|s| s.bounds_a_walk()) && !min.is_some_and(|m| m > 1);
            if mode == PathMode::Walk && max.is_none() && !tamed {
                return Err(invalid(
                    "an unbounded WALK matches infinitely many paths; add an upper \
                     bound, or a shortest path selector with a lower bound of one hop"
                        .into(),
                ));
            }
        }
        let ty = if pat.range.is_some() {
            Type::List(Box::new(Type::Rel))
        } else {
            Type::Rel
        };
        let slot = match &pat.var {
            Some(name) => {
                if self.scope.contains_key(name) {
                    // Cypher's relationship uniqueness: a rel variable
                    // binds exactly once, and a second declaration of a
                    // name already taken is an invalid reference.
                    return Err(bad_reference(format!(
                        "relationship variable '{name}' is already bound"
                    )));
                }
                self.declare(name, ty)?
            }
            None => self.anon_slot(ty),
        };
        self.variables[slot].rel_tables = candidates.clone();
        let props = self.bind_props(&pat.props)?;
        // The WHERE inside the brackets is asked of one edge at a
        // time, so on a variable-length step the pattern's variable
        // means something different inside it than it does outside:
        // one edge in here, the list of them out there. It binds
        // against an edge slot of its own with the name pointed at
        // that slot for as long as the predicate is being bound.
        let (filter, edge_slot) = match &pat.filter {
            Some(expr) => {
                let edge = if pat.range.is_some() {
                    // It carries the pattern's name so that a plan
                    // listing spells the predicate the way it was
                    // written. Nothing resolves by that name, since
                    // the scope entry below is what the predicate
                    // reads and it is put back afterwards.
                    let edge = match &pat.var {
                        Some(name) => self.new_slot(name.clone(), Type::Rel),
                        None => self.anon_slot(Type::Rel),
                    };
                    self.variables[edge].rel_tables = candidates;
                    edge
                } else {
                    slot
                };
                let shadowed = pat
                    .var
                    .as_ref()
                    .map(|name| (name.clone(), self.scope.insert(name.clone(), edge)));
                let mut ctx = ExprCtx::new(false);
                let bound = self.bind_expr(expr, &mut ctx).map(|(value, _)| value);
                if let Some((name, previous)) = shadowed {
                    match previous {
                        Some(slot) => self.scope.insert(name, slot),
                        None => self.scope.remove(&name),
                    };
                }
                (Some(bound?), Some(edge))
            }
            None => (None, None),
        };
        Ok(BoundRel {
            slot,
            direction: pat.direction,
            range: pat.range,
            mode,
            selector,
            props,
            filter,
            edge_slot,
        })
    }

    fn bind_props(&mut self, props: &[(String, Expr)]) -> Result<Vec<(String, BoundExpr)>> {
        let mut bound = Vec::new();
        for (key, expr) in props {
            let mut ctx = ExprCtx::new(false);
            let (value, _) = self.bind_expr(expr, &mut ctx)?;
            bound.push((key.clone(), value));
        }
        Ok(bound)
    }

    /// Narrows node and rel candidate tables along one path: a rel only
    /// stays a candidate when its endpoints fit the adjacent nodes, and
    /// a node only stays when some candidate rel reaches it. One pass
    /// each way settles a chain. Var-length steps narrow only the rel
    /// by its types, since intermediate nodes are unconstrained.
    fn narrow_path(&mut self, path: &BoundPath) -> Result<()> {
        for _ in 0..2 {
            let mut left = path.start.slot;
            for (rel, node) in &path.steps {
                let right = node.slot;
                if rel.range.is_none() {
                    self.narrow_step(left, rel, right)?;
                }
                left = right;
            }
        }
        Ok(())
    }

    fn narrow_step(&mut self, left: usize, rel: &BoundRel, right: usize) -> Result<()> {
        // Which way a step reads a table depends on the table: an
        // undirected one answers both ways round and only the patterns
        // that admit an undirected edge may read it at all (GH02).
        let step_dir = |r: &RelDef| rel.direction.resolve(r.undirected);
        let fits = |r: &RelDef, from: &[u32], to: &[u32]| match step_dir(r) {
            Some(RelDirection::Out) => from.contains(&r.from) && to.contains(&r.to),
            Some(RelDirection::In) => from.contains(&r.to) && to.contains(&r.from),
            Some(_) => {
                (from.contains(&r.from) && to.contains(&r.to))
                    || (from.contains(&r.to) && to.contains(&r.from))
            }
            None => false,
        };
        let left_tables = self.variables[left].node_tables.clone();
        let right_tables = self.variables[right].node_tables.clone();
        let rels: Vec<&RelDef> = self.variables[rel.slot]
            .rel_tables
            .iter()
            .filter_map(|id| self.schema.rel_by_id(*id))
            .filter(|r| fits(r, &left_tables, &right_tables))
            .collect();
        let reaches = |node: u32, end: fn(&RelDef, RelDirection) -> (u32, u32)| {
            rels.iter().any(|r| {
                let Some(d) = step_dir(r) else {
                    return false;
                };
                let (a, b) = end(r, d);
                if d.both_ways() {
                    node == a || node == b
                } else {
                    node == a
                }
            })
        };
        let new_left: Vec<u32> = left_tables
            .iter()
            .copied()
            .filter(|&n| {
                reaches(n, |r, d| match d {
                    RelDirection::In => (r.to, r.from),
                    _ => (r.from, r.to),
                })
            })
            .collect();
        let new_right: Vec<u32> = right_tables
            .iter()
            .copied()
            .filter(|&n| {
                reaches(n, |r, d| match d {
                    RelDirection::In => (r.from, r.to),
                    _ => (r.to, r.from),
                })
            })
            .collect();
        // A step no table can carry, or an end no table can be, leaves
        // the slot with nothing to scan, and that is the same empty
        // answer a label the graph does not hold gives. Refusing here
        // would make the shape of the store decide whether a portable
        // statement runs at all, and would turn a graph that has not
        // been written into yet into an error rather than into no rows.
        for (slot, tables) in [(left, new_left), (right, new_right)] {
            self.variables[slot].node_tables = tables;
        }
        self.variables[rel.slot].rel_tables = rels.iter().map(|r| r.id).collect();
        Ok(())
    }

    // Expressions.

    /// The group a name stands for, if it stands for one. A name in
    /// scope is a slot and wins, since a group name is refused where one
    /// is already in scope and the other way round.
    fn group_of(&self, expr: &Expr) -> Option<GroupVar> {
        let Expr::Variable(name) = expr else {
            return None;
        };
        if self.scope.contains_key(name) {
            return None;
        }
        self.groups.get(name).cloned()
    }

    /// Binds a `LET` written inside an expression, GE03.
    ///
    /// The names go into the ordinary scope and come back out at the
    /// `END`, so the body reads one of them the way it reads a variable
    /// the match wrote, and everything that already works on a variable
    /// works on these. What they get is a slot of their own rather than
    /// a copy of the expression in each place they are read, which is
    /// the whole point of writing one: `LET n = f(x) IN n + n END` calls
    /// `f` once, and a substitution would call it twice.
    ///
    /// A name already in scope is refused rather than shadowed, which is
    /// `declare`'s rule and the standard's. It also keeps the slot
    /// numbering honest for the projection: a slot only ever goes up,
    /// an item's own slot is made after its expression is bound, so a
    /// projection item and a name written inside it can share a name
    /// without the sink confusing the two.
    fn bind_let_expr(
        &mut self,
        definitions: &[ast::LetItem],
        body: &Expr,
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        let mut values = Vec::with_capacity(definitions.len());
        for item in definitions {
            // Each definition is in scope for the ones after it, the
            // same as in the clause, so a pair where the second is
            // about the first reads the way it is written.
            let (expr, ty) = self.bind_expr(&item.expr, ctx)?;
            values.push((self.declare(&item.name, ty)?, expr));
        }
        let bound = self.bind_expr(body, ctx);
        for item in definitions {
            self.scope.remove(&item.name);
        }
        let (body, ty) = bound?;
        Ok((
            BoundExpr::Let {
                values,
                body: Box::new(body),
            },
            ty,
        ))
    }

    /// The handle a graph reference expression names, out of the
    /// graphs the schema was told about.
    ///
    /// A parameter is refused rather than resolved, and the reason is
    /// worth writing down: `USE $g` is settled when the statement runs
    /// because the clause is read before anything is bound, while an
    /// expression is bound once and read many times, so a parameter in
    /// this position would have to be a handle carried to the executor
    /// instead of a handle settled here. That is GE04's work and not
    /// this one's.
    fn resolve_graph_ref(&self, reference: &GraphRef) -> Result<GraphHandle> {
        if self.schema.graphs.is_empty() {
            return Err(ZuError::gql(
                codes::C42002,
                "a graph reference names a graph in the catalog, and this statement was compiled without one behind it".to_string(),
            ));
        }
        let by_id = |id: Option<u32>, which: &str| -> Result<GraphHandle> {
            id.and_then(|id| self.schema.graph_by_id(id))
                .cloned()
                .ok_or_else(|| {
                    ZuError::gql(
                        codes::C42002,
                        format!("{which} is no graph this statement can name"),
                    )
                })
        };
        match reference {
            GraphRef::Current => by_id(self.schema.working_graph, "the working graph"),
            GraphRef::Home => by_id(self.schema.home_graph, "the home graph"),
            GraphRef::Named(name) => {
                let schema = name.schema.as_deref().unwrap_or("/");
                self.schema
                    .graphs
                    .iter()
                    .find(|g| g.schema == schema && g.name == name.name)
                    .cloned()
                    .ok_or_else(|| {
                        ZuError::gql(
                            codes::C42002,
                            format!("'{}' is no graph in '{schema}'", name.name),
                        )
                    })
            }
            GraphRef::Param(name) => Err(not_yet(
                format!("a graph parameter in an expression, ${name}").as_str(),
            )),
        }
    }

    fn bind_expr(&mut self, expr: &Expr, ctx: &mut ExprCtx) -> Result<(BoundExpr, Type)> {
        match expr {
            Expr::Literal(lit) => {
                let ty = match lit {
                    Literal::Null => Type::Any,
                    Literal::Bool(_) => Type::Bool,
                    Literal::Int(_) => Type::Int,
                    Literal::Float(_) => Type::Float,
                    Literal::Str(_) => Type::Str,
                    // The static lattice has no temporal type yet, so a
                    // temporal literal is only known to be a value. It
                    // reaches the runtime typed and the checks that
                    // matter happen there.
                    Literal::Temporal(_) => Type::Any,
                };
                Ok((BoundExpr::Literal(lit.clone()), ty))
            }
            Expr::Param(name) => {
                let index = match self.params.iter().position(|p| p == name) {
                    Some(ix) => ix,
                    None => {
                        self.params.push(name.clone());
                        self.params.len() - 1
                    }
                };
                Ok((BoundExpr::Param(index), Type::Any))
            }
            Expr::Variable(name) => {
                if let Some(slot) = self.scope.get(name).copied() {
                    return Ok((BoundExpr::Var(slot), self.variables[slot].ty.clone()));
                }
                // A group variable stands for one element per repetition
                // of the stretch that bound it, which is a list, and the
                // elements are already in the row.
                if let Some(group) = self.groups.get(name) {
                    let ty = Type::List(Box::new(group.element.clone()));
                    let items = group.slots.iter().map(|&slot| BoundExpr::Var(slot));
                    return Ok((BoundExpr::List(items.collect()), ty));
                }
                if self.forked.contains(name) {
                    return Err(out_of_reach(name));
                }
                // A name the query around this one defined, which this
                // query may read: it makes the value query expression
                // correlated, and a correlated one is answered per row
                // rather than once. What stands here is the parameter
                // the row's value arrives in.
                if self.outer.iter().any(|n| n == name) {
                    return Ok((BoundExpr::Param(self.capture(name)), Type::Any));
                }
                Err(bad_reference(format!("variable '{name}' is not defined")))
            }
            // A property read on a group variable is read of each of
            // its elements, and the row holds the list of the answers
            // in the order the walk took them (ISO 22.7, feature GQ17).
            Expr::Property { base, key } if self.group_of(base).is_some() => {
                let slots = self.group_of(base).expect("the guard just matched").slots;
                let reads = slots.into_iter().map(|slot| BoundExpr::Property {
                    base: Box::new(BoundExpr::Var(slot)),
                    key: key.clone(),
                });
                Ok((
                    BoundExpr::List(reads.collect()),
                    Type::List(Box::new(Type::Any)),
                ))
            }
            Expr::Property { base, key } => {
                let (bound, ty) = self.bind_expr(base, ctx)?;
                if !matches!(ty, Type::Node | Type::Rel | Type::Record | Type::Any) {
                    return Err(invalid(format!(
                        "property access needs a node, rel, or record, got {ty} from {}",
                        text(base)
                    )));
                }
                Ok((
                    BoundExpr::Property {
                        base: Box::new(bound),
                        key: key.clone(),
                    },
                    Type::Any,
                ))
            }
            // A NOT over a block is the block asked the other way
            // round, which is one flag on the match rather than a
            // negation over the answer, so it is taken here before the
            // general unary path binds the block on its own.
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } if matches!(expr.as_ref(), Expr::Exists { .. }) => {
                let Expr::Exists { patterns, filter } = expr.as_ref() else {
                    unreachable!("the guard just matched the block");
                };
                Ok((self.bind_mark(patterns, filter, true)?, Type::Bool))
            }
            Expr::Unary { op, expr } => {
                let (bound, ty) = self.bind_expr(expr, ctx)?;
                let out = match op {
                    UnaryOp::Not => {
                        if !ty.is_bool() {
                            return Err(bad_type(format!("NOT needs a boolean, got {ty}")));
                        }
                        Type::Bool
                    }
                    UnaryOp::Neg => {
                        if !ty.is_numeric() {
                            return Err(bad_type(format!("unary minus needs a number, got {ty}")));
                        }
                        ty
                    }
                };
                Ok((
                    BoundExpr::Unary {
                        op: *op,
                        expr: Box::new(bound),
                    },
                    out,
                ))
            }
            Expr::Binary { op, lhs, rhs } => {
                let (bl, tl) = self.bind_expr(lhs, ctx)?;
                let (br, tr) = self.bind_expr(rhs, ctx)?;
                let ty = self.binary_type(*op, &tl, &tr)?;
                Ok((
                    BoundExpr::Binary {
                        op: *op,
                        lhs: Box::new(bl),
                        rhs: Box::new(br),
                    },
                    ty,
                ))
            }
            Expr::IsNull { expr, negated } => {
                let (bound, _) = self.bind_expr(expr, ctx)?;
                Ok((
                    BoundExpr::IsNull {
                        expr: Box::new(bound),
                        negated: *negated,
                    },
                    Type::Bool,
                ))
            }
            // Both spellings of the same question reach the same call.
            // A negated predicate is a NOT around it rather than a flag
            // on it, because the function has an answer for every string
            // and the negation has nothing to say about nulls that the
            // NOT does not already say.
            Expr::Normalize { expr, form } => {
                self.bind_normalize(Func::Normalize(*form), expr, ctx)
            }
            Expr::Trim {
                side,
                chars,
                source,
            } => self.bind_trim(*side, chars.as_deref(), source, ctx),
            Expr::Temporal { func, arg } => self.bind_temporal(*func, arg.as_deref(), ctx),
            // The instant, which is a value the run supplies and the
            // type of a temporal value is ANY here, the way a date
            // literal's is.
            Expr::Clock => Ok((BoundExpr::Clock, Type::Any)),
            Expr::DurationBetween { args, kind } => self.bind_duration_between(args, *kind, ctx),
            Expr::IsNormalized {
                expr,
                form,
                negated,
            } => {
                let (bound, ty) = self.bind_normalize(Func::IsNormalized(*form), expr, ctx)?;
                match negated {
                    true => Ok((
                        BoundExpr::Unary {
                            op: UnaryOp::Not,
                            expr: Box::new(bound),
                        },
                        ty,
                    )),
                    false => Ok((bound, ty)),
                }
            }
            Expr::IsTyped { expr, ty, negated } => {
                let (bound, _) = self.bind_expr(expr, ctx)?;
                Ok((
                    BoundExpr::IsTyped {
                        expr: Box::new(bound),
                        ty: ty.clone(),
                        negated: *negated,
                    },
                    Type::Bool,
                ))
            }
            // G110. Only an edge has a direction, so a node here is a
            // question with no answer rather than one answered no.
            Expr::IsDirected { expr, negated } => {
                let (bound, ty) = self.bind_expr(expr, ctx)?;
                if !matches!(ty, Type::Rel | Type::Any) {
                    return Err(bad_type(format!(
                        "IS DIRECTED asks about an edge, got {ty}"
                    )));
                }
                let undirected = self
                    .schema
                    .rels()
                    .iter()
                    .filter(|rel| rel.undirected)
                    .map(|rel| rel.id)
                    .collect();
                Ok((
                    BoundExpr::IsDirected {
                        expr: Box::new(bound),
                        undirected,
                        negated: *negated,
                    },
                    Type::Bool,
                ))
            }
            // G111. Nodes and edges both carry labels, so this one is
            // asked of either and refuses everything else.
            Expr::IsLabeled {
                expr,
                label,
                negated,
            } => {
                let (bound, ty) = self.bind_expr(expr, ctx)?;
                if !matches!(ty, Type::Node | Type::Rel | Type::Any) {
                    return Err(bad_type(format!(
                        "IS LABELED asks about a node or an edge, got {ty}"
                    )));
                }
                let rels = self
                    .schema
                    .rels()
                    .iter()
                    .filter(|rel| label_holds(label, &rel.name))
                    .map(|rel| rel.id)
                    .collect();
                Ok((
                    BoundExpr::IsLabeled {
                        expr: Box::new(bound),
                        node: self.compile_label(label)?,
                        rels,
                        negated: *negated,
                    },
                    Type::Bool,
                ))
            }
            // G112. A node and an edge, in that order, whichever end
            // the query asked about.
            Expr::IsEndpoint {
                node,
                rel,
                end,
                negated,
            } => {
                let (bound_node, node_ty) = self.bind_expr(node, ctx)?;
                let (bound_rel, rel_ty) = self.bind_expr(rel, ctx)?;
                if !matches!(node_ty, Type::Node | Type::Any) {
                    return Err(bad_type(format!(
                        "IS {} OF asks about a node, got {node_ty}",
                        end.text()
                    )));
                }
                if !matches!(rel_ty, Type::Rel | Type::Any) {
                    return Err(bad_type(format!(
                        "IS {} OF names an edge, got {rel_ty}",
                        end.text()
                    )));
                }
                let ends = self
                    .schema
                    .rels()
                    .iter()
                    .map(|rel| (rel.id, rel.from, rel.to))
                    .collect();
                Ok((
                    BoundExpr::IsEndpoint {
                        node: Box::new(bound_node),
                        rel: Box::new(bound_rel),
                        end: *end,
                        ends,
                        negated: *negated,
                    },
                    Type::Bool,
                ))
            }
            // G115. The element is a node or an edge for the reason a
            // property read is: nothing else has properties.
            Expr::PropertyExists { expr, key } => {
                let (bound, ty) = self.bind_expr(expr, ctx)?;
                if !matches!(ty, Type::Node | Type::Rel | Type::Any) {
                    return Err(bad_type(format!(
                        "PROPERTY_EXISTS asks about a node or an edge, got {ty}"
                    )));
                }
                Ok((
                    BoundExpr::PropertyExists {
                        expr: Box::new(bound),
                        key: key.clone(),
                    },
                    Type::Bool,
                ))
            }
            Expr::Call {
                name,
                distinct,
                star,
                args,
            } => self.bind_call(name, *distinct, *star, args, ctx),
            Expr::List(items) => {
                let mut bound = Vec::new();
                let mut element = Type::Any;
                for item in items {
                    let (b, t) = self.bind_expr(item, ctx)?;
                    element = if element == Type::Any || element == t {
                        t
                    } else {
                        Type::Any
                    };
                    bound.push(b);
                }
                Ok((BoundExpr::List(bound), Type::List(Box::new(element))))
            }
            // GV45, the record constructor. A field named twice is
            // refused here rather than resolved by a rule, because
            // both rules are defensible and neither is what the query
            // meant: `{a: 1, a: 2}` is a typo every time.
            Expr::Map(entries) => {
                let mut bound: Vec<(String, BoundExpr)> = Vec::new();
                for (key, value) in entries {
                    if bound.iter().any(|(seen, _)| seen == key) {
                        return Err(ZuError::gql(
                            codes::C42001,
                            format!("a record names the field '{key}' twice"),
                        ));
                    }
                    let (b, _) = self.bind_expr(value, ctx)?;
                    bound.push((key.clone(), b));
                }
                Ok((BoundExpr::Map(bound), Type::Record))
            }
            // GE06. The element types are checked here because a query
            // that wrote a number where an edge goes is wrong before it
            // is run, and the joining is left to the executor because
            // no type says which nodes an edge touches.
            Expr::Path(elements) => {
                let mut bound = Vec::new();
                for (ix, element) in elements.iter().enumerate() {
                    let (b, t) = self.bind_expr(element, ctx)?;
                    let wanted = if ix.is_multiple_of(2) {
                        Type::Node
                    } else {
                        Type::Rel
                    };
                    if t != wanted && t != Type::Any {
                        return Err(bad_type(format!(
                            "element {} of a path is {t} and a path alternates node, edge, node",
                            ix + 1
                        )));
                    }
                    bound.push(b);
                }
                Ok((BoundExpr::Path(bound), Type::Path))
            }
            // GE01. Which graph the words name is settled here and
            // never again, because the catalog does not move under a
            // statement and a reference that is the same for every row
            // has no business being worked out per row.
            Expr::GraphRef(reference) => {
                let handle = self.resolve_graph_ref(reference)?;
                Ok((BoundExpr::Graph(handle), Type::Graph))
            }
            Expr::Let { definitions, body } => self.bind_let_expr(definitions, body, ctx),
            // GE01. A searched branch has to be a truth and a simple
            // branch has to be something the subject can be compared
            // with, and the answer is one type for the whole
            // expression, worked out from the branches and the ELSE
            // together. A CASE with no ELSE can answer null, which is
            // no more than every other expression here can do.
            Expr::Case {
                subject,
                branches,
                otherwise,
            } => {
                let subject = match subject {
                    Some(expr) => Some(Box::new(self.bind_expr(expr, ctx)?.0)),
                    None => None,
                };
                let mut bound = Vec::with_capacity(branches.len());
                let mut answer = None;
                for (when, then) in branches {
                    let (when, when_ty) = self.bind_expr(when, ctx)?;
                    // A simple branch holds a value to compare and is
                    // checked the way `=` is, which is to say not at
                    // all: whether two types can be equal is a question
                    // the values answer.
                    if subject.is_none() && !when_ty.is_bool() {
                        return Err(bad_type(format!(
                            "a WHEN of a CASE written without a value is a condition, and this one is {when_ty}"
                        )));
                    }
                    let (then, then_ty) = self.bind_expr(then, ctx)?;
                    answer = Some(merged(answer, then_ty));
                    bound.push((when, then));
                }
                let otherwise = match otherwise {
                    Some(expr) => {
                        let (bound, ty) = self.bind_expr(expr, ctx)?;
                        answer = Some(merged(answer, ty));
                        Some(Box::new(bound))
                    }
                    None => None,
                };
                Ok((
                    BoundExpr::Case {
                        subject,
                        branches: bound,
                        otherwise,
                    },
                    answer.unwrap_or(Type::Any),
                ))
            }
            Expr::Coalesce(args) => {
                let mut bound = Vec::with_capacity(args.len());
                let mut answer = None;
                for arg in args {
                    let (arg, ty) = self.bind_expr(arg, ctx)?;
                    answer = Some(merged(answer, ty));
                    bound.push(arg);
                }
                Ok((BoundExpr::Coalesce(bound), answer.unwrap_or(Type::Any)))
            }
            Expr::NullIf { value, compared } => {
                let (value, value_ty) = self.bind_expr(value, ctx)?;
                let (compared, _) = self.bind_expr(compared, ctx)?;
                Ok((
                    BoundExpr::NullIf {
                        value: Box::new(value),
                        compared: Box::new(compared),
                    },
                    value_ty,
                ))
            }
            Expr::Cast { expr, ty } => {
                let (bound, _) = self.bind_expr(expr, ctx)?;
                Ok((
                    BoundExpr::Cast {
                        expr: Box::new(bound),
                        ty: ty.clone(),
                    },
                    plan_type(ty),
                ))
            }
            // Everything that could be lifted was lifted before this
            // ran, so a block reaching here is one in a place that
            // wants a value out of it, which is the mark. Where a mark
            // cannot go either, `bind_mark` is what says so.
            Expr::Exists { patterns, filter } => {
                Ok((self.bind_mark(patterns, filter, false)?, Type::Bool))
            }
            Expr::ExistsQuery(query) => self.bind_exists_query(query),
            Expr::ValueQuery(query) => self.bind_value_query(query),
        }
    }

    /// Records that this query reads `name` from the query around it,
    /// and answers with the parameter position the value arrives at.
    ///
    /// One position per name however often it is read, and the same
    /// position at every level of nesting, because the parameter list
    /// is the whole statement's: a query two levels in reads the value
    /// the query that has the slot wrote there.
    fn capture(&mut self, name: &str) -> usize {
        let param = capture_param(name);
        let index = match self.params.iter().position(|p| *p == param) {
            Some(ix) => ix,
            None => {
                self.params.push(param);
                self.params.len() - 1
            }
        };
        if !self.captures.iter().any(|c| c.param == index) {
            self.captures.push(Capture {
                name: name.to_string(),
                // Filled in by the binder around this one, which is
                // the one the name is in scope in.
                slot: usize::MAX,
                param: index,
            });
        }
        index
    }

    /// Gives the captures of a value query expression the slots they
    /// are read out of, which is a thing only this binder knows.
    ///
    /// The operands of a composite each captured on their own, and
    /// they run as one value query expression, so the lists are joined
    /// into the one the executor reads. A name this binder does not
    /// have is one from further out still: this query captures it too,
    /// at the same parameter position, and the entry leaves the list
    /// inside because the level that fills that position is this one.
    fn settle_captures(&mut self, bound: &mut BoundQuery) {
        let mut all = std::mem::take(&mut bound.captures);
        for joined in &mut bound.conjoined {
            for capture in std::mem::take(&mut joined.query.captures) {
                if !all.iter().any(|c| c.param == capture.param) {
                    all.push(capture);
                }
            }
        }
        all.retain_mut(|capture| match self.scope.get(&capture.name).copied() {
            Some(slot) => {
                capture.slot = slot;
                true
            }
            None => {
                self.capture(&capture.name);
                false
            }
        });
        bound.captures = all;
    }

    /// GQ18: `VALUE { ... }`, a whole query standing where one value
    /// belongs (ISO 20.6).
    ///
    /// It binds as a query of its own with a scope of its own, because
    /// it shares nothing with the query around it but the parameters
    /// and the names it reads out of the scope here. What comes back
    /// is the index the executor reads its answer at. A query that
    /// read no name is worked out once, before the plan above it runs;
    /// one that read a name is worked out per row of the query it
    /// stands in, and the names it read are its captures.
    fn bind_value_query(&mut self, query: &ast::Query) -> Result<(BoundExpr, Type)> {
        let bound = self.bind_nested(query, "VALUE")?;
        if bound.columns.len() != 1 {
            return Err(invalid(format!(
                "a VALUE query stands for one value, so it has to return one column, and this one returns {}",
                bound.columns.len()
            )));
        }
        let ty = match bound.clauses.last() {
            Some(BoundClause::Project { items, .. }) if items.len() == 1 => items[0].ty.clone(),
            _ => Type::Any,
        };
        Ok((self.keep_nested(bound), ty))
    }

    /// The third shape of the existence predicate (ISO 19.4):
    /// `EXISTS { ... }` around a whole query rather than a block of
    /// matches.
    ///
    /// It binds the way a value query does, since it is the same thing
    /// written for a different reason, and differs in what is read off
    /// the end. Nothing here asks what the query returns or how many
    /// columns it returns, because the answer is whether it answered a
    /// row at all, and that is why the query is marked: the executor
    /// stops it at the first row rather than running it out.
    fn bind_exists_query(&mut self, query: &ast::Query) -> Result<(BoundExpr, Type)> {
        let mut bound = self.bind_nested(query, "EXISTS")?;
        bound.exists = true;
        Ok((self.keep_nested(bound), Type::Bool))
    }

    /// Binds the query a `VALUE` or an `EXISTS` was written around,
    /// with the scope of the query here behind it.
    fn bind_nested(&mut self, query: &ast::Query, word: &str) -> Result<BoundQuery> {
        if query.use_graph.is_some() {
            return Err(invalid(format!(
                "a query written inside {word} runs in the graph the statement runs in, so it may not carry a USE of its own"
            )));
        }
        let mut params = std::mem::take(&mut self.params);
        let mut outer = self.outer.clone();
        outer.extend(self.scope.keys().cloned());
        let mut bound = bind_body(&query.body, self.schema, &mut params, &outer)?;
        self.params = params;
        // A name the query around this one already has is read out of
        // that row, so a pattern in here that writes the same name
        // would be two different elements under one word: the read
        // would be the row's and the pattern would match anything. The
        // block form is where a pattern and the row share an element,
        // so that is what the message points at.
        if let Some(name) = shadowed(&bound, &outer) {
            return Err(invalid(format!(
                "'{name}' is a name the query around this {word} already wrote, so a pattern in here writing it again would mean two elements at once: rename it, or write the block form, where the pattern and the row are the same element"
            )));
        }
        self.settle_captures(&mut bound);
        let mut wrote = false;
        bound.walk(&mut |q| wrote |= q.clauses.iter().any(writes));
        if wrote {
            return Err(invalid(format!(
                "a query written inside {word} is read where a value belongs, so it may not write to the graph"
            )));
        }
        // A match written several ways runs as parts of the statement,
        // and a query written where a value belongs is run whole where
        // the value is read, so there is nowhere to put the parts.
        let mut forked = false;
        bound.walk(&mut |q| forked |= q.clauses.iter().any(forks));
        if forked {
            return Err(invalid(format!(
                "a match written several ways inside {word} is a walk per alternative, and a query read where a value belongs runs as one walk; write the alternatives as operands of a composite query"
            )));
        }
        Ok(bound)
    }

    /// Files a bound nested query where the executor reaches it and
    /// answers the expression that reads it by index.
    fn keep_nested(&mut self, bound: BoundQuery) -> BoundExpr {
        let reads = bound.captures.iter().map(|c| c.slot).collect();
        self.scalars.push(bound);
        BoundExpr::Scalar {
            ix: self.scalars.len() - 1,
            reads,
        }
    }

    /// Whether an expression reads a group variable, which is what tells
    /// an aggregate over a group from an aggregate over the rows.
    ///
    /// The two are written alike and mean different things, so what
    /// picks between them is the argument: `SUM(y.step)` where `y` is a
    /// group folds that row's group, and `SUM(b.step)` where `b` is one
    /// node folds the column. Only the two shapes a group can be read
    /// in are looked for, the group itself and a property of it, because
    /// those are the two the fold below knows how to write out.
    fn reads_a_group(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Property { base, .. } => self.group_of(base).is_some(),
            other => self.group_of(other).is_some(),
        }
    }

    /// An aggregate over a group variable (ISO 20.9, feature GE09).
    ///
    /// It folds the elements one row bound rather than the rows a clause
    /// answered, so it is a scalar expression and not an aggregate: the
    /// projection around it does not group, and the answer is one value
    /// per row the way an addition is. The elements are read straight
    /// out of the slots the walk filled, so the fold builds no list to
    /// walk down.
    fn bind_horizontal(
        &mut self,
        func: Func,
        distinct: bool,
        star: bool,
        arg: &Expr,
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        if star {
            return Err(invalid(
                "count(*) counts the rows a clause answered, and a group is one row's, \
                 so write count of the group itself"
                    .into(),
            ));
        }
        let (bound, ty) = self.bind_expr(arg, ctx)?;
        let BoundExpr::List(args) = bound else {
            return Err(invalid(
                "an aggregate over a group variable folds the elements that variable \
                 stands for, so what is written inside it has to be the group or a \
                 property of it"
                    .into(),
            ));
        };
        let element = match ty {
            Type::List(element) => *element,
            other => other,
        };
        // The same rule the vertical aggregate answers by, read against
        // the element type rather than the column's. A property read
        // answers a value the static lattice does not know, which is
        // what a group of properties folds to as well, and the runtime
        // checks what it was handed.
        let out = functions::signature(func)
            .expect("an aggregate has a signature")
            .ret
            .of(std::slice::from_ref(&element));
        Ok((
            BoundExpr::Fold {
                func,
                distinct,
                args,
            },
            out,
        ))
    }

    /// The one argument the two normalization functions take, bound and
    /// checked. It is a string or it is nothing: GQL casts nothing to a
    /// string on its own, and a number has no normal form to be in.
    fn bind_normalize(
        &mut self,
        func: Func,
        expr: &Expr,
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        let (bound, ty) = self.bind_expr(expr, ctx)?;
        if !ty.is_str() {
            return Err(bad_type(format!(
                "{}() needs a string, got {ty}",
                crate::plan::func_name(func)
            )));
        }
        let out = match func {
            Func::IsNormalized(_) => Type::Bool,
            _ => Type::Str,
        };
        Ok((
            BoundExpr::Call {
                func,
                sig: functions::row_of(func).expect("a normalization function has a row"),
                distinct: false,
                star: false,
                args: vec![bound],
            },
            out,
        ))
    }

    fn bind_call(
        &mut self,
        name: &str,
        distinct: bool,
        star: bool,
        args: &[Expr],
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        let at = crate::functions::lookup(name)
            .ok_or_else(|| bad_reference(format!("unknown function '{name}'")))?;
        self.bind_row(at, name, distinct, star, args, ctx)
    }

    /// `TRIM(LEADING 'x' FROM s)`, ISO 20.24 and GF06. The explicit
    /// form is not written like a call and is one once it is read, so
    /// the end it names picks the row and the rest is the checking
    /// every other call gets.
    fn bind_trim(
        &mut self,
        side: TrimSide,
        chars: Option<&Expr>,
        source: &Expr,
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        let trim = match side {
            TrimSide::Leading => Trim::Leading,
            TrimSide::Trailing => Trim::Trailing,
            TrimSide::Both => Trim::Both,
        };
        let at = functions::row_of(Func::Trim(trim)).expect("a trim has a row");
        let mut args = vec![source.clone()];
        args.extend(chars.cloned());
        self.bind_row(at, "trim", false, false, &args, ctx)
    }

    /// The temporal value functions of ISO 20.27 and 20.29. Each is a
    /// cut of one value, so the call the binder writes takes that value
    /// as its argument and the row says which cut it is: the string the
    /// statement wrote, or the instant it is running at where it wrote
    /// none. The clock is read where the statement runs rather than
    /// here, which is what keeps a cached plan from carrying the time
    /// it was compiled at, and it is why the two forms are one row and
    /// one kernel.
    fn bind_temporal(
        &mut self,
        func: TemporalFn,
        arg: Option<&Expr>,
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        let at = functions::row_of(Func::Temporal(func)).expect("a temporal function has a row");
        let arg = arg.cloned().unwrap_or(Expr::Clock);
        self.bind_row(at, func.word(), false, false, &[arg], ctx)
    }

    /// `DURATION_BETWEEN(a, b) [YEAR TO MONTH | DAY TO SECOND]`, ISO
    /// 20.28. The qualifier picks the row and the rest is the checking
    /// every other call gets, which is how a call written with two
    /// arguments and one written with three reach the same refusal.
    fn bind_duration_between(
        &mut self,
        args: &[Expr],
        kind: Option<DurationKind>,
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        let kind = kind.unwrap_or(DurationKind::DayTime);
        let at = functions::row_of(Func::DurationBetween(kind))
            .expect("a datetime subtraction has a row");
        self.bind_row(at, "duration_between", false, false, args, ctx)
    }

    /// A call whose row is already settled: the arity, the argument
    /// types, the folding and the answer's type, which every call gets
    /// however its name was written or whether it was written at all.
    fn bind_row(
        &mut self,
        at: u16,
        name: &str,
        distinct: bool,
        star: bool,
        args: &[Expr],
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        let sig = functions::row(at).expect("a row number came from the table");
        let func = sig.func;
        if sig.aggregate
            && let [arg] = args
            && self.reads_a_group(arg)
        {
            return self.bind_horizontal(func, distinct, star, arg, ctx);
        }
        if sig.aggregate {
            if !ctx.allow_aggregates {
                return Err(invalid(format!(
                    "aggregate {name}() is only allowed in WITH and RETURN items"
                )));
            }
            if ctx.in_aggregate {
                return Err(invalid(format!("aggregate {name}() cannot nest")));
            }
            ctx.saw_aggregate = true;
        }
        if star && !sig.star {
            return Err(invalid(format!("only count(*) takes *, not {name}(*)")));
        }
        // G113 and G114 are the two that take a list of elements rather
        // than one value, so their signature says how few will do and
        // the rest say exactly how many.
        match sig.arity {
            functions::Arity::Exactly(want) => {
                let want = if star { 0 } else { want };
                if args.len() != want {
                    return Err(invalid(format!(
                        "{name}() takes {want} argument(s), got {}",
                        args.len()
                    )));
                }
            }
            functions::Arity::AtLeast(least) => {
                if star || args.len() < least {
                    return Err(bad_type(format!(
                        "{}() needs at least {} elements",
                        sig.name,
                        spelled(least)
                    )));
                }
            }
            functions::Arity::Between(least, most) => {
                if star || args.len() < least || args.len() > most {
                    return Err(invalid(format!(
                        "{name}() takes {least} or {most} argument(s), got {}",
                        args.len()
                    )));
                }
            }
        }
        let was_in_aggregate = ctx.in_aggregate;
        if sig.aggregate {
            ctx.in_aggregate = true;
        }
        let mut bound = Vec::new();
        let mut arg_tys = Vec::new();
        for arg in args {
            let (b, t) = self.bind_expr(arg, ctx)?;
            arg_tys.push(t);
            bound.push(b);
        }
        ctx.in_aggregate = was_in_aggregate;
        for (at, ty) in arg_tys.iter().enumerate() {
            if !sig.arg.accepts_at(at, ty) {
                return Err(bad_type(format!("{}() {}, got {ty}", sig.name, sig.needs)));
            }
        }
        // A deterministic function over what the statement wrote is
        // answered here, once, rather than on every row it would reach.
        if let Some(lit) = functions::fold(sig, &bound) {
            let ty = sig.ret.of(&arg_tys);
            return Ok((BoundExpr::Literal(lit), ty));
        }
        // The answer's type is the signature's rule read against
        // what arrived: a fixed type for most of these, and for the
        // ones that hand back what they were given, the arguments'.
        let out = sig.ret.of(&arg_tys);
        Ok((
            BoundExpr::Call {
                func,
                sig: at,
                distinct,
                star,
                args: bound,
            },
            out,
        ))
    }

    fn binary_type(&self, op: BinaryOp, lhs: &Type, rhs: &Type) -> Result<Type> {
        let numeric = |lhs: &Type, rhs: &Type| -> Result<Type> {
            if !lhs.is_numeric() || !rhs.is_numeric() {
                return Err(bad_type(format!(
                    "{op:?} needs numbers, got {lhs} and {rhs}"
                )));
            }
            Ok(match (lhs, rhs) {
                (Type::Int, Type::Int) => Type::Int,
                (Type::Any, _) | (_, Type::Any) => Type::Any,
                _ => Type::Float,
            })
        };
        match op {
            BinaryOp::Or | BinaryOp::Xor | BinaryOp::And => {
                if !lhs.is_bool() || !rhs.is_bool() {
                    return Err(bad_type(format!(
                        "{op:?} needs booleans, got {lhs} and {rhs}"
                    )));
                }
                Ok(Type::Bool)
            }
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge => Ok(Type::Bool),
            BinaryOp::Add => match (lhs, rhs) {
                (Type::Str, Type::Str) => Ok(Type::Str),
                (Type::Str, Type::Any) | (Type::Any, Type::Str) => Ok(Type::Any),
                _ => numeric(lhs, rhs),
            },
            BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => numeric(lhs, rhs),
            BinaryOp::In => {
                if !matches!(rhs, Type::List(_) | Type::Any) {
                    return Err(bad_type(format!("IN needs a list on the right, got {rhs}")));
                }
                Ok(Type::Bool)
            }
            BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => {
                if !lhs.is_str() || !rhs.is_str() {
                    return Err(bad_type(format!(
                        "{op:?} needs strings, got {lhs} and {rhs}"
                    )));
                }
                Ok(Type::Bool)
            }
            // ISO 20.23. Strings on both sides and a string out. It is
            // written apart from the plus for the reason the standard
            // writes it apart: a plus over two numbers adds them and a
            // plus over two strings joins them, so a query whose
            // operands the lattice does not know yet says which of the
            // two it meant by which operator it wrote.
            BinaryOp::Concat => {
                if !lhs.is_str() || !rhs.is_str() {
                    return Err(bad_type(format!("|| joins strings, got {lhs} and {rhs}")));
                }
                Ok(Type::Str)
            }
        }
    }
}

/// A label expression as a query would write it, for the text that
/// names a column and titles an operator.
fn label_text(expr: &ast::LabelExpr) -> String {
    match expr {
        ast::LabelExpr::Label(name) => name.clone(),
        ast::LabelExpr::Wildcard => "%".into(),
        ast::LabelExpr::Not(inner) => format!("!{}", label_text(inner)),
        ast::LabelExpr::And(lhs, rhs) => format!("({}&{})", label_text(lhs), label_text(rhs)),
        ast::LabelExpr::Or(lhs, rhs) => format!("({}|{})", label_text(lhs), label_text(rhs)),
    }
}

/// Renders an expression compactly: the column name for unaliased
/// RETURN items and the operator text EXPLAIN will reuse.
pub fn text(expr: &Expr) -> String {
    match expr {
        Expr::Literal(Literal::Null) => "NULL".into(),
        Expr::Literal(Literal::Bool(b)) => if *b { "TRUE" } else { "FALSE" }.into(),
        Expr::Literal(Literal::Int(v)) => v.to_string(),
        Expr::Literal(Literal::Float(v)) => v.to_string(),
        Expr::Literal(Literal::Str(s)) => format!("'{s}'"),
        Expr::Literal(Literal::Temporal(t)) => t.to_string(),
        Expr::Param(p) => format!("${p}"),
        Expr::Variable(v) => v.clone(),
        Expr::Property { base, key } => format!("{}.{key}", text(base)),
        Expr::Unary { op, expr } => match op {
            UnaryOp::Not => format!("NOT {}", text(expr)),
            UnaryOp::Neg => format!("-{}", text(expr)),
        },
        Expr::Binary { op, lhs, rhs } => {
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
                BinaryOp::Concat => "||",
            };
            format!("{} {symbol} {}", text(lhs), text(rhs))
        }
        Expr::IsNull { expr, negated } => {
            if *negated {
                format!("{} IS NOT NULL", text(expr))
            } else {
                format!("{} IS NULL", text(expr))
            }
        }
        Expr::Normalize { expr, form } => {
            format!("NORMALIZE({}, {})", text(expr), form.name())
        }
        Expr::Trim {
            side,
            chars,
            source,
        } => {
            let side = match side {
                TrimSide::Leading => "LEADING ",
                TrimSide::Trailing => "TRAILING ",
                TrimSide::Both => "BOTH ",
            };
            let chars = match chars {
                Some(chars) => format!("{} ", text(chars)),
                None => String::new(),
            };
            format!("TRIM({side}{chars}FROM {})", text(source))
        }
        Expr::Temporal { func, arg } => match arg {
            Some(arg) => format!("{}({})", func.word().to_uppercase(), text(arg)),
            None => func.word().to_uppercase(),
        },
        // The word above is what a reader sees, since the instant is
        // the binder's own doing and naming a column after it would
        // name it after a thing the query never wrote.
        Expr::Clock => "CURRENT_TIMESTAMP".into(),
        Expr::DurationBetween { args, kind } => {
            let written: Vec<String> = args.iter().map(text).collect();
            let qualifier = match kind {
                Some(DurationKind::YearMonth) => " YEAR TO MONTH",
                Some(DurationKind::DayTime) => " DAY TO SECOND",
                None => "",
            };
            format!("DURATION_BETWEEN({}){qualifier}", written.join(", "))
        }
        Expr::IsNormalized {
            expr,
            form,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!("{} IS {not}NORMALIZED {}", text(expr), form.name())
        }
        Expr::IsTyped { expr, ty, negated } => {
            let not = if *negated { "NOT " } else { "" };
            format!("{} IS {not}TYPED {ty}", text(expr))
        }
        Expr::IsDirected { expr, negated } => {
            let not = if *negated { "NOT " } else { "" };
            format!("{} IS {not}DIRECTED", text(expr))
        }
        Expr::IsLabeled {
            expr,
            label,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!("{} IS {not}LABELED {}", text(expr), label_text(label))
        }
        Expr::IsEndpoint {
            node,
            rel,
            end,
            negated,
        } => {
            let not = if *negated { "NOT " } else { "" };
            format!("{} IS {not}{} OF {}", text(node), end.text(), text(rel))
        }
        Expr::PropertyExists { expr, key } => {
            format!("PROPERTY_EXISTS({}, {key})", text(expr))
        }
        Expr::Call {
            name,
            distinct,
            star,
            args,
        } => {
            let inner = if *star {
                "*".to_string()
            } else {
                let rendered: Vec<String> = args.iter().map(text).collect();
                rendered.join(", ")
            };
            if *distinct {
                format!("{name}(DISTINCT {inner})")
            } else {
                format!("{name}({inner})")
            }
        }
        Expr::List(items) => {
            let rendered: Vec<String> = items.iter().map(text).collect();
            format!("[{}]", rendered.join(", "))
        }
        Expr::Map(entries) => {
            let rendered: Vec<String> = entries
                .iter()
                .map(|(k, v)| format!("{k}: {}", text(v)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
        Expr::Path(elements) => {
            let rendered: Vec<String> = elements.iter().map(text).collect();
            format!("PATH [{}]", rendered.join(", "))
        }
        Expr::GraphRef(reference) => match reference {
            GraphRef::Current => "CURRENT_PROPERTY_GRAPH".into(),
            GraphRef::Home => "HOME_PROPERTY_GRAPH".into(),
            GraphRef::Named(name) => match &name.schema {
                Some(schema) => format!("GRAPH {schema}/{}", name.name),
                None => format!("GRAPH {}", name.name),
            },
            GraphRef::Param(name) => format!("GRAPH ${name}"),
        },
        Expr::Let { definitions, body } => {
            let rendered: Vec<String> = definitions
                .iter()
                .map(|item| format!("{} = {}", item.name, text(&item.expr)))
                .collect();
            format!("LET {} IN {} END", rendered.join(", "), text(body))
        }
        Expr::Cast { expr, ty } => format!("CAST({} AS {ty})", text(expr)),
        Expr::Case {
            subject,
            branches,
            otherwise,
        } => {
            let mut out = "CASE".to_string();
            if let Some(subject) = subject {
                out.push(' ');
                out.push_str(&text(subject));
            }
            for (when, then) in branches {
                out.push_str(&format!(" WHEN {} THEN {}", text(when), text(then)));
            }
            if let Some(otherwise) = otherwise {
                out.push_str(&format!(" ELSE {}", text(otherwise)));
            }
            out.push_str(" END");
            out
        }
        Expr::Coalesce(args) => {
            let rendered: Vec<String> = args.iter().map(text).collect();
            format!("COALESCE({})", rendered.join(", "))
        }
        Expr::NullIf { value, compared } => {
            format!("NULLIF({}, {})", text(value), text(compared))
        }
        // The patterns are not rendered: this text names a column and
        // titles an operator, and a whole match inside one of those
        // reads worse than the word does.
        Expr::Exists { .. } | Expr::ExistsQuery(_) => "EXISTS { ... }".into(),
        Expr::ValueQuery(_) => "VALUE { ... }".into(),
    }
}

/// The one type an expression with several answers is of, folded over
/// the answers a branch at a time.
///
/// Two branches of the same type answer that type, an integer branch
/// beside a float branch answers a float the way arithmetic over the
/// two does, and anything else answers `ANY`, which is what the binder
/// says where it knows a value arrives and not what kind. It is the
/// rule a list literal uses, with the numbers added, because a CASE
/// over a count and an average is a query somebody meant.
fn merged(seen: Option<Type>, next: Type) -> Type {
    match seen {
        None => next,
        Some(seen) if seen == next => seen,
        Some(Type::Int) if next == Type::Float => Type::Float,
        Some(Type::Float) if next == Type::Int => Type::Float,
        _ => Type::Any,
    }
}

/// The plan-time type a cast target lands in.
///
/// [`Type`] is the coarse type the rest of the binder reasons in and it
/// has one integer and one float, so every width in the tower answers
/// the same here. Nothing is lost: the width lives on in the bound
/// cast, which is what the executor reads.
fn plan_type(ty: &LogicalType) -> Type {
    match ty.base() {
        LogicalType::Bool => Type::Bool,
        LogicalType::Int { .. } => Type::Int,
        LogicalType::Float { .. } => Type::Float,
        LogicalType::Str { .. } => Type::Str,
        _ => Type::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn a_summary_scales_to_the_table_until_it_no_longer_describes_it() {
        let built = |edges| ColorSummary {
            counts: vec![10, 90],
            triples: vec![(0, 1, 400, 40)],
            epoch: 3,
            edges,
        };
        let sum = built(1000);
        assert_eq!(sum.scale(1000), 1.0);
        assert_eq!(sum.scale(2500), 2.5, "grown, so every count is short");
        assert_eq!(sum.scale(500), 0.5, "shrunk, so every count is long");
        assert!(sum.fresh_enough(1000) && sum.fresh_enough(8000) && sum.fresh_enough(125));
        assert!(!sum.fresh_enough(8001), "past the limit going up");
        assert!(!sum.fresh_enough(124), "past the limit going down");

        // A file written before the stamp existed carries no counts to
        // take a ratio of, and neither does an empty table. Both read
        // as no drift rather than as infinite drift, so an old file
        // plans exactly as it did.
        assert_eq!(built(0).scale(1000), 1.0);
        assert!(built(0).fresh_enough(1000));
        assert_eq!(sum.scale(0), 1.0);
        assert!(sum.fresh_enough(0));
    }

    #[test]
    fn the_weighted_mean_counts_a_hub_once_per_edge() {
        // One hub with three out-edges and one in-edge, plus three
        // leaves holding one out-edge each, so six edges in all. Four
        // sources hold out-edges and the plain mean is 1.5, but a row
        // that already holds an edge sits on the hub half the time.
        let out = DegreeNorms {
            l1: 6.0,
            l2: 18f64.sqrt(),
            l3: 30f64.cbrt(),
            linf: 3.0,
        };
        let inn = DegreeNorms {
            l1: 6.0,
            l2: 10f64.sqrt(),
            l3: 12f64.cbrt(),
            linf: 3.0,
        };
        let stats = DegreeStats {
            out,
            inn,
            cross: 3.0,
        };
        // Arriving by in-degree and leaving by out-degree is the two
        // hop chain, and 6 rows times 3/6 is the 3 paths there are.
        assert_eq!(stats.weighted(1, 0), 0.5);
        // Arriving and leaving by the same side is l2 squared over l1,
        // and squaring a root does not land on 3 exactly.
        assert!((stats.weighted(0, 0) - 3.0).abs() < 1e-9);
        assert_eq!(stats.side(1), inn);
    }

    #[test]
    fn the_top_sum_never_promises_more_than_the_whole_sequence() {
        let n = DegreeNorms {
            l1: 6.0,
            l2: 18f64.sqrt(),
            l3: 30f64.cbrt(),
            linf: 3.0,
        };
        // One row can only take the largest degree there is, and the
        // whole table can only hand back every edge it holds.
        assert_eq!(n.top_sum(1.0), 3.0);
        assert_eq!(n.top_sum(1000.0), 6.0);
        assert!(!DegreeNorms::default().known());
    }

    /// person(0), place(1), person-KNOWS->person, person-IS_LOCATED_IN->place,
    /// place-PART_OF->place, mirroring the LDBC SF1 core.
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
                RelDef {
                    id: 4,
                    name: "PART_OF".into(),
                    from: 1,
                    to: 1,
                    edge_count: 1400,
                    undirected: false,
                },
            ],
        )
        .expect("schema")
    }

    fn bound(source: &str) -> BoundQuery {
        bind(&parse(source).expect("parse"), &schema()).expect("bind")
    }

    fn bind_err(source: &str) -> String {
        bind(&parse(source).expect("parse"), &schema())
            .expect_err("should fail")
            .to_string()
    }

    fn var<'a>(q: &'a BoundQuery, name: &str) -> &'a VarDef {
        q.variables
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("variable {name}"))
    }

    /// A chain binds to one list of clauses: the result of a statement
    /// in the middle is a projection like any other, and the columns
    /// the caller gets are the last statement's.
    #[test]
    fn a_next_chain_binds_to_one_pipeline() {
        let q = bound(
            "MATCH (n:Person) RETURN n AS p NEXT MATCH (p)-[:KNOWS]->(f:Person) RETURN f.name AS name",
        );
        assert!(
            matches!(
                q.clauses.as_slice(),
                [
                    BoundClause::Match { .. },
                    BoundClause::Project { .. },
                    BoundClause::Match { .. },
                    BoundClause::Project { .. },
                ]
            ),
            "{:?}",
            q.clauses
        );
        assert_eq!(q.columns, ["name"], "the chain answers what it ends with");
    }

    /// The statement behind a NEXT reads its input by name, so two
    /// items of one name in the result it reads is a question with no
    /// answer. The last RETURN has no such reader and duplicate column
    /// names there are ordinary.
    #[test]
    fn a_result_that_feeds_a_chain_may_not_name_a_column_twice() {
        let err = bind_err("MATCH (n:Person) RETURN n.id AS x, n.age AS x NEXT RETURN x");
        assert!(err.contains("duplicate name 'x' in RETURN"), "{err}");
        let q = bound("MATCH (n:Person) RETURN n.id AS x, n.age AS x");
        assert_eq!(q.columns, ["x", "x"]);
    }

    /// A mid-chain RETURN names what it projects the way any RETURN
    /// does, so an item written without an alias keeps the name the
    /// text gives it rather than being turned away for having none,
    /// which is what a WITH would do with it.
    #[test]
    fn a_chained_result_names_an_unaliased_item_the_way_return_does() {
        let q = bound("MATCH (n:Person) RETURN n.name NEXT RETURN 1 AS one");
        let BoundClause::Project { items, .. } = &q.clauses[1] else {
            panic!("the first RETURN");
        };
        assert_eq!(items[0].name, "n.name");
        assert_eq!(q.columns, ["one"]);
    }

    /// What NEXT hands over is a result table and nothing else, so the
    /// variables the statement in front of it matched are gone. A chain
    /// is not the same thing as writing the clauses one after another.
    #[test]
    fn what_the_chain_hands_over_is_the_result_and_nothing_else() {
        let err = bind_err("MATCH (n:Person) RETURN n.name AS name NEXT RETURN n");
        assert!(err.contains("variable 'n' is not defined"), "{err}");
    }

    #[test]
    fn a_top_value_is_estimated_by_its_own_frequency() {
        // The SF1 gender column: two values over ten thousand rows, so
        // uniformity and the top list happen to agree, and the browser
        // column, where they do not.
        let gender = ColStats {
            rows: 10_000,
            ndv: 2,
            top: vec![(b"female".to_vec(), 6000), (b"male".to_vec(), 4000)],
            bounds: Vec::new(),
        };
        assert_eq!(gender.eq_selectivity(b"female"), 0.6);
        assert_eq!(gender.eq_selectivity(b"male"), 0.4);
        assert_eq!(gender.eq_average(), 0.5, "a parameter gets the average");
        // Every value is in the list, so a miss is a value the column
        // does not hold, and that is worth one row rather than none.
        assert_eq!(gender.eq_selectivity(b"other"), 1.0 / 10_000.0);
    }

    #[test]
    fn a_value_off_the_top_list_shares_what_the_list_left_behind() {
        // 1000 rows, 101 distinct. The one top value takes 500 rows,
        // so the other 100 values split the remaining 500.
        let skewed = ColStats {
            rows: 1000,
            ndv: 101,
            top: vec![(b"hub".to_vec(), 500)],
            bounds: Vec::new(),
        };
        assert_eq!(skewed.eq_selectivity(b"hub"), 0.5);
        assert_eq!(skewed.eq_selectivity(b"tail"), 5.0 / 1000.0);
        assert!(
            skewed.eq_selectivity(b"tail") < skewed.eq_average(),
            "taking the skew out first leaves the tail below the average"
        );
        assert_eq!(ColStats::default().eq_selectivity(b"anything"), 0.0);
    }

    #[test]
    fn a_range_reads_off_the_bucket_boundaries() {
        // Four buckets over the values 0, 10, 20, 30, 40.
        let stat = ColStats {
            rows: 400,
            ndv: 400,
            top: Vec::new(),
            bounds: (0..5).map(|b| vec![b * 10u8]).collect(),
        };
        assert_eq!(stat.below(&[0]), Some(0.0), "at or under the low bound");
        assert_eq!(stat.below(&[45]), Some(1.0), "over the high bound");
        assert_eq!(stat.below(&[20]), Some(0.375), "one whole bucket and half");
        assert_eq!(stat.below(&[35]), Some(0.875), "three whole and half");
        assert_eq!(
            ColStats::default().below(&[1]),
            None,
            "no buckets, no answer, and the caller keeps its fallback"
        );
    }

    #[test]
    fn point_lookup_binds_tables_params_and_columns() {
        let q = bound("MATCH (n:Person {id: $personId}) RETURN n.firstName AS firstName");
        assert_eq!(var(&q, "n").node_tables, [0]);
        assert_eq!(q.params, ["personId"]);
        assert_eq!(q.columns, ["firstName"]);
        let BoundClause::Match { patterns, .. } = &q.clauses[0] else {
            panic!("MATCH");
        };
        assert_eq!(patterns[0].start.props[0].0, "id");
        assert_eq!(patterns[0].start.props[0].1, BoundExpr::Param(0));
    }

    /// The point of giving each way its own slots. The two ways here
    /// reach different tables under the one name, so the name may hold
    /// either, and a single slot narrowed by both would have been
    /// narrowed to nothing.
    #[test]
    fn each_way_of_a_fork_binds_its_own_slots() {
        let q = bound(
            "MATCH (a:Person)-[:IS_LOCATED_IN]->(b) | (a:Person)-[:KNOWS]->(b) \
             RETURN count(*) AS n",
        );
        let BoundClause::Fork {
            branches,
            distinct,
            carry,
            base,
        } = &q.clauses[0]
        else {
            panic!("a match written two ways binds to a fork");
        };
        assert!(distinct, "one bar is the union");
        assert_eq!(*base, 0, "nothing was in scope in front of it");
        assert_eq!(branches.len(), 2);
        let held: Vec<usize> = branches.iter().map(|branch| branch.slots[1]).collect();
        assert_ne!(held[0], held[1], "the two ways bind 'b' apart");
        assert_eq!(q.variables[held[0]].node_tables, [1], "a place");
        assert_eq!(q.variables[held[1]].node_tables, [0], "a person");
        assert_eq!(
            q.variables[carry[1]].node_tables,
            [0, 1],
            "and the name the clauses after the fork read may hold either"
        );
    }

    #[test]
    fn an_exists_block_becomes_a_match_of_its_own() {
        let q = bound(
            "MATCH (a:Person) WHERE a.id > 1 AND EXISTS { MATCH (a)-[:KNOWS]->(b) } \
             RETURN a.id AS id",
        );
        let BoundClause::Match { kind, filter, .. } = &q.clauses[0] else {
            panic!("MATCH");
        };
        assert_eq!(*kind, MatchKind::Required);
        assert!(filter.is_some(), "the conjunct that is not a block stayed");
        let BoundClause::Match {
            kind,
            patterns,
            filter,
        } = &q.clauses[1]
        else {
            panic!("the block runs after the clause it was written in");
        };
        assert_eq!(*kind, MatchKind::Semi);
        assert_eq!(patterns[0].steps.len(), 1);
        assert!(filter.is_none());
    }

    #[test]
    fn a_negated_block_binds_the_other_way_round() {
        let q = bound("MATCH (a:Person) WHERE NOT EXISTS { MATCH (a)-[:KNOWS]->(b) } RETURN a");
        let BoundClause::Match { kind, filter, .. } = &q.clauses[0] else {
            panic!("MATCH");
        };
        assert_eq!(*kind, MatchKind::Required);
        assert!(filter.is_none(), "the WHERE was the block and nothing else");
        let BoundClause::Match { kind, .. } = &q.clauses[1] else {
            panic!("the block");
        };
        assert_eq!(*kind, MatchKind::Anti);
    }

    #[test]
    fn two_blocks_keep_their_written_order() {
        let q = bound(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) } \
             AND NOT EXISTS { MATCH (a)-[:IS_LOCATED_IN]->(p) } RETURN a",
        );
        let kinds: Vec<MatchKind> = q
            .clauses
            .iter()
            .filter_map(|c| match c {
                BoundClause::Match { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            [MatchKind::Required, MatchKind::Semi, MatchKind::Anti]
        );
    }

    #[test]
    fn a_block_variable_does_not_outlive_the_block() {
        let e = bind_err("MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) } RETURN b.id");
        assert!(e.contains("variable 'b' is not defined"), "got: {e}");
        // The same name in two blocks is two variables and neither is
        // the other's, so writing it twice is not a redeclaration.
        bound(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) } \
             AND EXISTS { MATCH (a)<-[:KNOWS]-(b) } RETURN a",
        );
    }

    #[test]
    fn a_block_sees_the_scope_around_it() {
        // b is the outer variable here, so the block joins to it rather
        // than introducing a second one.
        let q = bound(
            "MATCH (a:Person)-[:KNOWS]->(b) WHERE EXISTS { MATCH (b)-[:KNOWS]->(a) } RETURN a",
        );
        let BoundClause::Match { patterns, .. } = &q.clauses[1] else {
            panic!("the block");
        };
        let outer = q.variables.iter().position(|v| v.name == "b").expect("b");
        assert_eq!(patterns[0].start.slot, outer);
    }

    #[test]
    fn a_block_after_a_with_reads_the_projected_names() {
        let q = bound(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg WHERE deg > 1 \
             AND EXISTS { MATCH (a)-[:KNOWS]->(c) } RETURN a.id AS id",
        );
        let BoundClause::Match { kind, .. } = &q.clauses[2] else {
            panic!(
                "the block runs after the projection, got {:?}",
                q.clauses[2]
            );
        };
        assert_eq!(*kind, MatchKind::Semi);
        let a = q.variables.iter().position(|v| v.name == "a").expect("a");
        let BoundClause::Match { patterns, .. } = &q.clauses[2] else {
            unreachable!()
        };
        assert_eq!(patterns[0].start.slot, a, "the block joined to the group");

        // A name the projection dropped is out of scope by the time the
        // block runs, so writing it in the block's pattern introduces a
        // variable of the block's own the way any fresh name does.
        let q = bound(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg \
             WHERE EXISTS { MATCH (b)-[:KNOWS]->(c) } RETURN a.id AS id",
        );
        let BoundClause::Match { patterns, .. } = &q.clauses[2] else {
            panic!("the block");
        };
        let outer = q
            .variables
            .iter()
            .position(|v| v.name == "b")
            .expect("the outer b");
        assert_ne!(patterns[0].start.slot, outer);
    }

    #[test]
    fn a_block_is_not_a_value() {
        let e = bind_err("MATCH (a:Person) RETURN EXISTS { MATCH (a)-[:KNOWS]->(b) } AS friendly");
        assert!(e.contains("EXISTS is a match and not a value"), "got: {e}");
        // A block inside another block is asked for a value the same
        // way, and there is nowhere for the mark it would need to go.
        let e = bind_err(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b)              WHERE b.id = 0 OR EXISTS { MATCH (b)-[:KNOWS]->(c) } } RETURN a",
        );
        assert!(e.contains("EXISTS is a match and not a value"), "got: {e}");
        // An OPTIONAL MATCH's WHERE decides that match, so a mark there
        // would have to be answered inside the bracket.
        let e = bind_err(
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b)              WHERE b.id = 0 OR EXISTS { MATCH (b)-[:KNOWS]->(c) } RETURN a",
        );
        assert!(e.contains("EXISTS is a match and not a value"), "got: {e}");
        // So does a WHERE hanging off a projection: what it filters is
        // the group, and the block reads a row that is gone by then.
        let e = bind_err(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS deg              WHERE deg > 1 OR EXISTS { MATCH (a)-[:KNOWS]->(c) } RETURN a",
        );
        assert!(e.contains("EXISTS is a match and not a value"), "got: {e}");
    }

    #[test]
    fn a_block_under_an_or_becomes_a_mark() {
        let q =
            bound("MATCH (a:Person) WHERE a.id = 0 OR EXISTS { MATCH (a)-[:KNOWS]->(b) } RETURN a");
        let BoundClause::Match { kind, filter, .. } = &q.clauses[0] else {
            panic!("MATCH");
        };
        assert_eq!(*kind, MatchKind::Required);
        assert!(filter.is_none(), "the whole WHERE moved past the mark");
        let BoundClause::Match { kind, patterns, .. } = &q.clauses[1] else {
            panic!("the mark");
        };
        let MatchKind::Mark { slot, negated } = *kind else {
            panic!("a block under an OR is a mark, got {kind:?}");
        };
        assert!(!negated);
        assert_eq!(patterns[0].steps.len(), 1);
        assert_eq!(q.variables[slot].ty, Type::Bool);
        // The predicate runs after the mark is written, and it reads
        // the slot where the block was.
        let BoundClause::Match {
            kind,
            patterns,
            filter: Some(filter),
        } = &q.clauses[2]
        else {
            panic!("the predicate that reads the mark");
        };
        assert_eq!(*kind, MatchKind::Required);
        assert!(patterns.is_empty(), "nothing left to match here");
        let BoundExpr::Binary { op, rhs, .. } = filter else {
            panic!("the OR");
        };
        assert_eq!(*op, BinaryOp::Or);
        assert_eq!(**rhs, BoundExpr::Var(slot));
    }

    #[test]
    fn a_negated_mark_folds_the_not_in() {
        let q = bound(
            "MATCH (a:Person) WHERE a.id = 0 OR NOT EXISTS { MATCH (a)-[:KNOWS]->(b) } RETURN a",
        );
        let BoundClause::Match { kind, .. } = &q.clauses[1] else {
            panic!("the mark");
        };
        let MatchKind::Mark { slot, negated } = *kind else {
            panic!("a mark, got {kind:?}");
        };
        assert!(negated, "the NOT went into what gets written down");
        let BoundClause::Match {
            filter: Some(BoundExpr::Binary { rhs, .. }),
            ..
        } = &q.clauses[2]
        else {
            panic!("the predicate");
        };
        // Not a NOT around the read: the column already holds it.
        assert_eq!(**rhs, BoundExpr::Var(slot));
    }

    #[test]
    fn two_marks_under_one_where_keep_their_order() {
        let q = bound(
            "MATCH (a:Person) WHERE EXISTS { MATCH (a)-[:KNOWS]->(b) }              OR EXISTS { MATCH (a)-[:IS_LOCATED_IN]->(p) } RETURN a",
        );
        let kinds: Vec<MatchKind> = q
            .clauses
            .iter()
            .filter_map(|c| match c {
                BoundClause::Match { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect();
        assert_eq!(kinds.len(), 4, "the pattern, two marks, the predicate");
        assert!(matches!(kinds[1], MatchKind::Mark { .. }));
        assert!(matches!(kinds[2], MatchKind::Mark { .. }));
        let (MatchKind::Mark { slot: first, .. }, MatchKind::Mark { slot: second, .. }) =
            (kinds[1], kinds[2])
        else {
            unreachable!("just matched");
        };
        assert!(first < second, "written order is the order they run in");
    }

    #[test]
    fn unlabeled_nodes_narrow_through_rel_types() {
        // IS_LOCATED_IN only goes Person to Place, so both ends resolve.
        let q = bound("MATCH (a)-[:IS_LOCATED_IN]->(b) RETURN a, b");
        assert_eq!(var(&q, "a").node_tables, [0]);
        assert_eq!(var(&q, "b").node_tables, [1]);
    }

    #[test]
    fn untyped_rel_narrows_from_node_labels() {
        // Place to Place leaves PART_OF as the only rel candidate.
        let q = bound("MATCH (a:Place)-[r]->(b:Place) RETURN r");
        assert_eq!(var(&q, "r").rel_tables, [4]);
    }

    #[test]
    fn inbound_direction_narrows_the_other_way() {
        let q = bound("MATCH (a)<-[:IS_LOCATED_IN]-(b) RETURN a, b");
        assert_eq!(var(&q, "a").node_tables, [1]);
        assert_eq!(var(&q, "b").node_tables, [0]);
    }

    #[test]
    fn chain_narrowing_settles_both_directions() {
        // The KNOWS step pins m to Person even though only the second
        // step mentions a table, and the backward pass pins a too.
        let q = bound("MATCH (a)-[:KNOWS]->(m)-[:IS_LOCATED_IN]->(c) RETURN a, m, c");
        assert_eq!(var(&q, "a").node_tables, [0]);
        assert_eq!(var(&q, "m").node_tables, [0]);
        assert_eq!(var(&q, "c").node_tables, [1]);
    }

    #[test]
    fn impossible_patterns_match_nothing_rather_than_being_refused() {
        // A step no table runs, a label the graph does not hold, and a
        // type no table is named by. Each asks for something the graph
        // has nowhere, and the answer to that is no rows: the variable
        // is left with nothing to scan and the statement still runs.
        for source in [
            "MATCH (a:Place)-[:KNOWS]->(b) RETURN a",
            "MATCH (a:Nope) RETURN a",
            "MATCH (a)-[:NOPE]->(b) RETURN a",
        ] {
            let q = bound(source);
            assert!(
                var(&q, "a").node_tables.is_empty(),
                "{source} left something to scan"
            );
        }
    }

    #[test]
    fn var_length_rels_bind_as_lists_and_skip_node_narrowing() {
        let q = bound("MATCH (a:Person)-[r:KNOWS*1..3]-(b) RETURN b");
        assert_eq!(var(&q, "r").ty, Type::List(Box::new(Type::Rel)));
        assert_eq!(var(&q, "r").rel_tables, [2]);
        // b keeps both candidates: intermediate hops are unconstrained.
        assert_eq!(var(&q, "b").node_tables, [0, 1]);
        assert!(bind_err("MATCH (a)-[*0..2]->(b) RETURN a").contains("zero-length"));
        assert!(bind_err("MATCH (a)-[*3..2]->(b) RETURN a").contains("is empty"));
    }

    #[test]
    fn with_scoping_replaces_visibility() {
        let q = bound(
            "MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS friends \
             WHERE friends > 5 RETURN a.firstName AS name, friends",
        );
        let BoundClause::Project { items, filter, .. } = &q.clauses[1] else {
            panic!("WITH");
        };
        assert!(!items[0].aggregate && items[1].aggregate);
        assert!(filter.is_some());
        assert_eq!(q.columns, ["name", "friends"]);
        // b fell out of scope at WITH.
        let e = bind_err("MATCH (a)-[:KNOWS]->(b) WITH a AS x RETURN b");
        assert!(e.contains("'b' is not defined"), "got: {e}");
    }

    #[test]
    fn with_items_need_aliases_and_unique_names() {
        assert!(bind_err("MATCH (a) WITH a.x RETURN 1").contains("needs an alias"));
        assert!(bind_err("MATCH (a) WITH a.x AS v, a.y AS v RETURN v").contains("duplicate name"));
        // A plain variable passes through unaliased.
        let q = bound("MATCH (a:Person) WITH a RETURN a");
        assert_eq!(q.columns, ["a"]);
    }

    #[test]
    fn return_star_expands_scope_in_slot_order() {
        let q = bound("MATCH (a:Person)-[r:KNOWS]->(b) RETURN *");
        assert_eq!(q.columns, ["a", "r", "b"]);
        assert!(bind_err("RETURN *").contains("at least one variable"));
    }

    #[test]
    fn unaliased_return_items_name_themselves() {
        let q = bound("MATCH (a:Person) RETURN a.firstName, count(*), 1 + 2");
        assert_eq!(q.columns, ["a.firstName", "count(*)", "1 + 2"]);
    }

    #[test]
    fn aggregate_placement_is_enforced() {
        assert!(bind_err("MATCH (a) WHERE count(a) > 1 RETURN a").contains("only allowed in"));
        assert!(bind_err("MATCH (a) RETURN count(count(a))").contains("cannot nest"));
        assert!(bind_err("MATCH (a) RETURN sum(*)").contains("only count(*)"));
        assert!(bind_err("MATCH (a) RETURN nope(a)").contains("unknown function 'nope'"));
    }

    #[test]
    fn expression_types_check() {
        assert!(bind_err("MATCH (a) WHERE 1 + 2 RETURN a").contains("needs a boolean"));
        assert!(bind_err("MATCH (a) RETURN NOT 1 AS x").contains("needs a boolean"));
        assert!(bind_err("MATCH (a) RETURN 'x' - 1 AS x").contains("needs numbers"));
        assert!(bind_err("MATCH (a) RETURN 1 IN 2 AS x").contains("needs a list"));
        assert!(bind_err("MATCH (a) RETURN a STARTS WITH 'x' AS y").contains("needs strings"));
        assert!(bind_err("MATCH (a) RETURN (1).x AS y").contains("property access"));
        assert!(bind_err("MATCH (a) RETURN a LIMIT 'ten'").contains("LIMIT needs an integer"));
        assert!(bind_err("UNWIND 5 AS x RETURN x").contains("UNWIND needs a list"));
    }

    #[test]
    fn unwind_takes_the_element_type() {
        let q = bound("UNWIND [1, 2, 3] AS x RETURN x * 2 AS y");
        assert_eq!(var(&q, "x").ty, Type::Int);
        let BoundClause::Project { items, .. } = &q.clauses[1] else {
            panic!("RETURN");
        };
        assert_eq!(items[0].ty, Type::Int);
    }

    #[test]
    fn variable_reuse_rules() {
        // A node variable reused across patterns joins on one slot.
        let q = bound("MATCH (a:Person)-[:KNOWS]->(b), (a)-[:IS_LOCATED_IN]->(c) RETURN c");
        let BoundClause::Match { patterns, .. } = &q.clauses[0] else {
            panic!("MATCH");
        };
        assert_eq!(patterns[0].start.slot, patterns[1].start.slot);
        // Rel variables bind exactly once.
        let e = bind_err("MATCH (a)-[r:KNOWS]->(b)-[r:KNOWS]->(c) RETURN a");
        assert!(e.contains("'r' is already bound"), "got: {e}");
        // A slot cannot switch kinds.
        let e = bind_err("MATCH (a:Person) MATCH (b)-[a:KNOWS]->(c) RETURN b");
        assert!(e.contains("already"), "got: {e}");
    }

    #[test]
    fn order_by_sees_pre_projection_vars_without_aggregation() {
        bound("MATCH (a:Person) WITH a.firstName AS name ORDER BY a.lastName RETURN name");
        // With aggregation the underlying variables are gone.
        let e =
            bind_err("MATCH (a:Person)-[:KNOWS]->(b) WITH a, count(b) AS c ORDER BY b.x RETURN c");
        assert!(e.contains("'b' is not defined"), "got: {e}");
    }

    #[test]
    fn params_dedupe_by_name_in_first_use_order() {
        let q = bound(
            "MATCH (n:Person {id: $personId}) WHERE n.age > $min AND n.id <> $personId \
             RETURN n LIMIT $min",
        );
        assert_eq!(q.params, ["personId", "min"]);
    }

    #[test]
    fn path_variables_type_as_path() {
        let q = bound("MATCH p = (a:Person)-[:KNOWS]->(b) RETURN p");
        assert_eq!(var(&q, "p").ty, Type::Path);
        assert!(bind_err("MATCH p = (a) MATCH p = (b) RETURN p").contains("already defined"));
    }

    /// The same two tables over a graph that declares labels: a person
    /// may be an Employee or a Manager, a place may be an Employee,
    /// and nothing may be both a Person and a Place.
    fn labeled_schema() -> Schema {
        Schema::with_labels(
            vec![
                NodeDef {
                    id: 0,
                    name: "Person".into(),
                    node_count: 9000,
                    labels: vec![0, 2, 3],
                },
                NodeDef {
                    id: 1,
                    name: "Place".into(),
                    node_count: 1400,
                    labels: vec![1, 2],
                },
            ],
            vec![RelDef {
                id: 2,
                name: "KNOWS".into(),
                from: 0,
                to: 0,
                edge_count: 180_000,
                undirected: false,
            }],
            vec![
                "Person".into(),
                "Place".into(),
                "Employee".into(),
                "Manager".into(),
            ],
        )
        .expect("schema")
    }

    fn start_node(q: &BoundQuery) -> &BoundNode {
        let BoundClause::Match { patterns, .. } = &q.clauses[0] else {
            panic!("a MATCH");
        };
        &patterns[0].start
    }

    #[test]
    fn a_label_set_narrows_the_tables_and_leaves_the_rest_to_the_bitset() {
        let schema = labeled_schema();
        let bound = |s: &str| bind(&parse(s).expect("parse"), &schema).expect("bind");
        // A table's own label is what every one of its rows carries, so
        // narrowing to that table answers the whole pattern.
        let q = bound("MATCH (n:Person) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0]);
        assert_eq!(start_node(&q).label, None);
        // A declared label is one some rows of the table carry, so the
        // tables narrow to the ones that declare it and the bit stays
        // as a test.
        let q = bound("MATCH (n:Employee) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0, 1]);
        assert_eq!(start_node(&q).label, Some(LabelTest::All(1 << 2)));
        // Both at once: Place does not declare Manager and drops out,
        // and Person's own label needs no test.
        let q = bound("MATCH (n:Person:Manager:Employee) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0]);
        assert_eq!(start_node(&q).label, Some(LabelTest::All(1 << 3 | 1 << 2)));
        // A label the graph does not have, and a set no table declares.
        // Neither is a refusal: nothing carries what was asked for, so
        // the variable is left with no table to scan and the statement
        // answers over no rows.
        let q = bound("MATCH (n:Nope) RETURN n");
        assert!(var(&q, "n").node_tables.is_empty());
        let q = bound("MATCH (n:Person:Place) RETURN n");
        assert!(var(&q, "n").node_tables.is_empty());
    }

    #[test]
    fn a_label_expression_folds_against_what_each_table_declares() {
        let schema = labeled_schema();
        let bound = |s: &str| bind(&parse(s).expect("parse"), &schema).expect("bind");
        let all = |bits: u64| Some(LabelTest::All(bits));
        // Neither table settles a negated secondary label, so both
        // stay and the row answers.
        let q = bound("MATCH (n:!Employee) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0, 1]);
        assert_eq!(
            start_node(&q).label,
            Some(LabelTest::Not(Box::new(LabelTest::All(1 << 2))))
        );
        // A table's own label answers one side of the negation
        // outright: Place declares no Person so every row of it is a
        // non-Person, and Person's rows are all Person.
        let q = bound("MATCH (n:!Person) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [1]);
        assert_eq!(start_node(&q).label, None);
        // A disjunction one table always satisfies and the other only
        // sometimes keeps both tables and the test.
        let q = bound("MATCH (n:Person|Employee) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0, 1]);
        assert_eq!(
            start_node(&q).label,
            Some(LabelTest::Or(
                Box::new(LabelTest::All(1)),
                Box::new(LabelTest::All(1 << 2))
            ))
        );
        // Every table satisfies one side or the other, so nothing is
        // left to ask.
        let q = bound("MATCH (n:Person|Place) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0, 1]);
        assert_eq!(start_node(&q).label, None);
        // A node has a label by construction, so `%` is no test, and
        // it drops out of a conjunction the same way.
        let q = bound("MATCH (n:%) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0, 1]);
        assert_eq!(start_node(&q).label, None);
        let q = bound("MATCH (n:%&Employee) RETURN n");
        assert_eq!(start_node(&q).label, all(1 << 2));
        // The table's own bit comes out of the mask because every row
        // of the one table left carries it.
        let q = bound("MATCH (n:Person&!Manager) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0]);
        assert_eq!(
            start_node(&q).label,
            Some(LabelTest::Not(Box::new(LabelTest::All(1 << 3))))
        );
        // Nothing satisfies a negated wildcard, so every table drops
        // out and the pattern matches nothing.
        let q = bound("MATCH (n:!%) RETURN n");
        assert!(var(&q, "n").node_tables.is_empty());
    }

    #[test]
    fn a_rel_type_settles_a_label_the_pattern_alone_left_open() {
        let schema = labeled_schema();
        let bound = |s: &str| bind(&parse(s).expect("parse"), &schema).expect("bind");
        // On its own this keeps both tables and a runtime test.
        let q = bound("MATCH (n:Person|Employee) RETURN n");
        assert!(start_node(&q).label.is_some());
        // KNOWS runs Person to Person, so once the endpoint narrowing
        // has dropped Place the disjunction is true on every row left
        // and the test goes with it.
        let q = bound("MATCH (n:Person|Employee)-[:KNOWS]->(m) RETURN n");
        assert_eq!(var(&q, "n").node_tables, [0]);
        assert_eq!(start_node(&q).label, None);
    }

    /// The element a pattern creates is a variable like any other, in
    /// the order the patterns were written.
    #[test]
    fn an_insert_binds_the_elements_its_patterns_wrote() {
        let q = bound("INSERT (x:Person {name: $who}), (y:Person) RETURN x, y");
        assert_eq!(q.params, ["who"]);
        let BoundClause::Insert { nodes, carry, .. } = &q.clauses[0] else {
            panic!("INSERT");
        };
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].table, 0);
        assert_eq!(nodes[0].props.len(), 1);
        assert!(nodes[1].props.is_empty());
        // Nothing ran before the write, so the run carries nothing
        // across it and the elements are all the rows hold.
        assert!(carry.is_empty());
        assert_eq!(var(&q, "x").node_tables, [0]);
        assert_eq!(q.variables[nodes[0].slot].name, "x");
        assert_eq!(q.variables[nodes[1].slot].name, "y");
    }

    /// A write after a match runs for each row the match answered, and
    /// what the run carries across it is the row: the slots in scope
    /// where the write is written, and nothing the write itself makes.
    #[test]
    fn an_insert_after_a_match_carries_the_row_across_the_write() {
        let q = bound(
            "MATCH (a:Person)-[:IS_LOCATED_IN]->(p:Place) INSERT (a)-[k:KNOWS]->(b:Person) RETURN p, k",
        );
        let BoundClause::Insert { nodes, rels, carry } = &q.clauses[1] else {
            panic!("INSERT");
        };
        let slot = |name: &str| q.variables.iter().position(|v| v.name == name).expect(name);
        assert_eq!(carry, &[slot("a"), slot("p")]);
        assert_eq!(nodes.len(), 1, "only b is written");
        assert_eq!(nodes[0].slot, slot("b"));
        assert_eq!(rels[0].src, slot("a"));
        assert_eq!(rels[0].dst, slot("b"));
    }

    /// An end a match found has to be in the table the edge runs from,
    /// and a match that narrowed it to something else is refused with
    /// both tables named.
    #[test]
    fn an_end_a_match_found_is_checked_against_the_table_the_edge_runs_between() {
        bound("MATCH (p:Place) INSERT (a:Person)-[:IS_LOCATED_IN]->(p)");
        let e = bind_err("MATCH (p:Place) INSERT (p)-[:KNOWS]->(b:Person)");
        assert!(e.contains("leaves an element of 'Person'"), "got: {e}");
        assert!(e.contains("'p' is in 'Place'"), "got: {e}");
    }

    /// What the write surface does not do yet says so by name, so that
    /// a statement written against a later milestone fails with the
    /// piece it is waiting on rather than with a parse error.
    #[test]
    fn the_parts_of_insert_that_are_not_in_yet_say_which_part() {
        let e = bind_err("INSERT p = (a:Person)");
        assert!(e.contains("path"), "got: {e}");
        assert!(e.contains("not implemented yet"), "got: {e}");
    }

    /// An edge carries what the pattern wrote on it, bound the way an
    /// element's properties are, so a value there is an expression like
    /// any other rather than a literal.
    #[test]
    fn an_edge_written_under_insert_carries_what_the_pattern_wrote() {
        let q = bound("INSERT (a:Person)-[k:KNOWS {since: $year}]->(b:Person)");
        let BoundClause::Insert { rels, .. } = &q.clauses[0] else {
            panic!("INSERT");
        };
        assert_eq!(rels[0].props.len(), 1);
        assert_eq!(rels[0].props[0].0, "since");
        assert_eq!(rels[0].props[0].1, BoundExpr::Param(0));
    }

    /// An edge written under `INSERT` runs between two elements the
    /// same clause creates, and the arrow says which of them it leaves.
    #[test]
    fn an_insert_writes_an_edge_between_the_elements_it_made() {
        let q = bound("INSERT (a:Person)-[k:KNOWS]->(b:Person) RETURN k");
        let BoundClause::Insert { nodes, rels, .. } = &q.clauses[0] else {
            panic!("INSERT");
        };
        assert_eq!(nodes.len(), 2);
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].table, 2);
        assert_eq!(rels[0].src, nodes[0].slot);
        assert_eq!(rels[0].dst, nodes[1].slot);
        assert_eq!(q.variables[rels[0].slot].name, "k");
        assert_eq!(var(&q, "k").rel_tables, [2]);

        // A backwards arrow is the same edge written from the other
        // end, so the ends swap and nothing else does.
        let q = bound("INSERT (a:Person)<-[:KNOWS]-(b:Person)");
        let BoundClause::Insert { nodes, rels, .. } = &q.clauses[0] else {
            panic!("INSERT");
        };
        assert_eq!(rels[0].src, nodes[1].slot);
        assert_eq!(rels[0].dst, nodes[0].slot);
    }

    /// A name already standing for an element is that element again,
    /// which is how a statement writes two edges off one node.
    #[test]
    fn an_end_that_names_something_already_written_is_that_element() {
        let q = bound(
            "INSERT (a:Person)-[:KNOWS]->(b:Person), (a)-[:IS_LOCATED_IN]->(c:Place) RETURN a",
        );
        let BoundClause::Insert { nodes, rels, .. } = &q.clauses[0] else {
            panic!("INSERT");
        };
        assert_eq!(nodes.len(), 3, "a is written once and pointed at twice");
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[1].src, nodes[0].slot);
        assert_eq!(rels[1].dst, nodes[2].slot);
        // An element already described is not described again, whether
        // the second description is a label or a property.
        let e = bind_err("INSERT (a:Person)-[:KNOWS]->(b:Person), (a:Person)");
        assert!(e.contains("already described"), "got: {e}");
        let e = bind_err("INSERT (a:Person)-[:KNOWS]->(b:Person), (a {name: 'x'})-[:KNOWS]->(b)");
        assert!(e.contains("already described"), "got: {e}");
        // An edge is not an end of another edge.
        let e = bind_err("INSERT (a:Person)-[k:KNOWS]->(b:Person), (k)-[:KNOWS]->(a)");
        assert!(e.contains("not an end"), "got: {e}");
    }

    /// What an edge is written into is a table, and the ends have to be
    /// in the tables that table runs between.
    #[test]
    fn an_edge_goes_in_one_table_between_the_two_it_runs_between() {
        let e = bind_err("INSERT (a:Person)-[]->(b:Person)");
        assert!(e.contains("names none"), "got: {e}");
        let e = bind_err("INSERT (a:Person)-[:KNOWS|IS_LOCATED_IN]->(b:Person)");
        assert!(e.contains("names 2"), "got: {e}");
        let e = bind_err("INSERT (a:Person)-[:WORKS_AT]->(b:Person)");
        assert!(e.contains("no edge table"), "got: {e}");
        let e = bind_err("INSERT (a:Person)-[:IS_LOCATED_IN]->(b:Person)");
        assert!(e.contains("arrives at an element of 'Place'"), "got: {e}");
        let e = bind_err("INSERT (a:Person)-[:KNOWS]-(b:Person)");
        assert!(e.contains("holds directed edges"), "got: {e}");
        let e = bind_err("INSERT (a:Person)~[:KNOWS]~(b:Person)");
        assert!(e.contains("holds directed edges"), "got: {e}");
        let e = bind_err("INSERT (a:Person)-[:KNOWS*2]->(b:Person)");
        assert!(e.contains("hop range"), "got: {e}");
        let e = bind_err("INSERT ANY SHORTEST (a:Person)-[:KNOWS]->(b:Person)");
        assert!(e.contains("selector"), "got: {e}");
    }

    /// A table of undirected edges takes the pattern that has no arrow
    /// and only that one, because an arrow written on one of its edges
    /// would say something the table does not record.
    #[test]
    fn an_undirected_table_is_written_with_the_pattern_that_has_no_arrow() {
        let schema = Schema::new(
            vec![NodeDef {
                id: 0,
                name: "Person".into(),
                node_count: 9000,
                labels: Vec::new(),
            }],
            vec![RelDef {
                id: 1,
                name: "MARRIED_TO".into(),
                from: 0,
                to: 0,
                edge_count: 4000,
                undirected: true,
            }],
        )
        .expect("schema");
        let bind_here = |s: &str| bind(&parse(s).expect("parse"), &schema);

        let q = bind_here("INSERT (a:Person)~[m:MARRIED_TO]~(b:Person)").expect("bind");
        let BoundClause::Insert { nodes, rels, .. } = &q.clauses[0] else {
            panic!("INSERT");
        };
        assert_eq!(rels[0].table, 1);
        assert_eq!(rels[0].src, nodes[0].slot);
        assert_eq!(rels[0].dst, nodes[1].slot);

        let e = bind_here("INSERT (a:Person)-[:MARRIED_TO]->(b:Person)")
            .expect_err("an arrow on an edge that has none")
            .to_string();
        assert!(e.contains("holds undirected edges"), "got: {e}");
    }

    /// An element goes in one table, and the pattern has to say which:
    /// a label expression that names none, or names more than one, is
    /// not a table.
    #[test]
    fn an_insert_wants_one_plain_label_naming_a_table() {
        assert!(bind_err("INSERT (x)").contains("label"));
        let e = bind_err("INSERT (x:Person|Place)");
        assert!(e.contains("one name"), "got: {e}");
        let e = bind_err("INSERT (x:Company)");
        assert!(e.contains("Company"), "got: {e}");
    }

    #[test]
    fn path_mode_and_selector_rules() {
        // An unbounded WALK is infinite; a selector or a bound tames it.
        let e = bind_err("MATCH WALK (a:Person)-[:KNOWS*]->(b) RETURN b");
        assert!(e.contains("unbounded WALK"), "got: {e}");
        bound("MATCH WALK (a:Person)-[:KNOWS*1..3]->(b) RETURN b");
        bound("MATCH ANY SHORTEST WALK (a:Person)-[:KNOWS*]->(b) RETURN b");
        // What tames it is keeping the least length and nothing else. A
        // counted selector keeps a second length as well, and under WALK
        // that is the least length plus a lap of a cycle, so there is
        // still no end of them.
        let e = bind_err("MATCH SHORTEST 2 WALK (a:Person)-[:KNOWS*]->(b) RETURN b");
        assert!(e.contains("unbounded WALK"), "got: {e}");
        let e = bind_err("MATCH ANY 2 WALK (a:Person)-[:KNOWS*]->(b) RETURN b");
        assert!(e.contains("unbounded WALK"), "got: {e}");
        bound("MATCH SHORTEST 2 WALK (a:Person)-[:KNOWS*1..4]->(b) RETURN b");
        // A selector without a variable-length rel selects nothing.
        let e = bind_err("MATCH ANY SHORTEST (a:Person)-[:KNOWS]->(b) RETURN b");
        assert!(e.contains("variable-length"), "got: {e}");
        // A lower bound above one hop asks for the least length among
        // the paths that are long enough, which is a question the walk
        // answers and the levelling search cannot, so it binds and the
        // executor picks the search rather than being refused here.
        bound("MATCH ALL SHORTEST (a:Person)-[:KNOWS*2..3]->(b) RETURN b");
        // The plain modes carry through to the bound rel.
        let q = bound("MATCH ACYCLIC (a:Person)-[:KNOWS*1..3]->(b) RETURN b");
        let BoundClause::Match { patterns, .. } = &q.clauses[0] else {
            panic!("MATCH");
        };
        let (rel, _) = &patterns[0].steps[0];
        assert_eq!(rel.mode, PathMode::Acyclic);
        assert_eq!(rel.selector, None);
    }

    /// Each operand binds on its own: its own variables, its own slots
    /// and its own clauses, with the leftmost one holding the others
    /// and naming the columns the caller gets.
    #[test]
    fn each_operand_of_a_composite_binds_on_its_own() {
        let q = bound(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN b.id AS id \
             UNION \
             MATCH (c:Place) RETURN c.id AS id",
        );
        assert!(!q.is_linear());
        assert_eq!(q.columns, ["id"]);
        assert_eq!(q.conjoined.len(), 1);
        let right = q.conjoined[0].query.as_ref();
        assert_eq!(right.columns, ["id"], "the operands meet column by column");
        assert!(right.is_linear());
        assert_eq!(var(&q, "b").name, "b");
        assert!(
            right.variables.iter().all(|v| v.name != "b"),
            "a variable the left bound is not a variable the right has"
        );
        assert!(
            right.variables.iter().any(|v| v.name == "c"),
            "the right bound what it wrote"
        );
        let mut operands = 0;
        q.walk(&mut |_| operands += 1);
        assert_eq!(operands, 2, "the query is both operands");
    }

    /// The parameters are positional and belong to the statement, not
    /// to an operand of it, so the two operands see one list in the
    /// order the reader wrote the names in.
    #[test]
    fn the_operands_share_one_parameter_list() {
        let q = bound(
            "MATCH (a:Person) WHERE a.id = $left RETURN a.id AS id \
             UNION \
             MATCH (b:Person) WHERE b.id = $right RETURN b.id AS id",
        );
        assert_eq!(q.params, ["left", "right"]);
        assert_eq!(
            q.conjoined[0].query.params, q.params,
            "the same list, so a slot number means the same thing on either side"
        );
    }

    /// A composite meets two result tables. A statement that writes
    /// has no result table to offer, and letting one stand in an
    /// operand would make how many times it ran depend on which side
    /// the optimizer chose to build over.
    #[test]
    fn a_write_may_not_stand_in_an_operand() {
        let e = bind_err(
            "INSERT (x:Person) RETURN x.id AS id \
             UNION \
             MATCH (b:Person) RETURN b.id AS id",
        );
        assert!(e.contains("a statement that writes"), "got: {e}");
        let e = bind_err(
            "MATCH (b:Person) RETURN b.id AS id \
             OTHERWISE \
             INSERT (x:Person) RETURN x.id AS id",
        );
        assert!(e.contains("a statement that writes"), "got: {e}");
    }

    /// The operands are met column by column, so they have to have the
    /// same columns, and the message says which pair did not match
    /// rather than only that something did not.
    #[test]
    fn the_operands_have_to_agree_on_their_columns() {
        let e = bind_err(
            "MATCH (a:Person) RETURN a.id AS id \
             UNION \
             MATCH (b:Person) RETURN b.id AS id, b.id AS again",
        );
        assert!(e.contains("1 and 2 columns"), "got: {e}");
        let e = bind_err(
            "MATCH (a:Person) RETURN a.id AS id \
             INTERSECT ALL \
             MATCH (b:Person) RETURN b.id AS other",
        );
        assert!(
            e.contains("'id' where the other calls it 'other'"),
            "got: {e}"
        );
    }
    /// A FILTER binds to the shape a standalone condition already had
    /// here: a required match with no pattern under it and the
    /// condition on it.
    #[test]
    fn a_filter_binds_as_a_match_with_no_pattern() {
        let q = bound("MATCH (a:Person) FILTER a.id = $x RETURN a.id AS id");
        let BoundClause::Match {
            kind,
            patterns,
            filter,
        } = &q.clauses[1]
        else {
            panic!("the FILTER is the second clause");
        };
        assert_eq!(*kind, MatchKind::Required);
        assert!(patterns.is_empty(), "no pattern was written");
        assert!(filter.is_some(), "the condition is on it");
    }

    /// A LET carries the scope through and adds to it, which is a
    /// projection of everything in hand plus the new names. The names
    /// already in hand keep the slots they were in, so carrying them is
    /// not a copy.
    #[test]
    fn a_let_projects_the_scope_and_the_new_names() {
        let q = bound("MATCH (a:Person) LET twice = a.id * 2 RETURN twice AS v");
        let BoundClause::Project { items, .. } = &q.clauses[1] else {
            panic!("the LET is the second clause");
        };
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["a", "twice"],
            "the matched variable is carried and the new name follows it"
        );
        let a = q
            .variables
            .iter()
            .position(|v| v.name == "a")
            .expect("the matched variable");
        assert_eq!(items[0].slot, Some(a), "carrying a variable keeps its slot");
    }

    /// Definitions that read nothing the same statement made are one
    /// projection, because a projection evaluates its items against one
    /// row and these all read that row.
    #[test]
    fn independent_definitions_are_one_projection() {
        let q = bound("MATCH (a:Person) LET x = a.id, y = a.id + 1 RETURN x AS v");
        assert_eq!(q.clauses.len(), 3, "the MATCH, the LET, the RETURN");
        let BoundClause::Project { items, .. } = &q.clauses[1] else {
            panic!("the LET is one projection");
        };
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["a", "x", "y"]
        );
    }

    /// One that reads a name the same statement made cannot stand
    /// beside it, so it starts a projection of its own behind the one
    /// that made what it reads.
    #[test]
    fn a_definition_reading_an_earlier_one_starts_a_stage() {
        let q = bound("MATCH (a:Person) LET x = a.id, y = x + 1 RETURN y AS v");
        assert_eq!(q.clauses.len(), 4, "the MATCH, two stages, the RETURN");
        let BoundClause::Project { items, .. } = &q.clauses[1] else {
            panic!("the first stage");
        };
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["a", "x"]
        );
        let BoundClause::Project { items, .. } = &q.clauses[2] else {
            panic!("the second stage");
        };
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["a", "x", "y"],
            "the second stage carries what the first one left"
        );
    }

    /// A LET names a variable, so a name already in scope is refused
    /// rather than quietly meaning the new one from there on, and an
    /// aggregate is refused with the clauses that do group rows named.
    #[test]
    fn a_let_refuses_a_taken_name_and_an_aggregate() {
        let e = bind_err("MATCH (a:Person) LET a = 1 RETURN a AS v");
        assert!(e.contains("'a' is already defined"), "got: {e}");
        let e = bind_err("MATCH (a:Person) LET n = count(*) RETURN n AS v");
        assert!(e.contains("cannot be an aggregate"), "got: {e}");
    }
}
