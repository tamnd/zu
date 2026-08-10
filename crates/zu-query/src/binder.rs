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

use std::collections::HashMap;
use std::fmt;

use zu_common::{Result, ZuError};

use crate::ast::{
    self, BinaryOp, Clause, Expr, Literal, NodePattern, PathMode, Projection, RelDirection,
    RelPattern, Selector, UnaryOp,
};

fn invalid(detail: String) -> ZuError {
    ZuError::InvalidArgument(detail)
}

/// One node table: a label naming the row domain `0..node_count`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDef {
    pub id: u32,
    pub name: String,
    pub node_count: u64,
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

/// The table shape the binder resolves against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schema {
    nodes: Vec<NodeDef>,
    rels: Vec<RelDef>,
}

impl Schema {
    /// Builds a schema, rejecting duplicate names and rel endpoints
    /// that name no node table.
    pub fn new(nodes: Vec<NodeDef>, rels: Vec<RelDef>) -> Result<Self> {
        let schema = Schema { nodes, rels };
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
    Map,
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
            Type::Map => write!(f, "MAP"),
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
}

#[derive(Debug, Clone, PartialEq)]
pub enum BoundClause {
    Match {
        optional: bool,
        patterns: Vec<BoundPath>,
        filter: Option<BoundExpr>,
    },
    Unwind {
        expr: BoundExpr,
        slot: usize,
    },
    /// `WITH` and `RETURN` share one shape; `RETURN` is the final one.
    Project {
        distinct: bool,
        items: Vec<BoundItem>,
        order_by: Vec<(BoundExpr, bool)>,
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

#[derive(Debug, Clone, PartialEq)]
pub struct BoundNode {
    pub slot: usize,
    /// Inline `{key: expr}` equality predicates.
    pub props: Vec<(String, BoundExpr)>,
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
    Call {
        func: Func,
        distinct: bool,
        star: bool,
        args: Vec<BoundExpr>,
    },
    List(Vec<BoundExpr>),
    Map(Vec<(String, BoundExpr)>),
}

/// Binds a parsed query against a schema.
pub fn bind(query: &ast::Query, schema: &Schema) -> Result<BoundQuery> {
    let mut binder = Binder {
        schema,
        variables: Vec::new(),
        scope: HashMap::new(),
        params: Vec::new(),
        columns: Vec::new(),
    };
    let mut clauses = Vec::new();
    for clause in &query.clauses {
        clauses.push(binder.bind_clause(clause)?);
    }
    Ok(BoundQuery {
        clauses,
        variables: binder.variables,
        params: binder.params,
        columns: binder.columns,
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
            return Err(invalid(format!("variable '{name}' is already defined")));
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
                let filter = match filter {
                    Some(expr) => Some(self.bind_bool(expr, "WHERE")?),
                    None => None,
                };
                Ok(BoundClause::Match {
                    optional: *optional,
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
            Clause::With { projection, filter } => self.bind_projection(projection, false, filter),
            Clause::Return { projection } => self.bind_projection(projection, true, &None),
        }
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
                return Err(invalid(format!("duplicate name '{}' in WITH", item.name)));
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
        for (expr, ascending) in &projection.order_by {
            let mut ctx = ExprCtx::new(true);
            let (bound, _) = self.bind_expr(expr, &mut ctx)?;
            order_by.push((bound, *ascending));
        }
        let skip = self.bind_count_limit(&projection.skip, "SKIP")?;
        let limit = self.bind_count_limit(&projection.limit, "LIMIT")?;

        // The clause's ongoing scope is exactly the projected names.
        self.scope = new_scope;
        let filter = match filter {
            Some(expr) => Some(self.bind_bool(expr, "WHERE")?),
            None => None,
        };
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
            return Err(invalid(format!("{what} needs an integer, got {ty}")));
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
        Ok(BoundPath { slot, start, steps })
    }

    fn bind_node(&mut self, pat: &NodePattern) -> Result<BoundNode> {
        let candidates: Vec<u32> = match pat.labels.len() {
            0 => self.schema.nodes.iter().map(|n| n.id).collect(),
            1 => {
                let label = &pat.labels[0];
                let node = self
                    .schema
                    .node_by_name(label)
                    .ok_or_else(|| invalid(format!("unknown label '{label}'")))?;
                vec![node.id]
            }
            _ => {
                return Err(invalid(format!(
                    "node '{}' has {} labels, v0 nodes have exactly one table",
                    pat.var.as_deref().unwrap_or(""),
                    pat.labels.len()
                )));
            }
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
        Ok(BoundNode { slot, props })
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
                    .ok_or_else(|| invalid(format!("unknown relationship type '{ty}'")))?;
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
                    .ok_or_else(|| invalid(format!("variable '{name}' is not defined")))?;
                Ok((BoundExpr::Var(slot), self.variables[slot].ty.clone()))
            }
            Expr::Property { base, key } => {
                let (bound, ty) = self.bind_expr(base, ctx)?;
                if !matches!(ty, Type::Node | Type::Rel | Type::Map | Type::Any) {
                    return Err(invalid(format!(
                        "property access needs a node, rel, or map, got {ty} from {}",
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
                            return Err(invalid(format!("NOT needs a boolean, got {ty}")));
                        }
                        Type::Bool
                    }
                    UnaryOp::Neg => {
                        if !ty.is_numeric() {
                            return Err(invalid(format!("unary minus needs a number, got {ty}")));
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
            Expr::Map(entries) => {
                let mut bound = Vec::new();
                for (key, value) in entries {
                    let (b, _) = self.bind_expr(value, ctx)?;
                    bound.push((key.clone(), b));
                }
                Ok((BoundExpr::Map(bound), Type::Map))
            }
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
        let func =
            Func::resolve(name).ok_or_else(|| invalid(format!("unknown function '{name}'")))?;
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
                    return Err(invalid(format!("sum() needs a number, got {arg_ty}")));
                }
                arg_ty
            }
            Func::Avg => {
                if !arg_ty.is_numeric() {
                    return Err(invalid(format!("avg() needs a number, got {arg_ty}")));
                }
                Type::Float
            }
            Func::Min | Func::Max => arg_ty,
            Func::Collect => Type::List(Box::new(arg_ty)),
            Func::Id => {
                if !matches!(arg_ty, Type::Node | Type::Rel | Type::Any) {
                    return Err(invalid(format!("id() needs a node or rel, got {arg_ty}")));
                }
                Type::Int
            }
            Func::Size => {
                if !matches!(arg_ty, Type::List(_) | Type::Str | Type::Any) {
                    return Err(invalid(format!(
                        "size() needs a list or string, got {arg_ty}"
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
                return Err(invalid(format!(
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
                    return Err(invalid(format!(
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
                    return Err(invalid(format!("IN needs a list on the right, got {rhs}")));
                }
                Ok(Type::Bool)
            }
            BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => {
                if !lhs.is_str() || !rhs.is_str() {
                    return Err(invalid(format!(
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    /// person(0), place(1), person-KNOWS->person, person-IS_LOCATED_IN->place,
    /// place-PART_OF->place, mirroring the LDBC SF1 core.
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

    #[test]
    fn two_labels_are_rejected_in_v0() {
        assert!(bind_err("MATCH (n:Person:Place) RETURN n").contains("exactly one table"));
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
