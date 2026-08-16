//! Abstract syntax for the zuQL core (docs/07 §1, docs/grammar.ebnf).
//!
//! The parser produces this shape verbatim; name resolution, typing,
//! and structural rules that need the catalog (which variables exist,
//! what a label means) belong to the binder, so the AST stays a plain
//! description of the text.

use zu_common::{LogicalType, Temporal};

/// One statement: a query that reads, or a catalog statement that
/// changes what the file declares.
///
/// The two are parsed by the same entry point and told apart by their
/// first word, because a caller with a string in its hand does not know
/// which it has. They share nothing after that: a catalog statement has
/// no binding table, so it never reaches the binder or the optimizer.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Query),
    Catalog(CatalogStmt),
}

/// A statement that changes the catalog (docs/07 §9, GC03).
#[derive(Debug, Clone, PartialEq)]
pub enum CatalogStmt {
    CreateGraphType {
        name: String,
        /// GC03. With it, a name already taken is a statement that did
        /// nothing rather than an error.
        if_not_exists: bool,
        /// `OR REPLACE`, which is the other answer to the same
        /// question: the name is taken, so take it over.
        or_replace: bool,
        source: GraphTypeSource,
    },
    DropGraphType {
        name: String,
        if_exists: bool,
    },
    /// GC01: a schema, which is the directory a graph and a graph type
    /// are named in.
    CreateSchema {
        path: String,
        /// GC02, the same modifier `CREATE GRAPH TYPE` takes.
        if_not_exists: bool,
    },
    DropSchema {
        path: String,
        if_exists: bool,
    },
    /// GC04: a graph, which is a name, the type it is of, and what it
    /// holds.
    CreateGraph {
        name: GraphName,
        /// GC05.
        if_not_exists: bool,
        or_replace: bool,
        of: GraphTypeRef,
        /// GG05: the graph whose contents the new one starts with,
        /// which may be the graph the statement is against.
        copy_of: Option<GraphRef>,
    },
    DropGraph {
        name: GraphName,
        if_exists: bool,
    },
}

/// A catalog object's name and the schema it is in. ISO names one by an
/// absolute directory path, so a name with no path in it is a name in
/// the schema the session is working in, which is the root schema until
/// a session may say otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphName {
    pub schema: Option<String>,
    pub name: String,
}

/// The type a graph is created with (GG01 to GG04).
#[derive(Debug, Clone, PartialEq)]
pub enum GraphTypeRef {
    /// GG01: `ANY GRAPH`, or nothing written at all.
    Any,
    /// `::` and the name of a graph type the catalog holds.
    Named(String),
    /// A type written where the graph is created: in braces (GG03) or
    /// as `LIKE` another graph (GG04).
    Source(GraphTypeSource),
}

/// Where a graph type's element types come from (GG03, GG04).
#[derive(Debug, Clone, PartialEq)]
pub enum GraphTypeSource {
    /// GG03: the element types written out where the type is created,
    /// which is a closed type (GG02).
    Elements(Vec<ElementTypeDef>),
    /// GG04: the closed type a graph's tables already describe, read
    /// off the catalog and not off the data.
    Like(String),
}

/// One element type as it was written, in ISO's pattern spelling:
/// `(:Person => :Employee {name :: STRING})` is a node type keyed on
/// `Person` that also carries `Employee`.
///
/// Names here are names, not dictionary ids: the label dictionary
/// belongs to the catalog and the parser has never seen one. The name
/// is optional because ISO's element types are anonymous unless GG20
/// gives them one.
#[derive(Debug, Clone, PartialEq)]
pub struct ElementTypeDef {
    /// `NODE TYPE PersonType (...)` (GG20), or the alias written inside
    /// the pattern, which is the name an endpoint refers to it by.
    pub name: Option<String>,
    pub kind: ElementDefKind,
    /// GG21: the labels written before `=>`, empty when none were, in
    /// which case the key label set is inferred from the whole label
    /// set (GG22).
    pub key_labels: Vec<String>,
    /// The labels after `=>`, or all of them when there is no `=>`.
    /// The element carries the key labels and these together.
    pub labels: Vec<String>,
    pub properties: Vec<PropertyDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ElementDefKind {
    Node,
    Edge {
        from: Endpoint,
        to: Endpoint,
        /// GH02: `(a)-[:T]-(b)`, an edge type with no direction.
        undirected: bool,
    },
}

/// What an edge type's end refers to: a node type by the name it was
/// given, or one written out where the edge type is.
#[derive(Debug, Clone, PartialEq)]
pub enum Endpoint {
    Named(String),
    Inline(Box<ElementTypeDef>),
}

/// One property declaration. `optional` is where the type's
/// nullability went: `name :: STRING` may be left out and `name ::
/// STRING NOT NULL` may not, which is one rule about null rather than
/// two.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyDef {
    pub name: String,
    pub ty: LogicalType,
    pub optional: bool,
}

/// A whole query: clauses in source order, ending in `RETURN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// GQ01: the graph the clauses below run against, written as a
    /// `USE` clause in front of them. `None` is a query with no `USE`,
    /// which runs against whatever graph the session is working in.
    pub use_graph: Option<GraphRef>,
    pub clauses: Vec<Clause>,
}

