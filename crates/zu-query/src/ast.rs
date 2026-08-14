//! Abstract syntax for the zuQL core (docs/07 §1, docs/grammar.ebnf).
//!
//! The parser produces this shape verbatim; name resolution, typing,
//! and structural rules that need the catalog (which variables exist,
//! what a label means) belong to the binder, so the AST stays a plain
//! description of the text.

use zu_common::{LogicalType, Temporal};

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
    /// `CALL name(args) YIELD col [AS alias], ...`, a table function
    /// producing rows (docs/07 §4).
    Call {
        name: String,
        args: Vec<Expr>,
        /// `(column, alias)` pairs; the column names are fixed by the
        /// function, the alias is what later clauses see.
        yields: Vec<(String, Option<String>)>,
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
    /// The `ORDER BY` keys, in the order they were written.
    pub order_by: Vec<SortKey<Expr>>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// Where the null value sorts in one `ORDER BY` key, ISO subclause
/// 16.17 `<null ordering>`.
///
/// The standard leaves the implicit ordering to the implementation and
/// zu's answer is last, in both directions, which is impdef IS001. A
/// key that says `NULLS FIRST` or `NULLS LAST` says it outright and the
/// direction does not enter into it either way: `NULLS FIRST` means the
/// head of the result, not the small end of the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NullOrder {
    /// What a key that says neither one gets.
    #[default]
    Last,
    First,
}

/// One `ORDER BY` key: what to sort on, which way round, and where the
/// nulls go.
///
/// `E` is whatever stands for the key at that point in the pipeline, so
/// the parser's `Expr`, the binder's `BoundExpr` and the compiler's
/// output column index all read the same three fields under one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey<E> {
    pub expr: E,
    pub ascending: bool,
    pub nulls: NullOrder,
}

impl<E> SortKey<E> {
    /// The key with a different expression and the same direction and
    /// null ordering, which is what every rewrite of a sort key wants.
    pub fn with_expr<T>(&self, expr: T) -> SortKey<T> {
        SortKey {
            expr,
            ascending: self.ascending,
            nulls: self.nulls,
        }
    }

    /// True when a null in this key sorts ahead of every value.
    pub fn nulls_first(&self) -> bool {
        matches!(self.nulls, NullOrder::First)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

/// GQL path mode: which repeats a variable-length path may contain.
/// The default is `TRAIL`, GQL's `DIFFERENT EDGES` match mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathMode {
    /// Edges and nodes may repeat; needs an upper bound or a selector.
    Walk,
    /// No repeated edge.
    #[default]
    Trail,
    /// No repeated node.
    Acyclic,
}

/// GQL path selector: restricts a variable-length match to minimum-hop
/// paths per endpoint pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    /// One shortest path per reached endpoint.
    AnyShortest,
    /// Every shortest path per reached endpoint.
    AllShortest,
}

/// One linear path: a node, then rel-node steps left to right.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// `p = (a)-[]->(b)` binds the path itself.
    pub var: Option<String>,
    /// `ANY SHORTEST` / `ALL SHORTEST` before the first node.
    pub selector: Option<Selector>,
    /// `WALK` / `TRAIL` / `ACYCLIC` before the first node.
    pub mode: PathMode,
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
    /// `expr IS TYPED type`, ISO's GA06. The answer is a boolean and
    /// never a null, including when the value is one, because asking
    /// whether a null is of a nullable type has an answer.
    IsTyped {
        expr: Box<Expr>,
        ty: LogicalType,
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
    /// `PATH [a, e, b]`, ISO's GE06: a path built out of the elements
    /// the query names rather than out of a pattern it matched.
    Path(Vec<Expr>),
    /// `CAST(expr AS type)`, ISO's GA05.
    Cast {
        expr: Box<Expr>,
        ty: LogicalType,
    },
    /// `EXISTS { MATCH (a)-[:knows]->(b) WHERE b.id > 10 }`, the pattern
    /// existence predicate. The block is a match of its own that binds
    /// nothing outside itself: variables written in it live for the
    /// length of the block, variables already in scope tie it to the
    /// row being tested, and the answer is a boolean.
    Exists {
        patterns: Vec<PathPattern>,
        filter: Option<Box<Expr>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// A temporal value written with its type, as in `DATE '2024-01-15'`.
    /// The text is read at parse time, so the plan carries the instant
    /// and never the spelling.
    Temporal(Temporal),
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
