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

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use zu_common::gqlstatus::codes;
use zu_common::{LogicalType, Result, ZuError};

use crate::ast::{
    self, BinaryOp, Clause, Expr, LabelExpr, Literal, NodePattern, PathMode, Projection,
    RelDirection, RelPattern, Selector, SortKey, UnaryOp,
};

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
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
/// Only the top level of the AND chain is taken. A block under an OR,
/// or anywhere a value is wanted, is left where it is and the binder
/// refuses it there: turning one into a boolean column is a mark join
/// and this is not one.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelDef {
    pub id: u32,
    pub name: String,
    pub from: u32,
    pub to: u32,
    pub edge_count: u64,
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
        }
    }
}

/// One bound variable. Pattern elements without a name in the query get
/// a slot too, named `#slot`, so the planner addresses everything the
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
}

/// What a match does with the rows underneath it.
///
/// The first two are written as MATCH and OPTIONAL MATCH. The other
/// two are what an existence predicate becomes: `EXISTS { ... }` is a
/// match whose rows are never returned and whose only use is that
/// there was one, and `NOT EXISTS { ... }` is the same match keeping
/// the rows it found nothing for.
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundClause {
    Match {
        kind: MatchKind,
        patterns: Vec<BoundPath>,
        filter: Option<BoundExpr>,
    },
    Unwind {
        expr: BoundExpr,
        slot: usize,
    },
    /// A table function call, always the first clause: the kernel runs
    /// once over `rel` and yields one row per node of its domain.
    Call {
        func: TableFunc,
        rel: u32,
        /// The rel's node table, the type of the `node` column.
        table: u32,
        /// Arguments after the rel name, the sssp source key.
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

#[derive(Debug, Clone, PartialEq)]
pub struct BoundPath {
    pub slot: Option<usize>,
    pub start: BoundNode,
    pub steps: Vec<(BoundRel, BoundNode)>,
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
}

/// Whole-graph table functions the binder accepts in `CALL` (docs/07
/// §4). Each fixes its argument shape and YIELD columns here; the
/// engine's kernels do the work at execution time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableFunc {
    Pagerank,
    Wcc,
    Sssp,
    Louvain,
}

impl TableFunc {
    fn resolve(name: &str) -> Option<TableFunc> {
        Some(match name.to_ascii_lowercase().as_str() {
            "pagerank" => TableFunc::Pagerank,
            "wcc" => TableFunc::Wcc,
            "sssp" => TableFunc::Sssp,
            "louvain" => TableFunc::Louvain,
            _ => return None,
        })
    }

    /// The engine-facing kernel name.
    pub fn name(self) -> &'static str {
        match self {
            TableFunc::Pagerank => "pagerank",
            TableFunc::Wcc => "wcc",
            TableFunc::Sssp => "sssp",
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
            TableFunc::Sssp => ("distance", Type::Int),
            TableFunc::Louvain => ("community", Type::Int),
        }
    }
}

/// Builtin functions the binder accepts in v0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Collect,
    Id,
    Size,
    /// GF12. The element count of a list, which is `SIZE` asked for by
    /// its other name and refused on anything that is not a list.
    Cardinality,
    /// GF04. The number of edges in a path, which is one less than the
    /// number of hops in the element list and not the element count:
    /// a two node path has three elements and a length of one.
    PathLength,
}

impl Func {
    fn resolve(name: &str) -> Option<Func> {
        let lower = name.to_ascii_lowercase();
        Some(match lower.as_str() {
            "count" => Func::Count,
            "sum" => Func::Sum,
            "avg" => Func::Avg,
            "min" => Func::Min,
            "max" => Func::Max,
            "collect" => Func::Collect,
            "id" => Func::Id,
            "size" => Func::Size,
            "cardinality" => Func::Cardinality,
            "path_length" => Func::PathLength,
            _ => return None,
        })
    }