/// What a `USE` clause names (ISO 16.2, `use graph clause`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphRef {
    /// `USE CURRENT_PROPERTY_GRAPH`, the graph the session is already
    /// working in, which is what the clause names when it names no
    /// name at all.
    Current,
    /// A graph in the catalog, by name and by the schema it is in.
    Named(GraphName),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match {
        optional: bool,
        patterns: Vec<PathPattern>,
        filter: Option<Expr>,
    },
    /// `INSERT (x:Person {name: 'Zoe'}), (y:Person)`, the statement
    /// that adds elements (ISO 13.2). The patterns are the same shape a
    /// MATCH writes, because GQL writes them the same way: what makes
    /// this one a write is the clause in front of them, and a pattern
    /// here is a description of an element to create rather than one to
    /// look for.
    Insert {
        patterns: Vec<PathPattern>,
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
    /// The label expression after the colon, `None` for a pattern that
    /// names no label and therefore matches whatever it reaches.
    pub label: Option<LabelExpr>,
    pub props: Vec<(String, Expr)>,
}

/// A label expression: names joined by `&` and `|`, negated with `!`,
/// `%` for any label at all, and parentheses to group them. A second
/// colon is the conjunction Cypher writes it as, so `(n:A:B)` and
/// `(n:A&B)` are the same pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelExpr {
    Label(String),
    /// `%`, which every element satisfies: an element has a label.
    Wildcard,
    Not(Box<LabelExpr>),
    And(Box<LabelExpr>, Box<LabelExpr>),
    Or(Box<LabelExpr>, Box<LabelExpr>),
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

/// Which edges a pattern walks, and which way round.
///
/// ISO writes seven edge patterns and they split along two axes: which
/// of the stored lists a step reads, and whether the edge itself has a
/// direction (GH02). `-[]->` is a directed edge followed the way it
/// points, `~[]~` is an undirected edge and says nothing about the way
/// round, and `-[]-` is every edge either way, which is what a query
/// that does not care writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDirection {
    /// `-[]->`, a directed edge followed forwards.
    Out,
    /// `<-[]-`, a directed edge followed backwards.
    In,
    /// `<-[]->`, a directed edge either way round.
    AnyDirected,
    /// `~[]~`, an undirected edge.
    Undirected,
    /// `<~[]~`, an undirected edge or a directed one followed
    /// backwards.
    InOrUndirected,
    /// `~[]~>`, an undirected edge or a directed one followed forwards.
    OutOrUndirected,
    /// `-[]-`, any edge at all, either way round.
    Any,
}

impl RelDirection {
    /// Whether a step reads the forward stored list, which holds the
    /// edges written from the near end.
    pub fn walks_out(self) -> bool {
        !matches!(self, RelDirection::In)
    }

    /// Whether a step reads the backward stored list. An undirected
    /// edge is stored once, so reading it from the far end is what
    /// answers the other way round.
    pub fn walks_in(self) -> bool {
        !matches!(self, RelDirection::Out)
    }

    /// Whether this pattern walks a rel table whose edges are directed
    /// (or undirected, for `undirected`). The two are asked separately
    /// because four of the seven spellings admit both.
    pub fn admits_directed(self) -> bool {
        !matches!(self, RelDirection::Undirected)
    }

    pub fn admits_undirected(self) -> bool {
        matches!(
            self,
            RelDirection::Undirected
                | RelDirection::InOrUndirected
                | RelDirection::OutOrUndirected
                | RelDirection::Any
        )
    }

    /// Whether a table of this kind is one this pattern may walk.
    pub fn admits(self, undirected: bool) -> bool {
        if undirected {
            self.admits_undirected()
        } else {
            self.admits_directed()
        }
    }

    /// Whether the pattern reads both stored lists, which is what makes
    /// a step emit a row per direction rather than one.
    pub fn both_ways(self) -> bool {
        self.walks_out() && self.walks_in()
    }

    /// This pattern against one rel table, as one of the three
    /// directions storage knows: forward list, backward list, or both.
    /// `None` says the table is not one this pattern may walk at all.
    ///
    /// An undirected edge is stored once, from whichever end it was
    /// written, so every pattern that admits it reads both lists.
    pub fn resolve(self, undirected: bool) -> Option<RelDirection> {
        if !self.admits(undirected) {
            return None;
        }
        if undirected {
            return Some(RelDirection::Any);
        }
        Some(match self {
            RelDirection::Out | RelDirection::OutOrUndirected => RelDirection::Out,
            RelDirection::In | RelDirection::InOrUndirected => RelDirection::In,
            _ => RelDirection::Any,
        })
    }

    /// The same pattern read from the other end, which is what a plan
    /// writes when it walks a step backwards.
    pub fn flip(self) -> RelDirection {
        match self {
            RelDirection::Out => RelDirection::In,
            RelDirection::In => RelDirection::Out,
            RelDirection::InOrUndirected => RelDirection::OutOrUndirected,
            RelDirection::OutOrUndirected => RelDirection::InOrUndirected,
            other => other,
        }
    }

    /// How the pattern is written, for a plan line or an error.
    pub fn spelling(self) -> (&'static str, &'static str) {
        match self {
            RelDirection::Out => ("-", "->"),
            RelDirection::In => ("<-", "-"),
            RelDirection::AnyDirected => ("<-", "->"),
            RelDirection::Undirected => ("~", "~"),
            RelDirection::InOrUndirected => ("<~", "~"),
            RelDirection::OutOrUndirected => ("~", "~>"),
            RelDirection::Any => ("-", "-"),
        }
    }
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
