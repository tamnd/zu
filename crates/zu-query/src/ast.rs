//! Abstract syntax for the zuQL core (docs/07 §1, docs/grammar.ebnf).
//!
//! The parser produces this shape verbatim; name resolution, typing,
//! and structural rules that need the catalog (which variables exist,
//! what a label means) belong to the binder, so the AST stays a plain
//! description of the text.

use zu_common::{LogicalType, Temporal};

/// One statement: a query that reads, a catalog statement that changes
/// what the file declares, or one of the three that say where a
/// transaction begins and ends.
///
/// They are parsed by the same entry point and told apart by their
/// first word, because a caller with a string in its hand does not know
/// which it has. They share nothing after that: a catalog statement has
/// no binding table, so it never reaches the binder or the optimizer,
/// and a transaction statement has no plan at all.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Query),
    Catalog(CatalogStmt),
    Transaction(TxnStmt),
}

/// Where a transaction begins and ends (docs/08 §1, GT01, GT02).
///
/// A statement written outside one runs in a transaction of its own,
/// so these three do not turn transactions on. What they do is say
/// that several statements are one transaction: what the first one
/// wrote is either kept by the `COMMIT` at the end or unmade by the
/// `ROLLBACK`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStmt {
    /// `START TRANSACTION`, with the access mode it was written with.
    /// Nothing written is `READ WRITE`, which is what GQL implies when
    /// the characteristics are left off.
    Start {
        read_only: bool,
    },
    Commit,
    Rollback,
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

/// A whole query, which GQL builds out of four levels (ISO 12.1
/// through 12.4) rather than one list of clauses.
///
/// The levels are what the operators need. A set operator joins two
/// whole queries and cannot be written as a clause in a list, because
/// there is no list either side of it belongs to; `NEXT` joins two
/// statements that each end in a result of their own; and a result
/// statement is the one thing a statement may end with and nothing may
/// follow. Writing all four out is what lets `UNION`, `OTHERWISE` and
/// `NEXT` mean here what they mean in the standard, instead of being
/// bolted onto a flat clause list that has no room for them.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// GQ01: the graph the statements below run against, written as a
    /// `USE` clause in front of them. `None` is a query with no `USE`,
    /// which runs against whatever graph the session is working in.
    pub use_graph: Option<GraphRef>,
    pub body: Composite,
}

impl Query {
    /// Every primitive statement in the query, in written order,
    /// whichever statement of a `NEXT` chain it stands in.
    ///
    /// This answers questions about the text as a whole: which tables
    /// does it name, does it write anything. A caller whose answer
    /// depends on where a statement stands walks the levels instead,
    /// because this deliberately forgets where the joins were.
    pub fn clauses(&self) -> Vec<&Clause> {
        let mut out = Vec::new();
        self.body.walk(&mut |linear| {
            for simple in &linear.statements {
                out.extend(simple.clauses.iter());
            }
        });
        out
    }

    /// The projection the query ends with, or `None` for a write that
    /// ends without one.
    ///
    /// For a composite this is the leftmost operand's, because that is
    /// the one whose column names the answer carries: the operands of a
    /// set operator have to agree on their columns, so the leftmost
    /// speaks for all of them.
    pub fn result(&self) -> Option<&Projection> {
        self.body.leftmost().statements.last()?.result.as_ref()
    }
}

/// A composite query statement (ISO 12.1): one linear query statement,
/// or several of them joined by operators over their result tables.
///
/// The joins are left associative and all at one level, which is what
/// the standard's `composite query expression` production says: there
/// is no precedence between `UNION` and `INTERSECT` here, and a query
/// that wants one writes the operands in the order it wants them read.
#[derive(Debug, Clone, PartialEq)]
pub enum Composite {
    Linear(Linear),
    /// Two operands and the conjunction between them. The left is a
    /// composite so that a third operand joins onto the pair rather
    /// than nesting to the right.
    Conjoined {
        left: Box<Composite>,
        how: Conjunction,
        right: Linear,
    },
}

impl Composite {
    /// The leftmost linear operand, which is the one the answer takes
    /// its column names from.
    pub fn leftmost(&self) -> &Linear {
        match self {
            Composite::Linear(linear) => linear,
            Composite::Conjoined { left, .. } => left.leftmost(),
        }
    }

    /// Calls `f` on every linear operand, left to right.
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Linear)) {
        match self {
            Composite::Linear(linear) => f(linear),
            Composite::Conjoined { left, right, .. } => {
                left.walk(f);
                f(right);
            }
        }
    }
}

