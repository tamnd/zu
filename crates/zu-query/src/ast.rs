//! Abstract syntax for the zuQL core (docs/07 §1, docs/grammar.ebnf).
//!
//! The parser produces this shape verbatim; name resolution, typing,
//! and structural rules that need the catalog (which variables exist,
//! what a label means) belong to the binder, so the AST stays a plain
//! description of the text.

/// A whole query: clauses in source order, ending in `RETURN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub clauses: Vec<Clause>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match {
        optional: bool,
        patterns: Vec<PathPattern>,
        filter: Option<Expr>,
    },
    Unwind {
        expr: Expr,
        alias: String,
    },
    With {
        projection: Projection,
        filter: Option<Expr>,
    },
    Return {
        projection: Projection,
    },
}

/// The shared shape of `WITH` and `RETURN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub distinct: bool,
    /// `RETURN *`; may still carry explicit items after a comma.
    pub star: bool,
    pub items: Vec<ProjectionItem>,
    /// `(expr, ascending)` pairs in `ORDER BY` order.
    pub order_by: Vec<(Expr, bool)>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

/// One linear path: a node, then rel-node steps left to right.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// `p = (a)-[]->(b)` binds the path itself.
    pub var: Option<String>,
    pub start: NodePattern,
    pub steps: Vec<(RelPattern, NodePattern)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub var: Option<String>,
    pub labels: Vec<String>,
    pub props: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub var: Option<String>,
    /// `:KNOWS|LIKES` alternatives.
    pub types: Vec<String>,
    pub direction: RelDirection,
    /// `*`, `*2`, `*1..3`, `*..3`, `*2..` hop ranges; `None` is a
    /// single hop, `Some((None, None))` is a bare `*`.
    pub range: Option<(Option<u64>, Option<u64>)>,
    pub props: Vec<(String, Expr)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDirection {
    /// `-[]->`
    Out,
    /// `<-[]-`
    In,
    /// `-[]-`
    Undirected,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Param(String),
    Variable(String),
    Property {
        base: Box<Expr>,
        key: String,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `count(*)` is `star` with empty `args`.
    Call {
        name: String,
        distinct: bool,
        star: bool,
        args: Vec<Expr>,
    },
    List(Vec<Expr>),
    Map(Vec<(String, Expr)>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    Xor,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    In,
    StartsWith,
    EndsWith,
    Contains,
}