    pub fn is_aggregate(&self) -> bool {
        matches!(
            self,
            Func::Count | Func::Sum | Func::Avg | Func::Min | Func::Max | Func::Collect
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundExpr {
    Literal(Literal),
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
    Call {
        func: Func,
        distinct: bool,
        star: bool,
        args: Vec<BoundExpr>,
    },
    List(Vec<BoundExpr>),
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
    /// Whether the node in `slot` satisfies a label expression, the
    /// runtime half of a label set. The binder only plants one where
    /// the candidate tables leave the answer open, so a pattern naming
    /// a table's own label compiles to no predicate at all.
    HasLabels {
        slot: usize,
        test: LabelTest,
    },
}

/// Binds a parsed query against a schema.
pub fn bind(query: &ast::Query, schema: &Schema) -> Result<BoundQuery> {
    let mut binder = Binder {
        schema,
        variables: Vec::new(),
        scope: HashMap::new(),
        params: Vec::new(),
        columns: Vec::new(),
        path_shapes: BTreeMap::new(),
        pending: Vec::new(),
    };
    let mut clauses = Vec::new();
    for (i, clause) in query.clauses.iter().enumerate() {
        if i > 0 && matches!(clause, Clause::Call { .. }) {
            return Err(invalid(
                "CALL must be the first clause, table functions run once over the whole graph"
                    .into(),
            ));
        }
        clauses.push(binder.bind_clause(clause)?);
        // An existence block written in the clause's WHERE is a match
        // of its own and runs where the predicate would have, which is
        // straight after the clause it was written in.
        clauses.append(&mut binder.pending);
    }
    Ok(BoundQuery {
        clauses,
        variables: binder.variables,
        params: binder.params,
        columns: binder.columns,
        path_shapes: binder.path_shapes,
        labels: schema.labels.clone(),
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

    fn bind_clause(&mut self, clause: &Clause) -> Result<BoundClause> {
        match clause {
            Clause::Match {
                optional,
                patterns,
                filter,
            } => {
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
                let filter = self.bind_where(filter)?;
                Ok(BoundClause::Match {
                    kind: match optional {
                        true => MatchKind::Optional,
                        false => MatchKind::Required,
                    },
                    patterns: bound,
                    filter,
                })
            }
            Clause::Unwind { expr, alias } => {
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
                Ok(BoundClause::Unwind { expr: bound, slot })
            }
            Clause::Call { name, args, yields } => self.bind_table_call(name, args, yields),
            Clause::With { projection, filter } => self.bind_projection(projection, false, filter),
            Clause::Return { projection } => self.bind_projection(projection, true, &None),
        }
    }

    /// Binds a WHERE, the existence blocks in it lifted into matches of
    /// their own first.
    ///
    /// A block is a match and not a value, so it cannot be bound where
    /// it was written: what comes back here is the predicate that is
    /// left, and the blocks are queued in `pending` to run after the
    /// clause. Only the top level of the AND chain is lifted, since a
    /// block under an OR is asking for a boolean and this is not the
    /// thing that produces one.
    fn bind_where(&mut self, filter: &Option<Expr>) -> Result<Option<BoundExpr>> {
        let Some(expr) = filter else {
            return Ok(None);
        };
        let mut blocks = Vec::new();
        let rest = peel_exists(expr, &mut blocks);
        for block in blocks {
            let clause = self.bind_exists(block)?;
            self.pending.push(clause);
        }
        match rest {
            Some(expr) => Ok(Some(self.bind_bool(&expr, "WHERE")?)),
            None => Ok(None),
        }
    }

    /// Binds one existence block into the match it stands for.
    ///
    /// The block sees the scope around it, which is what ties it to the
    /// row being tested, and the names it writes itself are gone again
    /// when it ends: an EXISTS says whether a match was there and hands
    /// back nothing to read, so a variable of its own that outlived it
    /// would name a row nobody kept.
    fn bind_exists(&mut self, block: ExistsBlock) -> Result<BoundClause> {
        let outer = self.scope.clone();
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
        let filter = match &block.filter {
            Some(expr) => Some(self.bind_bool(expr, "WHERE")?),
            None => None,
        };
        self.scope = outer;
        Ok(BoundClause::Match {
            kind: match block.negated {
                true => MatchKind::Anti,
                false => MatchKind::Semi,
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
                "unknown table function '{name}', the v0 functions are pagerank, wcc, sssp, louvain"
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
            TableFunc::Sssp => {
                if bound_args.len() != 1 {
                    return Err(invalid(
                        "sssp takes the rel table and a source node id".into(),
                    ));
                }
                if !matches!(bound_args[0].1, Type::Int | Type::Any) {
                    return Err(invalid(format!(
                        "sssp's source must be a node id, got {}",
                        bound_args[0].1
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
        is_return: bool,
        filter: &Option<Expr>,
    ) -> Result<BoundClause> {
        let clause = if is_return { "RETURN" } else { "WITH" };
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
                    if !is_return {
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
        let has_aggregate = items.iter().any(|i| i.aggregate);

        // ORDER BY and a WITH's WHERE see the projected names; without
        // aggregation the pre-projection variables stay visible too.
        let old_scope = self.scope.clone();
        let mut new_scope: HashMap<String, usize> = HashMap::new();
        for item in &mut items {
            if !is_return && new_scope.contains_key(&item.name) {
                return Err(bad_reference(format!(
                    "duplicate name '{}' in WITH",
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
        let filter = self.bind_where(filter)?;
        if is_return {
            self.columns = items.iter().map(|i| i.name.clone()).collect();
            for item in &mut items {
                item.slot = None;
            }
        }
        Ok(BoundClause::Project {
            distinct: projection.distinct,
            items,
            order_by,
            skip,
            limit,
            filter,
        })
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
        for (rel, node) in &path.steps {
            let rel = self.bind_rel(rel, path.mode, path.selector)?;
            let node = self.bind_node(node)?;
            steps.push((rel, node));
        }
        if path.selector.is_some() && steps.iter().all(|(rel, _)| rel.range.is_none()) {
            return Err(invalid(
                "a SHORTEST selector needs a variable-length relationship".into(),
            ));
        }
        if let Some(slot) = slot {
            let mut parts = vec![PathPart::Node(start.slot)];
            for (rel, node) in &steps {
                parts.push(if rel.range.is_some() {
                    PathPart::VarRel(rel.slot)
                } else {
                    PathPart::Rel(rel.slot)
                });
                parts.push(PathPart::Node(node.slot));
            }
            self.path_shapes.insert(slot, parts);
        }
        Ok(BoundPath { slot, start, steps })
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
        if candidates.is_empty() {
            let written = test
                .as_ref()
                .map_or(String::new(), |t| t.text(&self.schema.labels));
            return Err(invalid(format!(
                "no node table can satisfy the labels on '{}:{written}'",
                pat.var.as_deref().unwrap_or("")
            )));
        }
        let residue = match settled {
            true => None,
            false => test.map(|test| test.prune(guaranteed(self.schema, &candidates))),
        };
        let slot = match &pat.var {
            Some(name) => match self.scope.get(name).copied() {
                Some(slot) => {
                    if self.variables[slot].ty != Type::Node {
                        return Err(invalid(format!(
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
                    if merged.is_empty() {
                        return Err(invalid(format!(
                            "no node table satisfies every label on '{name}'"
                        )));
                    }
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
        let props = self.bind_props(&pat.props)?;
        Ok(BoundNode {
            slot,
            props,
            label: residue,
        })
    }

    /// Resolves the names in a label expression to dictionary bits.
    /// A conjunction of plain names collapses to one mask, which is the
    /// shape almost every pattern has and the one the runtime answers
    /// with a single AND.
    fn compile_label(&self, expr: &LabelExpr) -> Result<LabelTest> {
        Ok(match expr {
            LabelExpr::Label(name) => {
                let id = self
                    .schema
                    .label_id(name)
                    .ok_or_else(|| bad_reference(format!("unknown label '{name}'")))?;
                LabelTest::All(1 << id)
            }
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
            for ty in &pat.types {
                let rel = self
                    .schema
                    .rel_by_name(ty)
                    .ok_or_else(|| bad_reference(format!("unknown relationship type '{ty}'")))?;
                candidates.push(rel.id);
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
            if selector.is_some() && min.is_some_and(|m| m > 1) {
                return Err(invalid(
                    "a SHORTEST selector needs a lower bound of 1; a minimum-hop \
                     path cannot be forced longer"
                        .into(),
                ));
            }
            if mode == PathMode::Walk && max.is_none() && selector.is_none() {
                return Err(invalid(
                    "an unbounded WALK matches infinitely many paths; add an upper \
                     bound or a SHORTEST selector"
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
                    // binds exactly once.
                    return Err(invalid(format!(
                        "relationship variable '{name}' is already bound"
                    )));
                }
                self.declare(name, ty)?
            }
            None => self.anon_slot(ty),
        };
        self.variables[slot].rel_tables = candidates;
        let props = self.bind_props(&pat.props)?;
        Ok(BoundRel {
            slot,
            direction: pat.direction,
            range: pat.range,
            mode,
            selector,
            props,
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
        let fits = |r: &RelDef, from: &[u32], to: &[u32]| match rel.direction {
            RelDirection::Out => from.contains(&r.from) && to.contains(&r.to),
            RelDirection::In => from.contains(&r.to) && to.contains(&r.from),
            RelDirection::Undirected => {
                (from.contains(&r.from) && to.contains(&r.to))
                    || (from.contains(&r.to) && to.contains(&r.from))
            }
        };
        let left_tables = self.variables[left].node_tables.clone();
        let right_tables = self.variables[right].node_tables.clone();
        let rels: Vec<&RelDef> = self.variables[rel.slot]
            .rel_tables
            .iter()
            .filter_map(|id| self.schema.rel_by_id(*id))
            .filter(|r| fits(r, &left_tables, &right_tables))
            .collect();
        if rels.is_empty() {
            return Err(invalid(format!(
                "pattern step at '{}' matches no relationship table",
                self.variables[rel.slot].name
            )));
        }
        let reaches = |node: u32, end: fn(&RelDef, RelDirection) -> (u32, u32)| {
            rels.iter().any(|r| {
                let (a, b) = end(r, rel.direction);
                match rel.direction {
                    RelDirection::Undirected => node == a || node == b,
                    _ => node == a,
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
        for (slot, tables) in [(left, new_left), (right, new_right)] {
            if tables.is_empty() {
                return Err(invalid(format!(
                    "pattern step at '{}' leaves '{}' with no node table",
                    self.variables[rel.slot].name, self.variables[slot].name
                )));
            }
            self.variables[slot].node_tables = tables;
        }
        self.variables[rel.slot].rel_tables = rels.iter().map(|r| r.id).collect();
        Ok(())
    }

    // Expressions.

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
                let slot =
                    self.scope.get(name).copied().ok_or_else(|| {
                        bad_reference(format!("variable '{name}' is not defined"))
                    })?;
                Ok((BoundExpr::Var(slot), self.variables[slot].ty.clone()))
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
            // wants a value out of it.
            Expr::Exists { .. } => Err(invalid(
                "EXISTS is a match and not a value: write it as a whole conjunct of a WHERE, \
                 on its own or under NOT"
                    .into(),
            )),
        }
    }

    fn bind_call(
        &mut self,
        name: &str,
        distinct: bool,
        star: bool,
        args: &[Expr],
        ctx: &mut ExprCtx,
    ) -> Result<(BoundExpr, Type)> {
        let func = Func::resolve(name)
            .ok_or_else(|| bad_reference(format!("unknown function '{name}'")))?;
        if func.is_aggregate() {
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
        if star && func != Func::Count {
            return Err(invalid(format!("only count(*) takes *, not {name}(*)")));
        }
        let want = if func == Func::Count && star { 0 } else { 1 };
        if args.len() != want {
            return Err(invalid(format!(
                "{name}() takes {want} argument(s), got {}",
                args.len()
            )));
        }
        let was_in_aggregate = ctx.in_aggregate;
        if func.is_aggregate() {
            ctx.in_aggregate = true;
        }
        let mut bound = Vec::new();
        let mut arg_ty = Type::Any;
        for arg in args {
            let (b, t) = self.bind_expr(arg, ctx)?;
            arg_ty = t;
            bound.push(b);
        }
        ctx.in_aggregate = was_in_aggregate;
        let out = match func {
            Func::Count => Type::Int,
            Func::Sum => {
                if !arg_ty.is_numeric() {
                    return Err(bad_type(format!("sum() needs a number, got {arg_ty}")));
                }
                arg_ty
            }
            Func::Avg => {
                if !arg_ty.is_numeric() {
                    return Err(bad_type(format!("avg() needs a number, got {arg_ty}")));
                }
                Type::Float
            }
            Func::Min | Func::Max => arg_ty,
            Func::Collect => Type::List(Box::new(arg_ty)),
            Func::Id => {
                if !matches!(arg_ty, Type::Node | Type::Rel | Type::Any) {
                    return Err(bad_type(format!("id() needs a node or rel, got {arg_ty}")));
                }
                Type::Int
            }
            Func::Size => {
                // A path is its alternating node and rel list, so
                // size() applies to it like any other list.
                if !matches!(arg_ty, Type::List(_) | Type::Str | Type::Path | Type::Any) {
                    return Err(bad_type(format!(
                        "size() needs a list or string, got {arg_ty}"
                    )));
                }
                Type::Int
            }
            // ISO defines CARDINALITY over lists and groups and nothing
            // else, so unlike size() it refuses a string rather than
            // counting its characters, which is CHAR_LENGTH's question.
            Func::Cardinality => {
                if !matches!(arg_ty, Type::List(_) | Type::Any) {
                    return Err(bad_type(format!(
                        "cardinality() needs a list, got {arg_ty}"
                    )));
                }
                Type::Int
            }
            // A path and nothing else. A list of the same elements is
            // not a path, so counting one here would answer a question
            // about a value that is not the one asked about.
            Func::PathLength => {
                if !matches!(arg_ty, Type::Path | Type::Any) {
                    return Err(bad_type(format!(
                        "path_length() needs a path, got {arg_ty}"
                    )));
                }
                Type::Int
            }
        };
        Ok((
            BoundExpr::Call {
                func,
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
        }
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
        Expr::IsTyped { expr, ty, negated } => {
            let not = if *negated { "NOT " } else { "" };
            format!("{} IS {not}TYPED {ty}", text(expr))
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
        Expr::Cast { expr, ty } => format!("CAST({} AS {ty})", text(expr)),
        // The patterns are not rendered: this text names a column and
        // titles an operator, and a whole match inside one of those
        // reads worse than the word does.
        Expr::Exists { .. } => "EXISTS { ... }".into(),
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
                },
                RelDef {
                    id: 3,
                    name: "IS_LOCATED_IN".into(),
                    from: 0,
                    to: 1,
                    edge_count: 9000,
                },
                RelDef {
                    id: 4,
                    name: "PART_OF".into(),
                    from: 1,
                    to: 1,
                    edge_count: 1400,
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
        // Under an OR it is being asked for a boolean, which is a mark
        // join and not this.
        let e = bind_err(
            "MATCH (a:Person) WHERE a.id = 0 OR EXISTS { MATCH (a)-[:KNOWS]->(b) } RETURN a",
        );
        assert!(e.contains("EXISTS is a match and not a value"), "got: {e}");
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
    fn impossible_patterns_are_rejected() {
        let e = bind_err("MATCH (a:Place)-[:KNOWS]->(b) RETURN a");
        assert!(e.contains("matches no relationship table"), "got: {e}");
        assert!(bind_err("MATCH (n:Nope) RETURN n").contains("unknown label 'Nope'"));
        assert!(bind_err("MATCH (a)-[:NOPE]->(b) RETURN a").contains("unknown relationship type"));
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
        assert!(bind_err("MATCH (n:Nope) RETURN n").contains("unknown label 'Nope'"));
        let err = bind(&parse("MATCH (n:Person:Place) RETURN n").unwrap(), &schema)
            .expect_err("no table is both")
            .to_string();
        assert!(
            err.contains("no node table can satisfy the labels"),
            "{err}"
        );
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
        // Nothing satisfies a negated wildcard, and the message says
        // what was asked for.
        let err = bind(&parse("MATCH (n:!%) RETURN n").unwrap(), &schema)
            .expect_err("no table has no label")
            .to_string();
        assert!(
            err.contains("no node table can satisfy the labels on 'n:!%'"),
            "{err}"
        );
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

    #[test]
    fn path_mode_and_selector_rules() {
        // An unbounded WALK is infinite; a selector or a bound tames it.
        let e = bind_err("MATCH WALK (a:Person)-[:KNOWS*]->(b) RETURN b");
        assert!(e.contains("unbounded WALK"), "got: {e}");
        bound("MATCH WALK (a:Person)-[:KNOWS*1..3]->(b) RETURN b");
        bound("MATCH ANY SHORTEST WALK (a:Person)-[:KNOWS*]->(b) RETURN b");
        // A selector without a variable-length rel selects nothing.
        let e = bind_err("MATCH ANY SHORTEST (a:Person)-[:KNOWS]->(b) RETURN b");
        assert!(e.contains("variable-length"), "got: {e}");
        // Minimum-hop paths cannot be forced longer than one hop.
        let e = bind_err("MATCH ALL SHORTEST (a:Person)-[:KNOWS*2..3]->(b) RETURN b");
        assert!(e.contains("lower bound of 1"), "got: {e}");
        // The plain modes carry through to the bound rel.
        let q = bound("MATCH ACYCLIC (a:Person)-[:KNOWS*1..3]->(b) RETURN b");
        let BoundClause::Match { patterns, .. } = &q.clauses[0] else {
            panic!("MATCH");
        };
        let (rel, _) = &patterns[0].steps[0];
        assert_eq!(rel.mode, PathMode::Acyclic);
        assert_eq!(rel.selector, None);
    }
}