/// What joins two operands of a composite query (ISO 12.1, `query
/// conjunction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conjunction {
    /// GQ03 to GQ07: a set operator over the two result tables.
    Set { op: SetOp, all: bool },
    /// GQ02: the right operand is evaluated only if the left answered
    /// no rows at all. This is not a set operator. It is a choice
    /// between two answers, and when the left has rows the right never
    /// runs.
    Otherwise,
}

/// Which set operator (ISO 12.1, `set operator`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// GQ03: everything either operand answered.
    Union,
    /// GQ04 and GQ05: what the left answered and the right did not.
    Except,
    /// GQ06 and GQ07: what both answered.
    Intersect,
}

impl SetOp {
    pub fn keyword(self) -> &'static str {
        match self {
            SetOp::Union => "UNION",
            SetOp::Except => "EXCEPT",
            SetOp::Intersect => "INTERSECT",
        }
    }
}

/// A linear query statement (ISO 12.2): simple statements chained by
/// `NEXT`, each one reading the binding table the one before it
/// answered.
///
/// One statement is the ordinary query. Two or more is a chain, and the
/// chain is not the same thing as writing the clauses one after the
/// other: each statement of it ends with a result of its own, and what
/// the next one sees is that result and nothing else the statement
/// before it had in hand.
#[derive(Debug, Clone, PartialEq)]
pub struct Linear {
    pub statements: Vec<Simple>,
}

/// A simple query statement (ISO 12.3): the primitive statements it is
/// written out of, and the result statement it ends with.
#[derive(Debug, Clone, PartialEq)]
pub struct Simple {
    pub clauses: Vec<Clause>,
    /// The primitive result statement (ISO 12.4), which is `RETURN` and
    /// what it projects. `None` is a statement that writes and ends
    /// without projecting, which is the ordinary way to write one: the
    /// answer to `INSERT (x:Person)` is that it worked.
    pub result: Option<Projection>,
}

/// What a `USE` clause names (ISO 16.2, `use graph clause`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphRef {
    /// `USE CURRENT_PROPERTY_GRAPH`, the graph the session is already
    /// working in, which is what the clause names when it names no
    /// name at all.
    Current,
    /// `USE HOME_PROPERTY_GRAPH`, the graph the session started in.
    /// It is the working graph until something moves the working
    /// graph, and it is what a statement names when it wants to go
    /// back rather than to stay.
    Home,
    /// A graph in the catalog, by name and by the schema it is in.
    Named(GraphName),
    /// `USE $g`, a graph the caller passed in (ISO 16.2, a reference
    /// parameter specification under `graph reference`).
    ///
    /// The name here is the parameter's, not the graph's: which graph
    /// this is is not known while the statement is being read, so the
    /// graph the statement runs against is settled when the parameter
    /// arrives. That is the whole of what makes a graph reference an
    /// expression rather than a word in the text.
    Param(String),
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
    Insert { patterns: Vec<PathPattern> },
    /// `SET p.age = 37`, the statement that changes what an element
    /// already there holds (ISO 13.3). The element is named by a
    /// variable an earlier clause bound, because a statement changes
    /// what it found rather than what it can describe.
    Set { items: Vec<SetItem> },
    /// `REMOVE p.age`, the statement that takes a property off an
    /// element (ISO 13.4). GQL defines it as setting the property to
    /// null, so what it does is a `SET` with nothing on the right of
    /// it; it is its own clause here because it is its own syntax, and
    /// because what a reader of an EXPLAIN listing wrote is what the
    /// listing should say back.
    Remove { items: Vec<RemoveItem> },
    /// `DELETE n`, the statement that takes an element out of the graph
    /// (ISO 13.5). Each item names one element, either as a variable an
    /// earlier clause bound or as a query that answers one.
    ///
    /// `DETACH` says the edges on the element go with it. Without it, an
    /// element that still has edges is an error rather than a way to
    /// leave one hanging, so either way what this clause deletes never
    /// leaves an edge behind.
    Delete {
        targets: Vec<DeleteTarget>,
        detach: bool,
    },
    /// `FOR x IN [1, 2, 3]`, the statement that makes a row out of
    /// every element of a list (ISO 14.8, GQ10), and `UNWIND [1, 2, 3]
    /// AS x`, which is the Cypher spelling of the same thing and the
    /// one this engine accepted first.
    ///
    /// `ordinal` is the counter GQL lets the statement number its own
    /// rows with, `WITH ORDINALITY i` from one (GQ11) and `WITH OFFSET
    /// i` from zero (GQ24). It counts the elements of the list rather
    /// than the rows the statement answers, so a `FOR` under a match
    /// starts again at each row that reaches it, which is what makes
    /// the number the position of the element and not a row id.
    Unwind {
        expr: Expr,
        alias: String,
        ordinal: Option<Ordinal>,
    },
    /// `FILTER p.age > 30`, the statement that keeps the rows a
    /// condition holds for (ISO 14.6, GQ08). It is the `WHERE` of a
    /// `MATCH` standing on its own, over whatever the statement has in
    /// hand rather than over a pattern the same clause wrote, which is
    /// how a reader writes a condition on the result of a `CALL` or of
    /// the statement in front of a `NEXT`. The `WHERE` in
    /// `FILTER WHERE p.age > 30` is optional and means nothing extra,
    /// which is the standard's own spelling and not a courtesy to
    /// Cypher.
    Filter { expr: Expr },
    /// `LET n = a.age + 1, big = n > 40`, the statement that names
    /// values (ISO 14.7, GQ09). Every variable in hand stays in hand
    /// and the new names are added to them, which is what makes it a
    /// different statement from `WITH`: a projection says what the rows
    /// are from there on, and this says what else they carry.
    ///
    /// The definitions read left to right, so a later one may use a
    /// name an earlier one in the same statement gave.
    Let { items: Vec<LetItem> },
    /// `MATCH (a)-[:KNOWS]->(b) YIELD b, a AS friend`, the graph
    /// pattern yield clause (ISO 16.14, GQ19). It says which of the
    /// variables a match wrote leave it, so it takes names away where a
    /// `LET` adds them, and it renames them where a `WITH` would have
    /// to write the whole projection out to do the same.
    ///
    /// It does not group and it does not drop a row, so the rows a
    /// match answered are the rows a yield answers, narrower.
    Yield { items: Vec<YieldItem> },
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
}

