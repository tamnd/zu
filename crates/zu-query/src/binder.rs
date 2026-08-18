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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use zu_common::gqlstatus::codes;
use zu_common::{LogicalType, Result, ZuError};

use crate::ast::{
    self, BinaryOp, Clause, Conjunction, DeleteTarget, Expr, LabelExpr, Literal, NodePattern,
    PathMode, Projection, RelDirection, RelPattern, RemoveItem, Removed, Selector, SetInto,
    SetItem, SortKey, UnaryOp,
};

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
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
    /// query expression reads nothing from the query around it: the
    /// two share their parameters and nothing else.
    pub scalars: Vec<BoundQuery>,
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
    /// GQ18. A value query expression, as an index into
    /// [`BoundQuery::scalars`].
    ///
    /// The query it names reads nothing from the row this expression
    /// stands in, so its answer is the same value for every row of the
    /// run and the executor works it out once, before the plan above
    /// it starts. That is the whole of the decorrelation: what is left
    /// here is a constant the run is handed, which is why the
    /// optimizer treats one the way it treats a parameter.
    Scalar(usize),
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

/// Whether a bound clause changes the graph.
fn writes(clause: &BoundClause) -> bool {
    matches!(
        clause,
        BoundClause::Insert { .. } | BoundClause::Set { .. } | BoundClause::Delete { .. }
    )
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

/// Every slot a bound expression reads.
///
/// The set is whatever the caller keeps them in, since the executor
/// wants them in slot order and the optimizer only asks whether one is
/// in there. It lives here because it is a fact about `BoundExpr` and
/// three places need it; two of them used to hold a copy each, which is
/// two places to forget when a variant is added.
pub(crate) fn expr_slots(expr: &BoundExpr, out: &mut impl Extend<usize>) {
    match expr {
        // A value query expression reads no slot of this query: that
        // is what makes it one value for the whole run.
        BoundExpr::Literal(_) | BoundExpr::Param(_) | BoundExpr::Scalar(_) => {}
        BoundExpr::Var(slot) | BoundExpr::HasLabels { slot, .. } => out.extend([*slot]),
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
            for (_, value) in pairs {
                expr_slots(value, out);
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
    /// of these is a correlated value query expression, which is a
    /// thing this engine says it cannot do rather than a name it
    /// pretends never to have heard of.
    outer: Vec<String>,
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
                // A mark is a match of its own that has to run before
                // the predicate reading it, and an OPTIONAL MATCH is
                // one bracket already: its WHERE decides the match
                // rather than filtering behind it, so a mark there
                // would have to run inside that bracket and refuses
                // instead.
                let filter = self.bind_where(filter, !optional)?;
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
                    if path.selector.is_some() || path.mode != PathMode::default() {
                        return Err(invalid(
                            "a selector or a path mode says which of the walks that are there to pick, and INSERT is making one rather than picking one".into(),
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
        let filter = match &block.filter {
            Some(expr) => Some(self.bind_bool(expr, "WHERE")?),
            None => None,
        };
        self.marks = held;
        self.scope = outer;
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
        Ok(BoundClause::Project {
            distinct: false,
            items: bound,
            order_by: Vec::new(),
            skip: None,
            limit: None,
            filter: None,
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
        if pat.range.is_some() {
            return Err(invalid(
                "a hop range asks for a walk of some length, and INSERT writes one edge".into(),
            ));
        }
        let [name] = pat.types.as_slice() else {
            return Err(match pat.types.is_empty() {
                true => invalid(
                    "INSERT needs an edge type saying which table the edge goes in, and this one names none".into(),
                ),
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
        let slot = match &pat.var {
            Some(name) => self.declare(name, Type::Rel)?,
            None => self.anon_slot(Type::Rel),
        };
        self.variables[slot].rel_tables = vec![rel.id];
        let props = self.bind_props(&pat.props)?;
        Ok(BoundInsertRel {
            slot,
            table: rel.id,
            src,
            dst,
            props,
        })
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
        let Some(label) = &pat.label else {
            return Err(invalid(format!(
                "INSERT needs a label saying which table the element goes in, and '({})' names none",
                pat.var.as_deref().unwrap_or("")
            )));
        };
        let LabelExpr::Label(name) = label else {
            return Err(not_yet(
                "INSERT of an element whose labels are written as anything but one name,",
            ));
        };
        // A row lands in a table, and the label a table gives every row
        // it holds is its own name, so that is the one that says where
        // an element goes. A secondary label is something a row carries
        // rather than somewhere it lives, and adding one to an element
        // being created is a key label set change, which is its own
        // line on the milestone.
        let table = self
            .schema
            .nodes
            .iter()
            .find(|n| n.name == *name)
            .ok_or_else(|| {
                bad_reference(format!(
                    "no node table is named '{name}', and an element is created in the table whose own name is the label"
                ))
            })?
            .id;
        let props = self.bind_props(&pat.props)?;
        let slot = match &pat.var {
            Some(name) => self.declare(name, Type::Node)?,
            None => self.anon_slot(Type::Node),
        };
        self.variables[slot].node_tables = vec![table];
        Ok(BoundInsertNode { slot, table, props })
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
                let slot = self
                    .scope
                    .get(name)
                    .copied()
                    .ok_or_else(|| self.undefined(name))?;
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
            // wants a value out of it, which is the mark. Where a mark
            // cannot go either, `bind_mark` is what says so.
            Expr::Exists { patterns, filter } => {
                Ok((self.bind_mark(patterns, filter, false)?, Type::Bool))
            }
            Expr::ValueQuery(query) => self.bind_value_query(query),
        }
    }

    /// A name that is not in scope, and what to say about it.
    ///
    /// Inside a value query expression the name may well be defined:
    /// it is defined in the query around this one, and this engine
    /// works one of these out once for the whole run, which it can
    /// only do while the answer is the same for every row. Saying so
    /// beats saying the name does not exist, because it does.
    fn undefined(&self, name: &str) -> ZuError {
        if self.outer.iter().any(|n| n == name) {
            return invalid(format!(
                "a VALUE query is worked out once for the whole statement, so it cannot read '{name}' from the query around it: lift the query out and join it, or write the value in"
            ));
        }
        bad_reference(format!("variable '{name}' is not defined"))
    }

    /// GQ18: `VALUE { ... }`, a whole query standing where one value
    /// belongs (ISO 20.6).
    ///
    /// It binds as a query of its own with a scope of its own, because
    /// it shares nothing with the query around it but the parameters.
    /// What comes back here is the index the executor reads its answer
    /// at, and the answer is worked out once, before the plan above it
    /// runs.
    fn bind_value_query(&mut self, query: &ast::Query) -> Result<(BoundExpr, Type)> {
        if query.use_graph.is_some() {
            return Err(invalid(
                "a VALUE query runs in the graph the statement runs in, so it may not carry a USE of its own".into(),
            ));
        }
        let mut params = std::mem::take(&mut self.params);
        let mut outer = self.outer.clone();
        outer.extend(self.scope.keys().cloned());
        let bound = bind_body(&query.body, self.schema, &mut params, &outer)?;
        self.params = params;
        let mut wrote = false;
        bound.walk(&mut |q| wrote |= q.clauses.iter().any(writes));
        if wrote {
            return Err(invalid(
                "a VALUE query stands for a value and is read where one belongs, so it may not write to the graph".into(),
            ));
        }
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
        self.scalars.push(bound);
        Ok((BoundExpr::Scalar(self.scalars.len() - 1), ty))
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
        Expr::ValueQuery(_) => "VALUE { ... }".into(),
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