/// One item of a `DELETE`, which ISO writes as a value expression and
/// splits into two optional features: a simple expression naming an
/// element (GD04) and a subquery answering one (GD03).
#[derive(Debug, Clone, PartialEq)]
pub enum DeleteTarget {
    /// `DELETE n`: a variable an earlier clause bound.
    Variable(String),
    /// `DELETE VALUE { MATCH (p:Person) WHERE p.name = 'Ada' RETURN p }`,
    /// the value query expression of ISO 20.9. The query inside runs on
    /// its own and has to answer one row of one column, because the
    /// item is one element and a query answering two of them has not
    /// said which.
    Value(Box<Query>),
}

/// What one item of a `SET` writes, which is the one thing that differs
/// between the three forms the statement has.
#[derive(Debug, Clone, PartialEq)]
pub enum SetInto {
    /// `SET p.age = 37`: one property, named.
    Property(String),
    /// `SET p = {age: 37}`: every property the element has, out of the
    /// record on the right. A property the record leaves out is emptied
    /// rather than left alone, which is why this form names no key.
    Record,
    /// `SET p:Admin&Bot`: labels the element takes on. A label is not a
    /// property, so there is nothing on the right of it; the labels
    /// written are what it says.
    Labels(Vec<String>),
}

/// One assignment under `SET`: the element it changes, what it writes,
/// and what that takes.
///
/// The value is an expression like any other, so it is evaluated once
/// for every row the clauses before the `SET` answered and can read
/// the element it is about to change. An item that writes labels
/// carries a null there, because what it writes is in the statement
/// rather than in a value.
#[derive(Debug, Clone, PartialEq)]
pub struct SetItem {
    /// The variable standing for the element, which an earlier clause
    /// bound.
    pub target: String,
    pub into: SetInto,
    pub value: Expr,
}

/// What one item of a `REMOVE` takes off an element.
#[derive(Debug, Clone, PartialEq)]
pub enum Removed {
    /// `REMOVE p.age`: one property, which GQL defines as setting it to
    /// null.
    Property(String),
    /// `REMOVE p:Admin`: labels the element stops carrying.
    Labels(Vec<String>),
}

/// One item under `REMOVE`: the element it comes off and what comes
/// off it. There is no value, which is the whole difference between
/// this and [`SetItem`].
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveItem {
    pub target: String,
    pub what: Removed,
}

/// The shared shape of `WITH` and `RETURN`.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub distinct: bool,
    /// `RETURN *`; may still carry explicit items after a comma.
    pub star: bool,
    pub items: Vec<ProjectionItem>,
    /// The `GROUP BY` keys, written after the items (ISO 16.15, GQ15)
    /// and empty when the clause was not written at all. Without it a
    /// projection holding an aggregate groups by everything else it
    /// projects, which is the Cypher rule and stays; with it the
    /// grouping is what the reader said it is.
    pub group_by: Vec<Expr>,
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

/// One name a `LET` gives a value. The name comes first and the value
/// second, the opposite way round from a projection item, because a
/// `LET` is a definition rather than a column: the reader is naming
/// something, not saying what a table looks like.
#[derive(Debug, Clone, PartialEq)]
pub struct LetItem {
    pub name: String,
    pub expr: Expr,
}

/// One variable a `YIELD` lets out of a match, and the name it wears
/// after it. The name is a variable the match wrote rather than an
/// expression, because a yield says what leaves the match and not what
/// to compute out of it.
#[derive(Debug, Clone, PartialEq)]
pub struct YieldItem {
    pub name: String,
    pub alias: Option<String>,
}

/// The counter a `FOR` numbers its rows with: the name it binds and
/// the number the first element of the list takes. `WITH ORDINALITY`
/// counts from one, the way the standard counts the members of a list
/// everywhere else, and `WITH OFFSET` counts from zero, which is what
/// a reader wants when the number is going to index something.
#[derive(Debug, Clone, PartialEq)]
pub struct Ordinal {
    pub name: String,
    pub start: i64,
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
    /// No repeated node, except that the path may end where it began.
    ///
    /// That exception is the whole of the difference from `ACYCLIC`, and
    /// it is why one is not a substitute for the other: a cycle is a
    /// simple path and is not an acyclic one.
    Simple,
    /// No repeated node.
    Acyclic,
}

/// GQL path selector (ISO 16.6): how many of the paths a pattern
/// matches are kept, per pair of endpoints.
///
/// `ALL PATHS` keeps every one of them, which is what a pattern with no
/// selector does, so it has no variant here: the parser reads the words
/// and lands on `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    /// `ANY k PATHS`, and `ANY` on its own, which is `ANY 1`: up to k
    /// paths per endpoint, whichever ones the search comes to first.
    /// The standard leaves which ones to the implementation, and says
    /// so.
    Any(u64),
    /// `ANY SHORTEST PATH`: one path of the least length per endpoint.
    AnyShortest,
    /// `ALL SHORTEST PATHS`: every path of the least length per
    /// endpoint.
    AllShortest,
    /// `SHORTEST k PATHS`: the k of least length per endpoint, which is
    /// k paths and not k lengths, so it may take some of the paths of
    /// one length and leave the rest.
    Shortest(u64),
    /// `SHORTEST k PATH GROUPS`: every path whose length is one of the
    /// k least per endpoint, which is k lengths and however many paths
    /// have them.
    ShortestGroup(u64),
}

impl Selector {
    /// Whether this one keeps paths by their length, which is every
    /// selector but `ANY`.
    ///
    /// It is the question two rules ask. A length-bounded pattern like
    /// `{3,5}` means one thing to a selector that reads lengths and
    /// another to one that does not, and an unbounded `WALK` is finite
    /// under some of these and not under others.
    pub fn by_length(self) -> bool {
        !matches!(self, Selector::Any(_))
    }

    /// Whether a pattern under this selector matches finitely many
    /// paths even when the mode repeats nodes and edges and no upper
    /// bound is written.
    ///
    /// Only the two that keep the least length alone do. A path of
    /// least length repeats nothing, so there are finitely many however
    /// the mode is written, while the second-least length under `WALK`
    /// is the least length plus a lap of some cycle, and there is no
    /// end of those.
    pub fn bounds_a_walk(self) -> bool {
        matches!(self, Selector::AnyShortest | Selector::AllShortest)
    }
}

/// One linear path: a node, then rel-node steps left to right.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    /// `p = (a)-[]->(b)` binds the path itself.
    pub var: Option<String>,
    /// The path selector before the first node, `None` for a pattern
    /// that keeps every path it matches.
    pub selector: Option<Selector>,
    /// `WALK` / `TRAIL` / `SIMPLE` / `ACYCLIC` before the first node,
    /// `None` for a pattern that names no mode and therefore walks under
    /// the default.
    ///
    /// The default is what `PathMode::default` says, and a pattern that
    /// wrote it and one that wrote nothing walk the same way, so the two
    /// are only told apart in one place: a `KEEP` fills in what the
    /// patterns left out, and it has to know what they left out.
    pub mode: Option<PathMode>,
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
    /// A `WHERE` written inside the brackets, which every edge the
    /// step walks has to satisfy. On a variable-length step the
    /// variable names one edge at a time inside this predicate, and
    /// the list of them everywhere else.
    pub filter: Option<Box<Expr>>,
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
    /// `VALUE { MATCH (p:Person) RETURN COUNT(*) }`, the value query
    /// expression (ISO 20.6, GQ18): a whole query written where one
    /// value belongs.
    ///
    /// The query inside is a query and not a block: it may chain with
    /// `NEXT`, join with a set operator and end with an ORDER BY and a
    /// LIMIT, and it has to end with a RETURN of exactly one column,
    /// because what stands here is one value.
    ValueQuery(Box<Query>),
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
